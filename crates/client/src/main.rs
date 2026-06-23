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
            todo!("consumer proxy — spec 07")
        }
        Role::Provider { ollama } => {
            tracing::info!(%ollama, gateway = %cli.gateway, "charon provider starting");
            todo!("provider proxy — spec 06")
        }
    }
}
