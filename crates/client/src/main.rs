//! charon — the client. One image, two roles (spec 06, 07).

use anyhow::{anyhow, Context};
use axum::{
    extract::State,
    http::StatusCode,
    response::{
        sse::{Event, KeepAlive, Sse},
        IntoResponse, Response,
    },
    routing::{get, post},
    Json, Router,
};
use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
use charon_core::{
    crypto::{initiator_handshake, prologue, public_from_private, responder_handshake, SimplePinStore},
    payment::{quote, Rate, DEFAULT_FLOOR_MSAT, DEFAULT_MARKUP_BPS},
    wire::{Envelope, Payout},
    ErrorCode, Frame, Keybind, ModelCard, Payment, Usage,
};
use clap::{Parser, Subcommand};
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{collections::HashMap, convert::Infallible, net::SocketAddr, sync::Arc, time::Duration};
use tokio::sync::Mutex;
use tokio_tungstenite::{connect_async, tungstenite::Message as WsMessage};

#[derive(Parser, Debug)]
#[command(name = "charon", version, about = "Charon marketplace client")]
struct Cli {
    /// Gateway WebSocket URL.
    #[arg(long, env = "CHARON_GATEWAY", default_value = "wss://charon.nuts.services/ws")]
    gateway: String,
    /// NUTS ahp_ token for this principal.
    #[arg(long, env = "NUTS_AHP_TOKEN")]
    ahp_token: Option<String>,
    /// Cashu mint URL.
    #[arg(long, env = "CASHU_MINT_URL", default_value = "https://testnut.cashu.space")]
    cashu_mint: String,
    #[command(subcommand)]
    role: Role,
}

