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
    #[arg(long, env = "CHARON_GATEWAY", default_value = "wss://gateway.nuts.services/ws")]
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
    /// Generate an X25519 identity key + keybind (for a provider or consumer).
    Keygen {
        /// Output directory (writes x25519.key and keybind.json).
        #[arg(long, default_value = ".")]
        out: String,
        /// Principal to bind (defaults to dev principal if omitted)
        #[arg(long, env = "CHARON_PRINCIPAL")]
        principal: Option<String>,
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
        Role::Keygen { out, principal } => run_keygen(out, principal),
    }
}

fn run_keygen(out: String, principal: Option<String>) -> anyhow::Result<()> {
    // Generate or load Nostr keypair
    let home_dir = std::env::var("HOME").unwrap_or_else(|_| "/opt/nemesis8".to_string());
    let charon_dir = std::path::PathBuf::from(&home_dir).join(".charon");
    std::fs::create_dir_all(&charon_dir)?;
    let nostr_key_path = charon_dir.join("nostr.key");

    let secp = secp256k1::Secp256k1::new();
    let (nostr_secret, xonly_pub_bytes) = if nostr_key_path.exists() {
        let hex_str = std::fs::read_to_string(&nostr_key_path)?;
        let trimmed = hex_str.trim();
        let bytes = hex::decode(trimmed).context("Invalid hex in ~/.charon/nostr.key")?;
        if bytes.len() != 32 {
            return Err(anyhow::anyhow!("~/.charon/nostr.key must be 32 bytes"));
        }
        let mut secret = [0u8; 32];
        secret.copy_from_slice(&bytes);
        let sk = secp256k1::SecretKey::from_slice(&secret)?;
        let kp = secp256k1::Keypair::from_secret_key(&secp, &sk);
        let (xpub, _) = kp.x_only_public_key();
        (secret, xpub.serialize())
    } else {
        let mut priv_bytes = [0u8; 32];
        loop {
            for chunk in priv_bytes.chunks_mut(16) {
                chunk.copy_from_slice(uuid::Uuid::new_v4().as_bytes());
            }
            if secp256k1::SecretKey::from_slice(&priv_bytes).is_ok() {
                break;
            }
        }
        let sk = secp256k1::SecretKey::from_slice(&priv_bytes)?;
        let kp = secp256k1::Keypair::from_secret_key(&secp, &sk);
        let (xpub, _) = kp.x_only_public_key();
        let hex_str = hex::encode(priv_bytes);
        
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            let mut options = std::fs::OpenOptions::new();
            options.create(true).write(true).truncate(true).mode(0o600);
            let mut file = options.open(&nostr_key_path)?;
            use std::io::Write;
            file.write_all(hex_str.as_bytes())?;
        }
        #[cfg(not(unix))]
        {
            std::fs::write(&nostr_key_path, hex_str.as_bytes())?;
        }
        println!("Generated new Nostr key at {}", nostr_key_path.display());
        (priv_bytes, xpub.serialize())
    };

    // Print npub as both bech32 and hex
    let hrp = bech32::Hrp::parse("npub")?;
    let bech32_npub = bech32::encode::<bech32::Bech32>(hrp, &xonly_pub_bytes)?;
    let hex_npub = hex::encode(xonly_pub_bytes);
    println!("Nostr Public Key (npub): {}", bech32_npub);
    println!("Nostr Public Key (hex):  {}", hex_npub);

    // X25519 secret: reuse the existing key if present so we don't rotate the
    // provider/consumer identity (that would break the consumer's pin and the
    // registered npub binding); otherwise generate a fresh 32-byte key.
    std::fs::create_dir_all(&out)?;
    let dir = std::path::Path::new(&out);
    let key_path = dir.join("x25519.key");
    let kb_path = dir.join("keybind.json");
    let priv_bytes: [u8; 32] = if key_path.exists() {
        let pb = read_key32_file(key_path.to_str().expect("utf8 path"))?;
        println!("Reusing existing X25519 key at {}", key_path.display());
        pb
    } else {
        let mut pb = [0u8; 32];
        for chunk in pb.chunks_mut(16) {
            chunk.copy_from_slice(uuid::Uuid::new_v4().as_bytes());
        }
        pb
    };

    let x25519_pub = public_from_private(&priv_bytes);
    let principal_str = principal.unwrap_or_else(|| charon_core::auth::NutsAuth::dev_principal().to_string());
    let keybind = charon_core::crypto::sign_keybind(x25519_pub, &principal_str, 0, nostr_secret);

    std::fs::write(&key_path, BASE64.encode(priv_bytes))?;
    std::fs::write(&kb_path, serde_json::to_string_pretty(&keybind)?)?;
    println!("wrote {}", key_path.display());
    println!("wrote {}", kb_path.display());
    println!("x25519_pub: {}\n", keybind.x25519_pub);
    println!("Point your provider config at these:");
    println!("  [identity]");
    println!("  x25519_key_file = \"{}\"", key_path.display());
    println!("  keybind_file    = \"{}\"", kb_path.display());
    Ok(())
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
    wallet: Arc<cdk::wallet::Wallet>,
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
    let principal = consumer_principal(ahp_token.as_deref()).await?;

    let home_dir = std::env::var("HOME").unwrap_or_else(|_| "/opt/nemesis8".to_string());
    let charon_dir = std::path::PathBuf::from(&home_dir).join(".charon");
    std::fs::create_dir_all(&charon_dir)?;

    let nostr_key_path = charon_dir.join("nostr.key");
    let nostr_secret = if nostr_key_path.exists() {
        let hex_str = std::fs::read_to_string(&nostr_key_path)?;
        let bytes = hex::decode(hex_str.trim())?;
        if bytes.len() != 32 {
            return Err(anyhow::anyhow!("Invalid nostr.key length"));
        }
        let mut secret = [0u8; 32];
        secret.copy_from_slice(&bytes);
        secret
    } else {
        let mut temp_secret = [0u8; 32];
        for chunk in temp_secret.chunks_mut(16) {
            chunk.copy_from_slice(uuid::Uuid::new_v4().as_bytes());
        }
        temp_secret
    };

    let x25519_pub = public_from_private(&static_private);
    let keybind = charon_core::crypto::sign_keybind(x25519_pub, &principal, 0, nostr_secret);
    let db_path = charon_dir.join("wallet.sqlite");
    
    let localstore = cdk_sqlite::WalletSqliteDatabase::new(db_path).await
        .map_err(|e| anyhow!("Failed to initialize SQLite database: {:?}", e))?;
        
    let seed_path = charon_dir.join("seed");
    let seed = if seed_path.exists() {
        let bytes = std::fs::read(&seed_path)?;
        if bytes.len() < 64 {
            return Err(anyhow!("Seed file is truncated"));
        }
        let mut seed = [0u8; 64];
        seed.copy_from_slice(&bytes[0..64]);
        seed
    } else {
        let mut seed = [0u8; 64];
        for chunk in seed.chunks_mut(16) {
            chunk.copy_from_slice(uuid::Uuid::new_v4().as_bytes());
        }
        std::fs::write(&seed_path, &seed)?;
        seed
    };
    
    let wallet = cdk::wallet::Wallet::new(
        &cashu_mint,
        cdk::nuts::CurrencyUnit::Sat,
        Arc::new(localstore),
        seed,
        None,
    ).map_err(|e| anyhow!("Failed to initialize wallet: {:?}", e))?;

    let state = ConsumerState {
        gateway,
        ahp_token,
        principal,
        static_private,
        keybind,
        pins: Arc::new(Mutex::new(SimplePinStore::new())),
        models: Arc::new(load_consumer_models()?),
        cashu_mint,
        wallet: Arc::new(wallet),
    };

    let app = Router::new()
        .route("/", get(consumer_home))
        .route("/v1/models", get(list_models))
        .route("/v1/estimate-cost", post(estimate_cost))
        .route("/v1/chat/completions", post(chat_completions))
        .route("/v1/fund", post(fund_wallet))
        .route("/v1/fund/{quote_id}", get(check_funding))
        .route("/v1/balance", get(get_balance))
        .with_state(state);

    let addr: SocketAddr = listen.parse()?;
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!(%addr, "charon consumer listening");
    axum::serve(listener, app).await?;
    Ok(())
}

