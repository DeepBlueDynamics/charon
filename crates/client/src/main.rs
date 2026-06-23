//! charon — the client. One image, two roles (spec 06, 07).
//!
//! ## Owner: Codex (Respective Bedbug). Build both roles against `charon-core`.
//!
//! - `charon consumer` — presents a plain OpenAI-compatible API to any agent
//!   (`/v1/chat/completions`, `/v1/models`, `/v1/estimate-cost`), resolves
//!   model→pinned provider, quotes, pays, runs the Noise IK handshake to the
//!   provider's pinned key, encrypts the body, relays via the gateway, decrypts
//!   and re-emits OpenAI SSE. MUST abort on a pinned-key mismatch (spec 07).
//! - `charon provider` — connects out to the gateway, `Register`s with the
//!   NUTS token + signed keybind + model cards, answers Noise handshakes as
//!   responder, enforces the envelope match, proxies to Ollama, streams back
//!   encrypted chunks + signed usage (spec 06).
//!
//! Both reach the gateway over one persistent WS. Deployed as the sidecar /
//! provider-host container (see `/spec/11-deployment.md`).

use clap::{Parser, Subcommand};
use axum::{
    extract::State,
    http::StatusCode,
    response::{
        sse::{Event, Sse},
        IntoResponse, Response,
    },
    routing::{get, post},
    Json, Router,
};
use charon_core::payment::{quote, Rate, DEFAULT_FLOOR_MSAT, DEFAULT_MARKUP_BPS};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{convert::Infallible, net::SocketAddr, sync::Arc, time::Duration};

#[derive(Parser, Debug)]
#[command(name = "charon", version, about = "Charon marketplace client")]
struct Cli {
    /// Gateway WebSocket URL.
    #[arg(long, env = "CHARON_GATEWAY", default_value = "wss://charon.nuts.services/ws")]
    gateway: String,
    /// NUTS ahp_ token for this principal.
    #[arg(long, env = "NUTS_AHP_TOKEN")]
    ahp_token: Option<String>,
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
        /// Ollama base URL.
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
        Role::Consumer { listen } => {
            tracing::info!(%listen, gateway = %cli.gateway, "charon consumer starting");
            run_consumer(listen, cli.gateway, cli.ahp_token).await
        }
        Role::Provider { ollama } => {
            tracing::info!(%ollama, gateway = %cli.gateway, "charon provider starting");
            run_provider(ollama, cli.gateway, cli.ahp_token).await
        }
    }
}

#[derive(Clone)]
struct ConsumerState {
    gateway: String,
    ahp_token: Option<String>,
    models: Arc<Vec<ModelConfig>>,
}

#[derive(Debug, Clone, Serialize)]
struct ModelConfig {
    name: String,
    provider: String,
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

#[derive(Debug, Deserialize)]
struct ChatCompletionRequest {
    model: String,
    #[serde(default)]
    messages: Vec<Value>,
    #[serde(default = "default_max_tokens")]
    max_tokens: u32,
    #[serde(default)]
    stream: bool,
    #[serde(flatten)]
    extra: Value,
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

async fn run_consumer(listen: String, gateway: String, ahp_token: Option<String>) -> anyhow::Result<()> {
    let state = ConsumerState {
        gateway,
        ahp_token,
        models: Arc::new(load_consumer_models()),
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

async fn run_provider(ollama: String, gateway: String, ahp_token: Option<String>) -> anyhow::Result<()> {
    let token_state = if ahp_token.is_some() { "configured" } else { "missing" };
    tracing::info!(%ollama, %gateway, ahp_token = token_state, "provider role is scaffolded; gateway session handling is next");
    std::future::pending::<()>().await;
    Ok(())
}

async fn list_models(State(state): State<ConsumerState>) -> Json<Value> {
    let data: Vec<_> = state
        .models
        .iter()
        .map(|model| {
            json!({
                "id": model.name,
                "object": "model",
                "owned_by": model.provider,
            })
        })
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

async fn chat_completions(
    State(state): State<ConsumerState>,
    Json(request): Json<ChatCompletionRequest>,
) -> Response {
    let Some(model) = state.models.iter().find(|model| model.name == request.model) else {
        return api_error(StatusCode::NOT_FOUND, "no_provider", format!("no pinned provider for model {}", request.model));
    };

    tracing::info!(
        gateway = %state.gateway,
        provider = %model.provider,
        model = %model.name,
        est_input_tokens = estimate_input_tokens(&request.messages),
        max_tokens = request.max_tokens,
        extra_fields = request.extra.as_object().map(|object| object.len()).unwrap_or_default(),
        has_ahp_token = state.ahp_token.is_some(),
        "consumer accepted chat request; gateway Noise relay is not yet wired in this slice"
    );

    if request.stream {
        let model_name = model.name.clone();
        let stream = async_stream::stream! {
            yield Ok::<_, Infallible>(
                Event::default().json_data(json!({
                    "id": format!("chatcmpl-{}", uuid::Uuid::new_v4()),
                    "object": "chat.completion.chunk",
                    "model": model_name,
                    "choices": [{
                        "index": 0,
                        "delta": {},
                        "finish_reason": "error"
                    }],
                    "error": {
                        "code": "gateway_not_wired",
                        "message": "gateway websocket, Noise relay, and payment execution are pending"
                    }
                })).expect("SSE JSON is serializable")
            );
            yield Ok::<_, Infallible>(Event::default().data("[DONE]"));
        };
        Sse::new(stream)
            .keep_alive(axum::response::sse::KeepAlive::new().interval(Duration::from_secs(15)))
            .into_response()
    } else {
        (
            StatusCode::NOT_IMPLEMENTED,
            Json(json!({
                "error": {
                    "code": "gateway_not_wired",
                    "message": "gateway websocket, Noise relay, and payment execution are pending"
                }
            })),
        )
            .into_response()
    }
}

fn api_error(status: StatusCode, code: &'static str, message: String) -> Response {
    (status, Json(ApiError { error: ApiErrorBody { code, message } })).into_response()
}

fn load_consumer_models() -> Vec<ModelConfig> {
    std::env::var("CHARON_MODELS")
        .ok()
        .map(|raw| {
            raw.split(',')
                .filter_map(|name| {
                    let name = name.trim();
                    if name.is_empty() {
                        return None;
                    }
                    Some(ModelConfig {
                        name: name.to_string(),
                        provider: std::env::var("CHARON_PROVIDER")
                            .unwrap_or_else(|_| "provider@charon.local".to_string()),
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
                .collect()
        })
        .unwrap_or_default()
}

fn estimate_input_tokens(messages: &[Value]) -> u32 {
    let bytes = serde_json::to_vec(messages).map(|body| body.len()).unwrap_or_default();
    ((bytes as u32).saturating_add(3) / 4).max(1)
}

fn default_max_tokens() -> u32 {
    1024
}