#[derive(Subcommand, Debug)]
enum Role {
    /// Run the consumer proxy (OpenAI-compatible local API).
    Consumer {
        /// Local OpenAI listener.
        #[arg(long, env = "CHARON_LISTEN", default_value = "0.0.0.0:8088")]
        listen: String,
    },
    /// Run the provider proxy (next to Ollama).
    Provider {
        /// Provider TOML config.
        #[arg(long, env = "CHARON_PROVIDER_CONFIG")]
        config: Option<String>,
        /// Ollama base URL override.
        #[arg(long, env = "OLLAMA_BASE_URL", default_value = "http://localhost:11434")]
        ollama: String,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();
    let cli = Cli::parse();
    match cli.role {
        Role::Consumer { listen } => run_consumer(listen, cli.gateway, cli.ahp_token, cli.cashu_mint).await,
        Role::Provider { config, ollama } => run_provider(config, ollama, cli.gateway, cli.ahp_token).await,
    }
}

#[derive(Clone)]
struct ConsumerState {
    gateway: String,
    ahp_token: Option<String>,
    principal: String,
    static_private: [u8; 32],
    keybind: Keybind,
    pins: Arc<Mutex<SimplePinStore>>,
    models: Arc<Vec<ModelConfig>>,
    cashu_mint: String,
}

#[derive(Debug, Clone, Serialize)]
struct ModelConfig {
    name: String,
    provider: String,
    provider_x25519_pub: [u8; 32],
    price_msat_per_mtok_in: u64,
    price_msat_per_mtok_out: u64,
}

#[derive(Debug, Deserialize)]
struct EstimateCostRequest {
    model: String,
    #[serde(default)]
    messages: Vec<Value>,
    #[serde(default = "default_max_tokens")]
    max_tokens: u32,
}

#[derive(Debug, Serialize)]
struct ApiError {
    error: ApiErrorBody,
}

#[derive(Debug, Serialize)]
struct ApiErrorBody {
    code: &'static str,
    message: String,
}

#[derive(Debug, Deserialize)]
struct ProviderConfig {
    #[serde(default)]
    gateway: ProviderGatewayConfig,
    identity: ProviderIdentityConfig,
    #[serde(default)]
    wallet: ProviderWalletConfig,
    #[serde(default)]
    ollama: ProviderOllamaConfig,
    #[serde(default)]
    models: Vec<ProviderModelConfig>,
}

#[derive(Debug, Default, Deserialize)]
struct ProviderGatewayConfig {
    url: Option<String>,
    provider_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ProviderIdentityConfig {
    x25519_key_file: String,
    keybind_file: String,
}

#[derive(Debug, Default, Deserialize)]
struct ProviderWalletConfig {
    rail: Option<String>,
    receive_address: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct ProviderOllamaConfig {
    base_url: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct ProviderModelConfig {
    name: String,
    #[serde(default = "default_backend_name")]
    backend: String,
    base_url: Option<String>,
    api_key_env: Option<String>,
    ollama_model: Option<String>,
    openai_model: Option<String>,
    litellm_model: Option<String>,
    #[serde(default = "default_context_length")]
    context_length: u32,
    #[serde(default)]
    price_msat_per_mtok_in: u64,
    #[serde(default)]
    price_msat_per_mtok_out: u64,
}

fn default_backend_name() -> String {
    "ollama".to_string()
}

impl ProviderModelConfig {
    fn rewritten_model_name(&self) -> String {
        if self.backend == "openai" {
            self.openai_model
                .clone()
                .or_else(|| self.litellm_model.clone())
                .unwrap_or_else(|| self.name.clone())
        } else {
            self.ollama_model
                .clone()
                .unwrap_or_else(|| self.name.clone())
        }
    }
}

struct ProviderRuntime {
    gateway: String,
    principal: String,
    ahp_token: String,
    static_private: [u8; 32],
    keybind: Keybind,
    payout: Payout,
    ollama_base_url: String,
    models: HashMap<String, ProviderModelConfig>,
}

struct ProviderSession {
    envelope: Envelope,
    session: Option<charon_core::crypto::Session>,
}

async fn run_consumer(listen: String, gateway: String, ahp_token: Option<String>, cashu_mint: String) -> anyhow::Result<()> {
    let static_private = load_consumer_private()?;
    let keybind = keybind_for_private(&static_private);
    let principal = consumer_principal(ahp_token.as_deref()).await?;
    let state = ConsumerState {
        gateway,
        ahp_token,
        principal,
        static_private,
        keybind,
        pins: Arc::new(Mutex::new(SimplePinStore::new())),
        models: Arc::new(load_consumer_models()?),
        cashu_mint,
    };

    let app = Router::new()
        .route("/v1/models", get(list_models))
        .route("/v1/estimate-cost", post(estimate_cost))
        .route("/v1/chat/completions", post(chat_completions))
        .with_state(state);

    let addr: SocketAddr = listen.parse()?;
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!(%addr, "charon consumer listening");
    axum::serve(listener, app).await?;
    Ok(())
}

async fn run_provider(
    config_path: Option<String>,
    ollama_override: String,
    gateway_override: String,
    ahp_token: Option<String>,
) -> anyhow::Result<()> {
    let runtime = load_provider_runtime(config_path, ollama_override, gateway_override, ahp_token)?;
    tracing::info!(
        gateway = %runtime.gateway,
        principal = %runtime.principal,
        ollama = %runtime.ollama_base_url,
        models = runtime.models.len(),
        provider_x25519_pub = %runtime.keybind.x25519_pub,
        "charon provider connecting"
    );

    verify_ollama_models(&runtime.ollama_base_url, &runtime.models).await;

    let (mut ws, _) = connect_async(gateway_url_with_token(&runtime.gateway, Some(&runtime.ahp_token))).await?;
    send_frame(
        &mut ws,
        &Frame::Register {
            ahp_token: runtime.ahp_token.clone(),
            keybind: runtime.keybind.clone(),
            models: runtime
                .models
                .values()
                .map(|model| ModelCard {
                    name: model.name.clone(),
                    backend: model.backend.clone(),
                    context_length: model.context_length,
                    price_msat_per_mtok_in: model.price_msat_per_mtok_in,
                    price_msat_per_mtok_out: model.price_msat_per_mtok_out,
                })
                .collect(),
            payout: runtime.payout.clone(),
        },
    )
    .await?;

    let mut sessions: HashMap<String, ProviderSession> = HashMap::new();
    loop {
        let frame = read_frame(&mut ws).await?;
        match frame {
            Frame::Ping => send_frame(&mut ws, &Frame::Pong).await?,
            Frame::Registered { provider } => tracing::info!(%provider, "provider registered"),
            Frame::Deliver { session_id, frame } => match *frame {
                Frame::Open { envelope, .. } => {
                    tracing::info!(%session_id, model = %envelope.model, "provider received open");
                    sessions.insert(session_id, ProviderSession { envelope, session: None });
                }
                Frame::Hs { blob, .. } => {
                    let Some(provider_session) = sessions.get_mut(&session_id) else {
                        tracing::warn!(%session_id, "handshake for unknown session");
                        continue;
                    };
                    let hs = decode_blob(&blob)?;
                    let p = prologue(
                        &provider_session.envelope.provider,
                        &provider_session.envelope.consumer,
                        &provider_session.envelope.model,
                        provider_session.envelope.max_tokens,
                        &session_id,
                    );
                    let responder = responder_handshake(&runtime.static_private, &p)?;
                    let (response, session) = responder.respond(&hs)?;
                    provider_session.session = Some(session);
                    send_frame(&mut ws, &Frame::Hs { session_id, blob: encode_blob(&response) }).await?;
                }
                Frame::Req { blob, .. } => {
                    handle_provider_req(&runtime, &mut ws, &mut sessions, session_id, blob).await?;
                }
                Frame::Cancel { .. } => {
                    sessions.remove(&session_id);
                }
                other => tracing::warn!(?other, "provider ignored delivered frame"),
            },
            Frame::Error { code, message, .. } => {
                tracing::warn!(?code, %message, "gateway error");
            }
            other => tracing::debug!(?other, "provider ignored frame"),
        }
    }
}

async fn list_models(State(state): State<ConsumerState>) -> Json<Value> {
    let data: Vec<_> = state
        .models
        .iter()
        .map(|model| json!({ "id": model.name, "object": "model", "owned_by": model.provider }))
        .collect();
    Json(json!({ "object": "list", "data": data }))
}

async fn estimate_cost(
    State(state): State<ConsumerState>,
    Json(request): Json<EstimateCostRequest>,
) -> Response {
    let Some(model) = state.models.iter().find(|model| model.name == request.model) else {
        return api_error(StatusCode::NOT_FOUND, "no_provider", format!("no pinned provider for model {}", request.model));
    };

    let est_input_tokens = estimate_input_tokens(&request.messages);
    let priced = quote(
        Rate {
            price_msat_per_mtok_in: model.price_msat_per_mtok_in,
            price_msat_per_mtok_out: model.price_msat_per_mtok_out,
        },
        est_input_tokens,
        request.max_tokens,
        DEFAULT_MARKUP_BPS,
        DEFAULT_FLOOR_MSAT,
    );

    Json(json!({
        "model": request.model,
        "provider": model.provider,
        "est_input_tokens": est_input_tokens,
        "max_tokens": request.max_tokens,
        "provider_msat": priced.provider_msat,
        "gateway_msat": priced.gateway_msat,
        "total_msat": priced.total_msat,
    }))
    .into_response()
}

async fn chat_completions(State(state): State<ConsumerState>, Json(body): Json<Value>) -> Response {
    let Some(model_name) = body.get("model").and_then(Value::as_str).map(str::to_string) else {
        return api_error(StatusCode::BAD_REQUEST, "invalid_request", "missing model".to_string());
    };
    let stream = body.get("stream").and_then(Value::as_bool).unwrap_or(false);

    if stream {
        let relay_state = state.clone();
        let relay_body = body.clone();
        let stream = async_stream::stream! {
            match consumer_relay(relay_state, relay_body).await {
                Ok(chunks) => {
                    let mut saw_done = false;
                    for chunk in chunks {
                        let data = String::from_utf8_lossy(&chunk);
                        for event in data.split("\n\n").filter(|event| !event.trim().is_empty()) {
                            let event = event.strip_prefix("data: ").unwrap_or(event).trim();
                            if event == "[DONE]" {
                                saw_done = true;
                            }
                            yield Ok::<_, Infallible>(Event::default().data(event.to_string()));
                        }
                    }
                    if !saw_done {
                        yield Ok::<_, Infallible>(Event::default().data("[DONE]"));
                    }
                }
                Err(err) => {
                    yield Ok::<_, Infallible>(
                        Event::default().json_data(json!({
                            "error": {
                                "code": "relay_failed",
                                "message": err.to_string()
                            }
                        })).expect("SSE error JSON is serializable")
                    );
                    yield Ok::<_, Infallible>(Event::default().data("[DONE]"));
                }
            }
        };
        Sse::new(stream)
            .keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
            .into_response()
    } else if state.models.iter().any(|model| model.name == model_name) {
        match consumer_relay(state, body).await {
            Ok(chunks) => {
                let bytes = chunks.into_iter().flatten().collect::<Vec<u8>>();
                (StatusCode::OK, [("content-type", "application/json")], bytes).into_response()
            }
            Err(err) => api_error(StatusCode::BAD_GATEWAY, "relay_failed", err.to_string()),
        }
    } else {
        api_error(StatusCode::NOT_FOUND, "no_provider", format!("no pinned provider for model {model_name}"))
    }
}

async fn mint_cashu_token(mint_url_str: &str, amount_msat: u64) -> anyhow::Result<String> {
    use std::str::FromStr;
    use cdk::wallet::Wallet;
    use cdk::Amount;
    use cdk::nuts::{CurrencyUnit, PaymentMethod, nut00::KnownMethod, nut00::Token};
    use cdk::mint_url::MintUrl;

    let total_sats = ((amount_msat + 999) / 1000).max(1);
    
    // Create ephemeral db
    let localstore = cdk_sqlite::wallet::memory::empty().await
        .map_err(|e| anyhow!("Failed to create empty in-memory wallet db: {:?}", e))?;
        
    // Generate a random seed
    let mut seed = [0u8; 64];
    for chunk in seed.chunks_mut(16) {
        chunk.copy_from_slice(uuid::Uuid::new_v4().as_bytes());
    }
    
    // Initialize wallet
    let wallet = Wallet::new(
        mint_url_str,
        CurrencyUnit::Sat,
        Arc::new(localstore),
        seed,
        None,
    ).map_err(|e| anyhow!("Failed to initialize wallet: {:?}", e))?;
    
    // Request mint quote
    let amount = Amount::from(total_sats);
    let quote = wallet.mint_quote(
        PaymentMethod::Known(KnownMethod::Bolt11),
        Some(amount),
        None,
        None,
    ).await.map_err(|e| anyhow!("Failed to get mint quote: {:?}", e))?;
    
    // wait and mint the quote
    let target = cdk::amount::SplitTarget::default();
    let proofs = wallet.wait_and_mint_quote(
        quote,
        target,
        None,
        Duration::from_secs(15),
    ).await.map_err(|e| anyhow!("Failed to mint quote: {:?}", e))?;
    
    // Serialize to Token
    let mint_url = MintUrl::from_str(mint_url_str)
        .map_err(|e| anyhow!("Invalid mint URL: {:?}", e))?;
    let token = Token::new(mint_url, proofs, None, CurrencyUnit::Sat);
    
    Ok(token.to_string())
}

async fn consumer_relay(state: ConsumerState, body: Value) -> anyhow::Result<Vec<Vec<u8>>> {
    let model_name = body
        .get("model")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("missing model"))?
        .to_string();
    let max_tokens = body
        .get("max_tokens")
        .and_then(Value::as_u64)
        .unwrap_or_else(|| default_max_tokens() as u64)
        .min(u32::MAX as u64) as u32;
    let messages = body.get("messages").and_then(Value::as_array).cloned().unwrap_or_default();
    let est_input_tokens = estimate_input_tokens(&messages);
    let model = state
        .models
        .iter()
        .find(|model| model.name == model_name)
        .ok_or_else(|| anyhow!("no pinned provider for model {model_name}"))?
        .clone();

    {
        let mut pins = state.pins.lock().await;
        pins.verify_or_pin(&model.provider, model.provider_x25519_pub)?;
    }

    let priced = quote(
        Rate {
            price_msat_per_mtok_in: model.price_msat_per_mtok_in,
            price_msat_per_mtok_out: model.price_msat_per_mtok_out,
        },
        est_input_tokens,
        max_tokens,
        DEFAULT_MARKUP_BPS,
        DEFAULT_FLOOR_MSAT,
    );
    let total_msat = priced.total_msat;
    tracing::info!(total_msat, mint = %state.cashu_mint, "minting Cashu payment for request");

    let cashu_token = mint_cashu_token(&state.cashu_mint, total_msat).await?;

    let session_id = uuid::Uuid::new_v4().to_string();
    let envelope = Envelope {
        provider: model.provider.clone(),
        consumer: state.principal.clone(),
        model: model.name.clone(),
        max_tokens,
        est_input_tokens,
        payment: Payment::Cashu { token: cashu_token },
        consumer_keybind: state.keybind.clone(),
    };

    let (mut ws, _) = connect_async(gateway_url_with_token(&state.gateway, state.ahp_token.as_deref())).await?;
    send_frame(&mut ws, &Frame::Open { session_id: session_id.clone(), envelope }).await?;

    loop {
        match read_frame(&mut ws).await? {
            Frame::OpenOk { session_id: got, .. } if got == session_id => break,
            Frame::Ping => send_frame(&mut ws, &Frame::Pong).await?,
            Frame::Error { code, message, .. } => return Err(anyhow!("gateway error {:?}: {}", code, message)),
            other => tracing::debug!(?other, "consumer ignored frame while awaiting open_ok"),
        }
    }

    let p = prologue(&model.provider, &state.principal, &model.name, max_tokens, &session_id);
    let mut handshake = initiator_handshake(&state.static_private, &model.provider_x25519_pub, &p)?;
    let first = handshake.first_message()?;
    send_frame(&mut ws, &Frame::Hs { session_id: session_id.clone(), blob: encode_blob(&first) }).await?;
    let response = loop {
        match read_frame(&mut ws).await? {
            Frame::Deliver { session_id: got, frame } if got == session_id => {
                if let Frame::Hs { blob, .. } = *frame {
                    break decode_blob(&blob)?;
                }
            }
            Frame::Ping => send_frame(&mut ws, &Frame::Pong).await?,
            Frame::Error { code, message, .. } => return Err(anyhow!("gateway error {:?}: {}", code, message)),
            other => tracing::debug!(?other, "consumer ignored frame while awaiting handshake"),
        }
    };
    let mut session = handshake.finish(&response)?;

    let plaintext = serde_json::to_vec(&body)?;
    let ciphertext = session.seal(&plaintext)?;
    send_frame(&mut ws, &Frame::Req { session_id: session_id.clone(), blob: encode_blob(&ciphertext) }).await?;

    let mut chunks = Vec::new();
    loop {
        match read_frame(&mut ws).await? {
            Frame::Deliver { session_id: got, frame } if got == session_id => match *frame {
                Frame::ResHead { blob, .. } => {
                    let _ = session.open(&decode_blob(&blob)?)?;
                }
                Frame::Res { blob, .. } => {
                    chunks.push(session.open(&decode_blob(&blob)?)?);
                }
                Frame::ResEnd { .. } => break,
                other => tracing::debug!(?other, "consumer ignored delivered frame"),
            },
            Frame::Settled { .. } => break,
            Frame::Ping => send_frame(&mut ws, &Frame::Pong).await?,
            Frame::Error { code, message, .. } => return Err(anyhow!("gateway error {:?}: {}", code, message)),
            other => tracing::debug!(?other, "consumer ignored frame while awaiting response"),
        }
    }
    Ok(chunks)
}

async fn handle_provider_req<S>(
    runtime: &ProviderRuntime,
    ws: &mut tokio_tungstenite::WebSocketStream<S>,
    sessions: &mut HashMap<String, ProviderSession>,
    session_id: String,
    blob: String,
) -> anyhow::Result<()>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let Some(provider_session) = sessions.get_mut(&session_id) else {
        return Ok(());
    };
    let Some(session) = provider_session.session.as_mut() else {
        return Ok(());
    };
    let request_bytes = session.open(&decode_blob(&blob)?)?;
    let mut request: Value = serde_json::from_slice(&request_bytes)?;

