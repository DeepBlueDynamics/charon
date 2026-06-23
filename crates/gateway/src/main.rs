//! charon-gateway — the blind relay / matchmaker (spec 09).
//!
//! ## Owner: Antigravity (Obliged Coral). Build the relay against `charon-core`.
//!
//! Responsibilities (spec 09): authenticate both proxies by NUTS token (02),
//! track a connected-provider directory + model cards (03/08), reserve a route
//! from a consumer `Open` envelope to the named provider, relay `hs`/`req`/`res*`
//! frames between the two principals of a `session_id` treating bodies as opaque,
//! verify/collect payment and settle (05), aggregate signed ratings (08), and
//! rate-limit. It MUST NOT parse or log payload bodies (it has no session key).
//!
//! Listen on `0.0.0.0:$PORT` (Cloud Run injects `PORT`; default 8080). See
//! `/spec/11-deployment.md` for the Cloud Run / WebSocket settings.

use clap::Parser;

#[derive(Parser, Debug)]
#[command(name = "charon-gateway", version, about = "Charon blind relay")]
struct Args {
    /// Bind address. Cloud Run sets PORT; honor it.
    #[arg(long, env = "BIND", default_value = "0.0.0.0:8080")]
    bind: String,
    /// NUTS auth base URL.
    #[arg(long, env = "GNOSIS_AUTH_URL", default_value = "https://auth.nuts.services")]
    auth_url: String,
    /// Skip token validation — private/test deployments only (spec 02).
    #[arg(long, env = "DISABLE_AUTH", default_value_t = false)]
    disable_auth: bool,
    /// Gateway markup in basis points.
    #[arg(long, env = "MARKUP_BPS", default_value_t = charon_core::payment::DEFAULT_MARKUP_BPS)]
    markup_bps: u64,
    /// Gateway floor in msat.
    #[arg(long, env = "FLOOR_MSAT", default_value_t = charon_core::payment::DEFAULT_FLOOR_MSAT)]
    floor_msat: u64,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    // Honor Cloud Run's PORT over the default bind if present.
    let mut args = Args::parse();
    if let Ok(port) = std::env::var("PORT") {
        args.bind = format!("0.0.0.0:{port}");
    }

    tracing::info!(bind = %args.bind, auth = %args.auth_url, "charon-gateway starting");
    // TODO(Antigravity): accept WS connections, dispatch charon_core::Frame,
    // maintain the directory + in-flight session table, relay opaquely, settle.
    todo!("gateway relay loop — spec 09")
}
