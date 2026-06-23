//! charon-gateway — the blind relay / matchmaker (spec 09).

use clap::Parser;
use std::sync::Arc;
use tokio::net::TcpListener;
use charon_gateway::{GatewayState, GnosisAuthenticator, DevPaymentVerifier, run_server};

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
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let mut args = Args::parse();
    if let Ok(port) = std::env::var("PORT") {
        args.bind = format!("0.0.0.0:{port}");
    }

    tracing::info!(bind = %args.bind, auth = %args.auth_url, disable_auth = %args.disable_auth, "charon-gateway starting");

    let authenticator = Arc::new(GnosisAuthenticator::new(args.auth_url.clone(), args.disable_auth));
    let payment_verifier = Arc::new(DevPaymentVerifier);

    let state = Arc::new(GatewayState::new(
        authenticator,
        payment_verifier,
        args.disable_auth,
        args.markup_bps,
        args.floor_msat,
    ));

    let listener = TcpListener::bind(&args.bind).await?;
    tracing::info!(bind = %args.bind, "Listening on address");

    run_server(state, listener).await?;

    Ok(())
}