    let requested_model = request.get("model").and_then(Value::as_str).unwrap_or(&provider_session.envelope.model);
    let requested_max_tokens = request
        .get("max_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(provider_session.envelope.max_tokens as u64);
    if requested_model != provider_session.envelope.model || requested_max_tokens > provider_session.envelope.max_tokens as u64 {
        send_frame(
            ws,
            &Frame::Error {
                session_id: Some(session_id),
                code: ErrorCode::EnvelopeMismatch,
                message: "decrypted request does not match paid envelope".to_string(),
                http_status: Some(400),
            },
        )
        .await?;
        return Ok(());
    }

    let Some(model_config) = runtime.models.get(&provider_session.envelope.model) else {
        return Ok(());
    };
    request["model"] = Value::String(model_config.rewritten_model_name());

    let base_url = model_config.base_url.clone().unwrap_or_else(|| {
        if model_config.backend == "openai" {
            "http://localhost:4000/v1".to_string()
        } else {
            runtime.ollama_base_url.clone()
        }
    });

    let api_key = model_config.api_key_env.as_ref().and_then(|env_name| {
        std::env::var(env_name).ok()
    });

    let response_chunks = upstream_or_canned(&base_url, api_key, request).await;
    let head = session.seal(br#"{"status":200,"content_type":"text/event-stream"}"#)?;
    send_frame(ws, &Frame::ResHead { session_id: session_id.clone(), blob: encode_blob(&head) }).await?;
    let mut completion_tokens = 0;
    for chunk in response_chunks {
        completion_tokens += 1;
        let sealed = session.seal(chunk.as_bytes())?;
        send_frame(ws, &Frame::Res { session_id: session_id.clone(), blob: encode_blob(&sealed) }).await?;
    }
    send_frame(
        ws,
        &Frame::ResEnd {
            session_id: session_id.clone(),
            usage: Usage { prompt_tokens: provider_session.envelope.est_input_tokens, completion_tokens, sig: String::new() },
        },
    )
    .await?;
    sessions.remove(&session_id);
    Ok(())
}

async fn upstream_or_canned(base_url: &str, api_key: Option<String>, request: Value) -> Vec<String> {
    let client = reqwest::Client::new();
    let base = base_url.trim_end_matches('/');
    let url = if base.ends_with("/v1") {
        format!("{}/chat/completions", base)
    } else {
        format!("{}/v1/chat/completions", base)
    };

    let mut req_builder = client.post(url).json(&request);
    if let Some(key) = api_key {
        req_builder = req_builder.header("Authorization", format!("Bearer {key}"));
    }

    match req_builder.send().await {
        Ok(response) if response.status().is_success() => {
            if request.get("stream").and_then(Value::as_bool).unwrap_or(false) {
                let mut chunks = Vec::new();
                let mut stream = response.bytes_stream();
                while let Some(next) = stream.next().await {
                    match next {
                        Ok(bytes) => chunks.push(String::from_utf8_lossy(&bytes).to_string()),
                        Err(err) => {
                            tracing::warn!(error = ?err, "upstream stream failed; switching to canned tail");
                            chunks.push(canned_sse_chunk("upstream stream failed"));
                            break;
                        }
                    }
                }
                if !chunks.is_empty() {
                    return chunks;
                }
            } else if let Ok(bytes) = response.bytes().await {
                return vec![String::from_utf8_lossy(&bytes).to_string()];
            }
        }
        Ok(response) => {
            tracing::warn!(status = %response.status(), "upstream returned non-success; using canned response");
        }
        Err(err) => {
            tracing::warn!(error = ?err, "upstream unreachable; using canned response");
        }
    }

    if request.get("stream").and_then(Value::as_bool).unwrap_or(false) {
        vec![
            canned_sse_chunk("charon dev provider response"),
            "data: [DONE]\n\n".to_string(),
        ]
    } else {
        vec![json!({
            "id": format!("chatcmpl-{}", uuid::Uuid::new_v4()),
            "object": "chat.completion",
            "model": request.get("model").and_then(Value::as_str).unwrap_or("dev"),
            "choices": [{
                "index": 0,
                "message": { "role": "assistant", "content": "charon dev provider response" },
                "finish_reason": "stop"
            }],
            "usage": { "prompt_tokens": 1, "completion_tokens": 4, "total_tokens": 5 }
        }).to_string()]
    }
}

fn canned_sse_chunk(content: &str) -> String {
    format!(
        "data: {}\n\n",
        json!({
            "id": format!("chatcmpl-{}", uuid::Uuid::new_v4()),
            "object": "chat.completion.chunk",
            "choices": [{
                "index": 0,
                "delta": { "content": content },
                "finish_reason": null
            }]
        })
    )
}

fn api_error(status: StatusCode, code: &'static str, message: String) -> Response {
    (status, Json(ApiError { error: ApiErrorBody { code, message } })).into_response()
}

fn load_consumer_models() -> anyhow::Result<Vec<ModelConfig>> {
    let provider = std::env::var("CHARON_PROVIDER").unwrap_or_else(|_| charon_core::auth::NutsAuth::dev_principal().to_string());
    let provider_pub = parse_key32_env("CHARON_PROVIDER_X25519_PUB")?;
    Ok(std::env::var("CHARON_MODELS")
        .unwrap_or_else(|_| "qwen2.5-coder:32b".to_string())
        .split(',')
        .filter_map(|name| {
            let name = name.trim();
            if name.is_empty() {
                return None;
            }
            Some(ModelConfig {
                name: name.to_string(),
                provider: provider.clone(),
                provider_x25519_pub: provider_pub,
                price_msat_per_mtok_in: std::env::var("CHARON_PRICE_IN_MSAT_PER_MTOK")
                    .ok()
                    .and_then(|value| value.parse().ok())
                    .unwrap_or(200_000),
                price_msat_per_mtok_out: std::env::var("CHARON_PRICE_OUT_MSAT_PER_MTOK")
                    .ok()
                    .and_then(|value| value.parse().ok())
                    .unwrap_or(600_000),
            })
        })
        .collect())
}

/// Verify ollama-backed models actually exist in the local Ollama at startup
/// (spec 06). Warns — does not abort — so the provider still comes up, but the
/// operator immediately sees which advertised models can't really be served.
async fn verify_ollama_models(base_url: &str, models: &HashMap<String, ProviderModelConfig>) {
    let ollama: Vec<&ProviderModelConfig> = models.values().filter(|m| m.backend != "openai").collect();
    if ollama.is_empty() {
        return;
    }
    let url = format!("{}/api/tags", base_url.trim_end_matches('/'));
    let available: Option<Vec<String>> = async {
        let resp = reqwest::Client::new().get(&url).send().await.ok()?;
        if !resp.status().is_success() {
            return None;
        }
        let json: serde_json::Value = resp.json().await.ok()?;
        Some(
            json.get("models")?
                .as_array()?
                .iter()
                .filter_map(|m| m.get("name").and_then(|n| n.as_str()).map(str::to_string))
                .collect(),
        )
    }
    .await;
    match available {
        None => tracing::warn!(
            %url,
            "could not reach Ollama to verify models; advertised models will serve a fallback reply until Ollama is reachable"
        ),
        Some(tags) => {
            for m in ollama {
                let want = m.rewritten_model_name();
                if tags.iter().any(|t| t == &want) {
                    tracing::info!(model = %m.name, ollama = %want, "model available in Ollama");
                } else {
                    tracing::warn!(
                        model = %m.name, ollama = %want, available = ?tags,
                        "model NOT in Ollama — it will be advertised but serve a fallback until you `ollama pull` it"
                    );
                }
            }
        }
    }
}

fn load_provider_runtime(
    config_path: Option<String>,
    ollama_override: String,
    gateway_override: String,
    ahp_token: Option<String>,
) -> anyhow::Result<ProviderRuntime> {
    let config_path = config_path.unwrap_or_else(|| "charon-provider.toml".to_string());
    let config_text = std::fs::read_to_string(&config_path).with_context(|| format!("reading provider config {config_path}"))?;
    let config: ProviderConfig = toml::from_str(&config_text)?;
    let static_private = read_key32_file(&config.identity.x25519_key_file)?;
    let keybind_text = std::fs::read_to_string(&config.identity.keybind_file)?;
    let keybind: Keybind = serde_json::from_str(&keybind_text)?;
    Ok(ProviderRuntime {
        gateway: config.gateway.url.unwrap_or(gateway_override),
        principal: config
            .gateway
            .provider_id
            .unwrap_or_else(|| charon_core::auth::NutsAuth::dev_principal().to_string()),
        ahp_token: ahp_token
            .or_else(|| std::env::var("NUTS_AHP_TOKEN").ok())
            .unwrap_or_else(|| "ahp_dev".to_string()),
        static_private,
        keybind,
        payout: Payout {
            rail: config.wallet.rail.unwrap_or_else(|| "dev".to_string()),
            address: config.wallet.receive_address.unwrap_or_else(|| "dev".to_string()),
        },
        ollama_base_url: config.ollama.base_url.unwrap_or(ollama_override),
        models: config.models.into_iter().map(|model| (model.name.clone(), model)).collect(),
    })
}

async fn send_frame<S>(ws: &mut tokio_tungstenite::WebSocketStream<S>, frame: &Frame) -> anyhow::Result<()>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    ws.send(WsMessage::Text(serde_json::to_string(frame)?.into())).await?;
    Ok(())
}

async fn read_frame<S>(ws: &mut tokio_tungstenite::WebSocketStream<S>) -> anyhow::Result<Frame>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    while let Some(message) = ws.next().await {
        match message? {
            WsMessage::Text(text) => return Ok(serde_json::from_str(&text)?),
            WsMessage::Close(close) => return Err(anyhow!("websocket closed: {:?}", close)),
            _ => {}
        }
    }
    Err(anyhow!("websocket ended"))
}

fn gateway_url_with_token(gateway: &str, token: Option<&str>) -> String {
    let Some(token) = token else {
        return gateway.to_string();
    };
    let sep = if gateway.contains('?') { '&' } else { '?' };
    format!("{gateway}{sep}token={token}")
}

fn load_consumer_private() -> anyhow::Result<[u8; 32]> {
    match std::env::var("CHARON_CONSUMER_X25519_PRIV") {
        Ok(value) => parse_key32(&value),
        Err(_) => Ok([7; 32]),
    }
}

fn keybind_for_private(private: &[u8; 32]) -> Keybind {
    Keybind {
        x25519_pub: BASE64.encode(public_from_private(private)),
        sig: "dev-keybind".to_string(),
        not_after: 0,
    }
}

async fn consumer_principal(token: Option<&str>) -> anyhow::Result<String> {
    let Some(token) = token else {
        return Ok(charon_core::auth::NutsAuth::dev_principal().to_string());
    };
    let disable_auth = env_bool("DISABLE_AUTH");
    let auth_url = std::env::var("GNOSIS_AUTH_URL")
        .unwrap_or_else(|_| "https://auth.nuts.services".to_string());
    let auth = charon_core::auth::NutsAuth::new(auth_url, disable_auth);
    Ok(auth.validate(token).await?)
}

fn env_bool(name: &str) -> bool {
    std::env::var(name)
        .map(|value| matches!(value.as_str(), "1" | "true" | "TRUE" | "yes" | "YES"))
        .unwrap_or(false)
}

fn parse_key32_env(name: &str) -> anyhow::Result<[u8; 32]> {
    let value = std::env::var(name).with_context(|| format!("{name} must be set to the provider public key for dev pinning"))?;
    parse_key32(&value)
}

fn read_key32_file(path: &str) -> anyhow::Result<[u8; 32]> {
    parse_key32(std::fs::read_to_string(path)?.trim())
}

fn parse_key32(value: &str) -> anyhow::Result<[u8; 32]> {
    let trimmed = value.trim();
    let bytes = if let Some(hex) = trimmed.strip_prefix("hex:") {
        decode_hex(hex)?
    } else {
        BASE64.decode(trimmed)?
    };
    bytes.try_into().map_err(|_| anyhow!("key must decode to 32 bytes"))
}

fn decode_hex(hex: &str) -> anyhow::Result<Vec<u8>> {
    if hex.len() % 2 != 0 {
        return Err(anyhow!("hex key has odd length"));
    }
    (0..hex.len())
        .step_by(2)
        .map(|idx| u8::from_str_radix(&hex[idx..idx + 2], 16).map_err(Into::into))
        .collect()
}

fn encode_blob(bytes: &[u8]) -> String {
    BASE64.encode(bytes)
}

fn decode_blob(blob: &str) -> anyhow::Result<Vec<u8>> {
    Ok(BASE64.decode(blob)?)
}

fn estimate_input_tokens(messages: &[Value]) -> u32 {
    let bytes = serde_json::to_vec(messages).map(|body| body.len()).unwrap_or_default();
    ((bytes as u32).saturating_add(3) / 4).max(1)
}

fn default_max_tokens() -> u32 {
    1024
}

fn default_context_length() -> u32 {
    4096
}