async fn consumer_home() -> axum::response::Html<&'static str> {
    axum::response::Html(CONSUMER_HOME)
}

const CONSUMER_HOME: &str = r##"<!doctype html>
<html lang="en"><head>
<meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1">
<title>Charon Consumer</title>
<script src="https://cdnjs.cloudflare.com/ajax/libs/qrcodejs/1.0.0/qrcode.min.js"></script>
<style>
:root{--bg:#0a0a0b;--surface:#131316;--border:#1f1f24;--border2:#2a2a31;--text:#eceaef;--t2:#b5b3bb;--t3:#7c7a83;--orange:#f7931a;--green:#5fb27a}
*{box-sizing:border-box;margin:0;padding:0}
body{background:var(--bg);color:var(--text);font-family:system-ui,-apple-system,Segoe UI,sans-serif;line-height:1.5}
.wrap{max-width:680px;margin:0 auto;padding:34px 20px}
h1{font-size:1.25rem;font-weight:700;letter-spacing:.05em;display:flex;align-items:center;gap:10px}
.dot{width:9px;height:9px;border-radius:50%;background:var(--green);box-shadow:0 0 10px var(--green)}
.card{background:var(--surface);border:1px solid var(--border);border-radius:10px;padding:20px;margin-top:18px}
.card h2{font-size:.68rem;letter-spacing:.18em;text-transform:uppercase;color:var(--t3);margin-bottom:12px;font-family:ui-monospace,monospace}
.bal{font-size:2.1rem;font-weight:700;color:var(--orange);font-family:ui-monospace,monospace}
.bal small{font-size:.85rem;color:var(--t3);font-weight:400}
input,textarea,select{width:100%;background:#0f0f11;color:var(--text);border:1px solid var(--border2);border-radius:7px;padding:10px 12px;font-family:ui-monospace,monospace;font-size:13px}
.btn{background:var(--orange);color:#1a1206;border:0;border-radius:7px;padding:10px 18px;font-weight:600;cursor:pointer;font-family:ui-monospace,monospace;font-size:13px;white-space:nowrap}
.btn:hover{background:#ffa733}.btn:disabled{opacity:.5;cursor:default}
.row{display:flex;gap:10px;align-items:center;flex-wrap:wrap}
.qr{background:#fff;padding:14px;border-radius:10px;width:fit-content;margin:14px auto 6px}
.muted{color:var(--t3);font-size:12px;font-family:ui-monospace,monospace}
.out{white-space:pre-wrap;background:#0f0f11;border:1px solid var(--border);border-radius:7px;padding:12px;margin-top:10px;font-family:ui-monospace,monospace;font-size:12.5px;color:var(--t2);min-height:18px}
.hide{display:none}
</style></head>
<body><div class="wrap">
<h1><span class="dot"></span> CHARON &middot; consumer</h1>
<p class="muted" style="margin-top:6px">Local OpenAI-compatible API on this port. Pay-per-request with bitcoin ecash &mdash; nothing leaves this machine but the encrypted relay.</p>

<div class="card">
  <h2>Wallet balance</h2>
  <div class="bal"><span id="bal">&hellip;</span> <small>sat</small></div>
  <div class="row" style="margin-top:14px">
    <input id="amt" type="number" value="200" min="1" style="max-width:130px">
    <button class="btn" id="fundBtn">Add funds</button>
    <span class="muted" id="fundMsg"></span>
  </div>
  <div id="qrwrap" class="hide" style="text-align:center">
    <div class="qr" id="qr"></div>
    <div class="muted">Scan with your phone&#39;s Lightning wallet (or Coinbase &rarr; Send &rarr; Lightning)</div>
  </div>
</div>

<div class="card">
  <h2>Test a model</h2>
  <select id="model"></select>
  <textarea id="prompt" rows="2" style="margin-top:10px">Say hello in exactly three words</textarea>
  <div class="row" style="margin-top:10px"><button class="btn" id="sendBtn">Send</button><span class="muted" id="sendMsg"></span></div>
  <div class="out" id="out"></div>
</div>
</div>
<script>
const $=id=>document.getElementById(id);
async function j(u,o){const r=await fetch(u,o);return r.json();}
async function refreshBal(){try{const b=await j('/v1/balance');$('bal').textContent=(b.balance_sat!=null?b.balance_sat:'?');}catch(e){$('bal').textContent='?';}}
async function loadModels(){try{const m=await j('/v1/models');const s=$('model');s.innerHTML='';const d=m.data||[];d.forEach(x=>{const o=document.createElement('option');o.value=x.id;o.textContent=x.id;s.appendChild(o);});if(!d.length){s.innerHTML='<option value="">(no models &mdash; pin a provider)</option>';}}catch(e){}}
let pollTimer=null;
$('fundBtn').onclick=async()=>{
  const amt=parseInt($('amt').value||'0');if(!amt)return;
  $('fundBtn').disabled=true;$('fundMsg').textContent='generating invoice…';
  try{
    const f=await j('/v1/fund',{method:'POST',headers:{'content-type':'application/json'},body:JSON.stringify({amount_sat:amt})});
    if(!f.request)throw new Error(f.error||'no invoice');
    $('qr').innerHTML='';new QRCode($('qr'),{text:f.request.toUpperCase(),width:240,height:240,correctLevel:QRCode.CorrectLevel.M});
    $('qrwrap').classList.remove('hide');$('fundMsg').textContent='waiting for payment…';
    clearInterval(pollTimer);
    pollTimer=setInterval(async()=>{
      try{const c=await j('/v1/fund/'+f.quote_id);
        if(c.state==='Paid'||c.state==='Issued'){clearInterval(pollTimer);$('qrwrap').classList.add('hide');$('fundMsg').textContent='funded ✓';$('fundBtn').disabled=false;refreshBal();}
      }catch(e){}
    },3000);
  }catch(e){$('fundMsg').textContent='error: '+e.message;$('fundBtn').disabled=false;}
};
$('sendBtn').onclick=async()=>{
  if(!$('model').value){$('sendMsg').textContent='no model';return;}
  $('sendBtn').disabled=true;$('sendMsg').textContent='buying…';$('out').textContent='';
  try{
    const r=await fetch('/v1/chat/completions',{method:'POST',headers:{'content-type':'application/json'},body:JSON.stringify({model:$('model').value,messages:[{role:'user',content:$('prompt').value}]})});
    const d=await r.json();
    if(d.error){$('out').textContent='error: '+(typeof d.error==='string'?d.error:JSON.stringify(d.error));}
    else{$('out').textContent=(d.choices&&d.choices[0]&&d.choices[0].message&&d.choices[0].message.content)||JSON.stringify(d,null,2);}
    $('sendMsg').textContent='';refreshBal();
  }catch(e){$('sendMsg').textContent='error: '+e.message;}
  $('sendBtn').disabled=false;
};
refreshBal();loadModels();setInterval(refreshBal,8000);
</script></body></html>"##;

fn provider_npub_hex() -> Option<String> {
    let home = std::env::var("HOME").ok()?;
    let path = std::path::PathBuf::from(home).join(".charon").join("nostr.key");
    let hex_str = std::fs::read_to_string(&path).ok()?;
    let bytes = hex::decode(hex_str.trim()).ok()?;
    let sk = secp256k1::SecretKey::from_slice(&bytes).ok()?;
    let secp = secp256k1::Secp256k1::new();
    let kp = secp256k1::Keypair::from_secret_key(&secp, &sk);
    let (xpub, _) = kp.x_only_public_key();
    Some(hex::encode(xpub.serialize()))
}

#[derive(Clone)]
struct ProviderConsole {
    gateway: String,
    principal: String,
    npub: String,
    ahp_token: String,
    auth_url: String,
    ollama_base_url: String,
    config_path: Option<String>,
}

async fn provider_home() -> axum::response::Html<&'static str> {
    axum::response::Html(PROVIDER_HOME)
}

async fn provider_status(State(c): State<ProviderConsole>) -> Json<Value> {
    let models = c.config_path.as_ref()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|t| toml::from_str::<ProviderConfig>(&t).ok())
        .map(|cfg| cfg.models.into_iter().map(|m| {
            let om = m.ollama_model.clone().unwrap_or_else(|| m.name.clone());
            json!({"name": m.name, "ollama_model": om, "price_in": m.price_msat_per_mtok_in, "price_out": m.price_msat_per_mtok_out})
        }).collect::<Vec<_>>())
        .unwrap_or_default();
    Json(json!({
        "gateway": c.gateway, "principal": c.principal, "npub": c.npub,
        "ollama": c.ollama_base_url, "models": models
    }))
}

async fn provider_ollama_tags(State(c): State<ProviderConsole>) -> Json<Value> {
    let url = format!("{}/api/tags", c.ollama_base_url.trim_end_matches('/'));
    match reqwest::Client::new().get(&url).timeout(std::time::Duration::from_secs(5)).send().await {
        Ok(r) => match r.json::<Value>().await {
            Ok(v) => {
                let names: Vec<String> = v.get("models").and_then(|m| m.as_array())
                    .map(|arr| arr.iter().filter_map(|x| x.get("name").and_then(|n| n.as_str()).map(String::from)).collect())
                    .unwrap_or_default();
                Json(json!({"models": names}))
            }
            Err(_) => Json(json!({"models": [], "error": "parse"})),
        },
        Err(_) => Json(json!({"models": [], "error": "ollama unreachable"})),
    }
}

async fn provider_register_nostr(State(c): State<ProviderConsole>) -> Json<Value> {
    if c.npub.is_empty() { return Json(json!({"ok": false, "error": "no nostr key (run keygen)"})); }
    let url = format!("{}/api/identity/nostr", c.auth_url.trim_end_matches('/'));
    let body = json!({"token": c.ahp_token, "nostr_pubkey": c.npub});
    match reqwest::Client::new().post(&url).json(&body).timeout(std::time::Duration::from_secs(10)).send().await {
        Ok(r) => { let ok = r.status().is_success(); let status = r.status().as_u16(); let txt = r.text().await.unwrap_or_default(); Json(json!({"ok": ok, "status": status, "body": txt})) }
        Err(e) => Json(json!({"ok": false, "error": e.to_string()})),
    }
}

#[derive(serde::Deserialize)]
struct SaveModel { ollama_model: String, name: String, price_in: u64, price_out: u64 }

async fn provider_save_models(State(c): State<ProviderConsole>, Json(models): Json<Vec<SaveModel>>) -> Json<Value> {
    let Some(path) = c.config_path.clone() else { return Json(json!({"ok": false, "error": "no config path"})); };
    let cfg = match std::fs::read_to_string(&path).ok().and_then(|t| toml::from_str::<ProviderConfig>(&t).ok()) {
        Some(cfg) => cfg,
        None => return Json(json!({"ok": false, "error": "cannot read existing config"})),
    };
    let gw_url = cfg.gateway.url.unwrap_or_else(|| c.gateway.clone());
    let gw_id = cfg.gateway.provider_id.unwrap_or_else(|| c.principal.clone());
    let ollama = cfg.ollama.base_url.unwrap_or_else(|| c.ollama_base_url.clone());
    let mut out = String::new();
    out.push_str(&format!("[gateway]\nurl = \"{}\"\nprovider_id = \"{}\"\n\n", gw_url, gw_id));
    out.push_str(&format!("[identity]\nx25519_key_file = \"{}\"\nkeybind_file = \"{}\"\n\n", cfg.identity.x25519_key_file, cfg.identity.keybind_file));
    out.push_str(&format!("[ollama]\nbase_url = \"{}\"\n\n", ollama));
    for m in &models {
        let display = if m.name.trim().is_empty() { m.ollama_model.clone() } else { m.name.clone() };
        out.push_str(&format!("[[models]]\nname = \"{}\"\nollama_model = \"{}\"\ncontext_length = 8192\nprice_msat_per_mtok_in = {}\nprice_msat_per_mtok_out = {}\n\n", display, m.ollama_model, m.price_in, m.price_out));
    }
    match std::fs::write(&path, out) {
        Ok(_) => Json(json!({"ok": true, "count": models.len()})),
        Err(e) => Json(json!({"ok": false, "error": e.to_string()})),
    }
}

const PROVIDER_HOME: &str = r##"<!doctype html>
<html lang="en"><head>
<meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1">
<title>Charon Provider</title>
<style>
:root{--bg:#0a0a0b;--surface:#131316;--border:#1f1f24;--border2:#2a2a31;--text:#eceaef;--t2:#b5b3bb;--t3:#7c7a83;--orange:#f7931a;--green:#5fb27a}
*{box-sizing:border-box;margin:0;padding:0}
body{background:var(--bg);color:var(--text);font-family:system-ui,-apple-system,Segoe UI,sans-serif;line-height:1.5}
.wrap{max-width:720px;margin:0 auto;padding:34px 20px}
h1{font-size:1.25rem;font-weight:700;letter-spacing:.05em;display:flex;align-items:center;gap:10px}
.dot{width:9px;height:9px;border-radius:50%;background:var(--green);box-shadow:0 0 10px var(--green)}
.card{background:var(--surface);border:1px solid var(--border);border-radius:10px;padding:20px;margin-top:18px}
.card h2{font-size:.72rem;letter-spacing:.16em;text-transform:uppercase;color:var(--t3);margin-bottom:6px;font-family:ui-monospace,monospace}
.mono{font-family:ui-monospace,monospace}
.kv{display:flex;justify-content:space-between;gap:14px;font-family:ui-monospace,monospace;font-size:12.5px;padding:7px 0;border-bottom:1px solid var(--border)}
.kv:last-child{border-bottom:0}.kv span:last-child{color:var(--orange);overflow-wrap:anywhere;text-align:right}
input{background:#0f0f11;color:var(--text);border:1px solid var(--border2);border-radius:7px;padding:8px 10px;font-family:ui-monospace,monospace;font-size:12px}
.btn{background:var(--orange);color:#1a1206;border:0;border-radius:7px;padding:10px 18px;font-weight:600;cursor:pointer;font-family:ui-monospace,monospace;font-size:13px}
.btn:hover{background:#ffa733}.btn:disabled{opacity:.5;cursor:default}
.row{display:flex;gap:10px;align-items:center;flex-wrap:wrap}
.muted{color:var(--t3);font-size:12px;font-family:ui-monospace,monospace}
</style></head>
<body><div class="wrap">
<h1><span class="dot"></span> CHARON &middot; provider</h1>
<p class="muted" style="margin-top:6px">Sell your local models for bitcoin. This page is served by your running provider.</p>

<div class="card">
  <h2>Status</h2>
  <div class="kv"><span>gateway</span><span id="gw">&hellip;</span></div>
  <div class="kv"><span>provider id</span><span id="pid">&hellip;</span></div>
  <div class="kv"><span>nostr npub</span><span id="npub" style="font-size:10.5px">&hellip;</span></div>
</div>

<div class="card">
  <h2>1 &middot; Bind your Nostr key</h2>
  <p class="muted">One-time. Binds your npub to your identity so the gateway can verify it&#39;s really you (anti-MITM). Without this, registration is rejected.</p>
  <div class="row" style="margin-top:12px"><button class="btn" id="regBtn">Register npub</button><span class="muted" id="regMsg"></span></div>
</div>

<div class="card">
  <h2>2 &middot; Choose models to sell</h2>
  <p class="muted">Your local Ollama models. Check the ones to sell, set the advertised name (drop :cloud / :latest if you like), and price per million tokens (msat).</p>
  <div id="models" style="margin-top:12px"></div>
  <div class="row" style="margin-top:12px"><button class="btn" id="saveBtn">Save</button><span class="muted" id="saveMsg"></span></div>
  <p class="muted" style="margin-top:8px">After saving, restart the provider to apply.</p>
</div>
</div>
<script>
const $=id=>document.getElementById(id);
async function j(u,o){const r=await fetch(u,o);return r.json();}
let current={};
async function load(){
  try{
    const s=await j('/api/status');
    $('gw').textContent=s.gateway||'?';$('pid').textContent=s.principal||'?';$('npub').textContent=s.npub||'(no key — run keygen)';
    (s.models||[]).forEach(m=>{current[m.ollama_model||m.name]={disp:m.name,in:m.price_in,out:m.price_out};});
  }catch(e){}
  const tags=await j('/api/ollama-tags');
  const box=$('models');box.innerHTML='';
  const names=(tags.models||[]);
  if(!names.length){box.innerHTML='<div class="muted">No Ollama models found. Run <b>ollama serve</b> and <b>ollama pull &lt;model&gt;</b>, then reload.</div>';return;}
  names.forEach(n=>{
    const cur=current[n];
    const disp=cur?cur.disp:n.replace(/:(cloud|latest)$/,'');
    const row=document.createElement('div');row.className='row';row.style='margin-bottom:9px';
    row.innerHTML='<label style="display:flex;align-items:center;gap:8px;color:var(--text);font-size:12.5px;min-width:170px"><input type="checkbox" class="msel" data-n="'+n+'" '+(cur?'checked':'')+'> '+n+'</label>'
      +'<input type="text" class="dname" data-n="'+n+'" value="'+disp+'" title="advertised name" style="max-width:150px">'
      +'<input type="number" class="pin" data-n="'+n+'" value="'+(cur?cur.in:200000)+'" title="msat / Mtok in" style="max-width:110px">'
      +'<input type="number" class="pout" data-n="'+n+'" value="'+(cur?cur.out:600000)+'" title="msat / Mtok out" style="max-width:110px">';
    box.appendChild(row);
  });
}
$('regBtn').onclick=async()=>{
  $('regBtn').disabled=true;$('regMsg').textContent='registering…';
  try{const r=await j('/api/register-nostr',{method:'POST'});$('regMsg').textContent=r.ok?'bound ✓':('error: '+(r.error||r.status||''));}
  catch(e){$('regMsg').textContent='error: '+e.message;}
  $('regBtn').disabled=false;
};
$('saveBtn').onclick=async()=>{
  const models=[];
  document.querySelectorAll('.msel').forEach(c=>{if(c.checked){const n=c.dataset.n;const dn=(document.querySelector('.dname[data-n="'+n+'"]').value||'').trim()||n;const pin=document.querySelector('.pin[data-n="'+n+'"]').value;const pout=document.querySelector('.pout[data-n="'+n+'"]').value;models.push({ollama_model:n,name:dn,price_in:parseInt(pin)||200000,price_out:parseInt(pout)||600000});}});
  if(!models.length){$('saveMsg').textContent='select at least one model';return;}
  $('saveBtn').disabled=true;$('saveMsg').textContent='saving…';
  try{const r=await j('/api/save-models',{method:'POST',headers:{'content-type':'application/json'},body:JSON.stringify(models)});
    $('saveMsg').textContent=r.ok?('saved '+r.count+' model(s) — restart the provider to apply'):('error: '+(r.error||''));}
  catch(e){$('saveMsg').textContent='error: '+e.message;}
  $('saveBtn').disabled=false;
};
load();
</script></body></html>"##;

async fn run_provider(
    config_path: Option<String>,
    ollama_override: String,
    gateway_override: String,
    ahp_token: Option<String>,
) -> anyhow::Result<()> {
    let cfg_path = config_path.clone();
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

    // Local provider console (status, model picker, one-click Nostr register).
    {
        let console = ProviderConsole {
            gateway: runtime.gateway.clone(),
            principal: runtime.principal.clone(),
            npub: provider_npub_hex().unwrap_or_default(),
            ahp_token: runtime.ahp_token.clone(),
            auth_url: std::env::var("GNOSIS_AUTH_URL").unwrap_or_else(|_| "https://auth.nuts.services".to_string()),
            ollama_base_url: runtime.ollama_base_url.clone(),
            config_path: cfg_path,
        };
        let port: u16 = std::env::var("CHARON_PROVIDER_CONSOLE").ok().and_then(|v| v.parse().ok()).unwrap_or(8091);
        tokio::spawn(async move {
            let app = Router::new()
                .route("/", get(provider_home))
                .route("/api/status", get(provider_status))
                .route("/api/ollama-tags", get(provider_ollama_tags))
                .route("/api/register-nostr", post(provider_register_nostr))
                .route("/api/save-models", post(provider_save_models))
                .with_state(console);
            match tokio::net::TcpListener::bind(("127.0.0.1", port)).await {
                Ok(listener) => { tracing::info!(port, "provider console listening"); let _ = axum::serve(listener, app).await; }
                Err(e) => tracing::warn!(error = ?e, "provider console failed to bind"),
            }
        });
    }

    // Auto-reconnect with backoff. Any gateway eventually drops a long-lived WS
    // (Cloud Run's 60-min request cap, restarts, transient network). Reconnecting
    // and re-registering keeps the provider live instead of leaving a zombie
    // socket + a stale directory entry (which makes consumers hit ProviderGone).
    let mut backoff_secs = 1u64;
    loop {
        let started = std::time::Instant::now();
        if let Err(e) = provider_session(&runtime).await {
            tracing::error!(error = ?e, "provider session ended; reconnecting");
        } else {
            tracing::warn!("gateway connection closed; reconnecting");
        }
        // A connection that lasted a while is healthy: reset backoff so a normal
        // hourly reconnect is immediate, while a gateway that's truly down backs off.
        if started.elapsed() > std::time::Duration::from_secs(30) {
            backoff_secs = 1;
        }
        tracing::info!(backoff_secs, "reconnecting to gateway");
        tokio::time::sleep(std::time::Duration::from_secs(backoff_secs)).await;
        backoff_secs = backoff_secs.saturating_mul(2).min(30);
    }
}

async fn provider_session(runtime: &ProviderRuntime) -> anyhow::Result<()> {
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
                    
                    let disable_keybind_verify = std::env::var("DISABLE_KEYBIND_VERIFY")
                        .map(|v| v == "true")
                        .unwrap_or(false);
                    if !disable_keybind_verify {
                        let auth_url = std::env::var("GNOSIS_AUTH_URL")
                            .unwrap_or_else(|_| "https://auth.nuts.services".to_string());
                        match charon_core::auth::get_principal_nostr_pubkey(&auth_url, &envelope.consumer).await {
                            Ok(pubkey) => {
                                if !charon_core::crypto::verify_keybind(&envelope.consumer_keybind, &envelope.consumer, pubkey) {
                                    tracing::error!(%session_id, consumer = %envelope.consumer, "Consumer keybind verification failed");
                                    let err_frame = Frame::Error {
                                        session_id: Some(session_id.clone()),
                                        code: ErrorCode::KeyUnverified,
                                        message: "Consumer keybind verification failed".into(),
                                        http_status: Some(401),
                                    };
                                    let _ = send_frame(&mut ws, &err_frame).await;
                                    continue;
                                }
                            }
                            Err(e) => {
                                tracing::error!(%session_id, consumer = %envelope.consumer, "Failed to fetch consumer Nostr pubkey: {:?}", e);
                                let err_frame = Frame::Error {
                                    session_id: Some(session_id.clone()),
                                    code: ErrorCode::KeyUnverified,
                                    message: "Failed to fetch consumer Nostr pubkey".into(),
                                    http_status: Some(401),
                                };
                                let _ = send_frame(&mut ws, &err_frame).await;
                                continue;
                            }
                        }
                    }

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
                    handle_provider_req(runtime, &mut ws, &mut sessions, session_id, blob).await?;
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
    let Some(model) = state.models.iter().find(|m| m.name == model_name) else {
        return api_error(StatusCode::NOT_FOUND, "no_provider", format!("no pinned provider for model {model_name}"));
    };

    let max_tokens = body
        .get("max_tokens")
        .and_then(Value::as_u64)
        .unwrap_or_else(|| default_max_tokens() as u64)
        .min(u32::MAX as u64) as u32;
    let messages = body.get("messages").and_then(Value::as_array).cloned().unwrap_or_default();
    let est_input_tokens = estimate_input_tokens(&messages);

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
    let needed_sat = (total_msat + 999) / 1000;

    let balance = match state.wallet.total_balance().await {
        Ok(b) => u64::from(b),
        Err(e) => return api_error(StatusCode::INTERNAL_SERVER_ERROR, "wallet_error", format!("Failed to get balance: {:?}", e)),
    };

    if balance < needed_sat {
        return (
            StatusCode::PAYMENT_REQUIRED,
            Json(json!({
                "error": "payment_required",
                "balance_sat": balance,
                "needed_sat": needed_sat
            })),
        ).into_response();
    }

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
                    if let Some(pay_err) = err.downcast_ref::<PaymentRequiredError>() {
                        yield Ok::<_, Infallible>(
                            Event::default().json_data(json!({
                                "error": "payment_required",
                                "balance_sat": pay_err.balance_sat,
                                "needed_sat": pay_err.needed_sat
                            })).expect("SSE error JSON is serializable")
                        );
                    } else {
                        yield Ok::<_, Infallible>(
                            Event::default().json_data(json!({
                                "error": {
                                    "code": "relay_failed",
                                    "message": err.to_string()
                                }
                            })).expect("SSE error JSON is serializable")
                        );
                    }
                    yield Ok::<_, Infallible>(Event::default().data("[DONE]"));
                }
            }
        };
        Sse::new(stream)
            .keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
            .into_response()
    } else {
        match consumer_relay(state, body).await {
            Ok(chunks) => {
                let bytes = chunks.into_iter().flatten().collect::<Vec<u8>>();
                (StatusCode::OK, [("content-type", "application/json")], bytes).into_response()
            }
            Err(err) => {
                if let Some(pay_err) = err.downcast_ref::<PaymentRequiredError>() {
                    (
                        StatusCode::PAYMENT_REQUIRED,
                        Json(json!({
                            "error": "payment_required",
                            "balance_sat": pay_err.balance_sat,
                            "needed_sat": pay_err.needed_sat
                        })),
                    ).into_response()
                } else {
                    api_error(StatusCode::BAD_GATEWAY, "relay_failed", err.to_string())
                }
            }
        }
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PaymentRequiredError {
    pub balance_sat: u64,
    pub needed_sat: u64,
}

impl std::fmt::Display for PaymentRequiredError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "payment_required: balance={} needed={}", self.balance_sat, self.needed_sat)
    }
}

impl std::error::Error for PaymentRequiredError {}

async fn spend_cashu_token(wallet: &cdk::wallet::Wallet, amount_msat: u64) -> anyhow::Result<String> {
    use cdk::Amount;
    use cdk::wallet::SendOptions;

    let needed_sat = (amount_msat + 999) / 1000;
    
    let balance = wallet.total_balance().await.map_err(|e| anyhow!("Failed to get balance: {:?}", e))?;
    let balance_sat: u64 = balance.into();
    if balance_sat < needed_sat {
        return Err(anyhow::anyhow!(PaymentRequiredError { balance_sat, needed_sat }));
    }
    
    let prepared = wallet.prepare_send(
        Amount::from(needed_sat),
        SendOptions::default(),
    ).await.map_err(|e| {
        tracing::warn!("Failed to prepare send: {:?}", e);
        anyhow::anyhow!(PaymentRequiredError { balance_sat, needed_sat })
    })?;
    
    let token = prepared.confirm(None).await
        .map_err(|e| anyhow!("Failed to confirm send: {:?}", e))?;
    Ok(token.to_string())
}

#[derive(Debug, serde::Deserialize)]
struct FundRequest {
    amount_sat: u64,
}

async fn fund_wallet(
    State(state): State<ConsumerState>,
    Json(request): Json<FundRequest>,
) -> Response {
    let amount = cdk::Amount::from(request.amount_sat);
    match state.wallet.mint_quote(
        cdk::nuts::PaymentMethod::Known(cdk::nuts::nut00::KnownMethod::Bolt11),
        Some(amount),
        None,
        None,
    ).await {
        Ok(quote) => {
            (
                StatusCode::OK,
                Json(json!({
                    "quote_id": quote.id,
                    "request": quote.request,
                    "amount_sat": request.amount_sat
                })),
            ).into_response()
        }
        Err(e) => {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({
                    "error": format!("Failed to create mint quote: {:?}", e)
                })),
            ).into_response()
        }
    }
}

async fn check_funding(
    State(state): State<ConsumerState>,
    axum::extract::Path(quote_id): axum::extract::Path<String>,
) -> Response {
    match state.wallet.check_mint_quote(&quote_id).await {
        Ok(quote) => {
            if quote.state == cdk::nuts::MintQuoteState::Paid {
                if let Err(e) = state.wallet.mint(&quote_id, cdk::amount::SplitTarget::default(), None).await {
                    return (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(json!({
                            "error": format!("Failed to mint quote: {:?}", e)
                        })),
                    ).into_response();
                }
            }
            
            // Get updated balance
            let balance = match state.wallet.total_balance().await {
                Ok(b) => u64::from(b),
                Err(e) => {
                    return (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(json!({
                            "error": format!("Failed to fetch balance: {:?}", e)
                        })),
                    ).into_response();
                }
            };
            
            (
                StatusCode::OK,
                Json(json!({
                    "state": format!("{:?}", quote.state),
                    "balance_sat": balance
                })),
            ).into_response()
        }
        Err(e) => {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({
                    "error": format!("Failed to check mint quote status: {:?}", e)
                })),
            ).into_response()
        }
    }
}

async fn get_balance(
    State(state): State<ConsumerState>,
) -> Response {
    match state.wallet.total_balance().await {
        Ok(balance) => {
            let balance_sat = u64::from(balance);
            let balance_msat = balance_sat * 1000;
            (
                StatusCode::OK,
                Json(json!({
                    "balance_sat": balance_sat,
                    "balance_msat": balance_msat
                })),
            ).into_response()
        }
        Err(e) => {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({
                    "error": format!("Failed to fetch balance: {:?}", e)
                })),
            ).into_response()
        }
    }
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
    tracing::info!(total_msat, mint = %state.cashu_mint, "spending Cashu payment for request");

    let cashu_token = spend_cashu_token(&state.wallet, total_msat).await?;

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
