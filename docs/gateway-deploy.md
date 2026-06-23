# Gateway deployment

Deploy the blind gateway matching service (`charon-gateway`) locally or to Google Cloud Run.

## Local development deployment

To run the gateway locally for testing, you can disable token verification and run it on a loopback interface.

```bash
# Start the gateway on port 8080 with auth disabled
DISABLE_AUTH=true cargo run -p charon-gateway -- --bind 127.0.0.1:8080
#
# Response:
# info: charon-gateway starting
# info: Listening on address
```

### CLI reference

- `--bind <ADDR>`: Bind address (env: `BIND`). Defaults to `0.0.0.0:8080`. Honored by `PORT` env var.
- `--auth-url <URL>`: NUTS auth server base URL (env: `GNOSIS_AUTH_URL`). Defaults to `https://auth.nuts.services`.
- `--disable-auth`: Bypass NUTS token validation (env: `DISABLE_AUTH`). Defaults to `false`.
- `--markup-bps <VALUE>`: Gateway fee markup in basis points (env: `MARKUP_BPS`). Defaults to `1000` (+10%).
- `--floor-msat <VALUE>`: Gateway minimum fee in millisats (env: `FLOOR_MSAT`). Defaults to `21000` (21 sats).

## Google Cloud Run production deployment

In production, the gateway is deployed to Google Cloud Run in the `gnosis-459403` project. Because the gateway holds active WebSocket connections, specific flags must be supplied.

### Docker build and push
Submit the container build to Google Cloud Build.

```bash
# Configure gcloud project
gcloud config set project gnosis-459403

# Submit the build to Artifact Registry / Container Registry
gcloud builds submit --tag gcr.io/gnosis-459403/charon-gateway
#
# Response:
# SUCCESS: Image built and pushed
```

### Deploy to Cloud Run
Run the deploy command with session affinity and extended timeouts to keep WebSocket tunnels active.

```bash
# Deploy to Google Cloud Run in us-central1
gcloud run deploy charon-gateway \
  --image gcr.io/gnosis-459403/charon-gateway \
  --region us-central1 \
  --allow-unauthenticated \
  --port 8080 \
  --timeout 3600 \
  --min-instances 1 \
  --session-affinity \
  --concurrency 80 \
  --cpu 1 \
  --memory 512Mi \
  --set-env-vars "GNOSIS_AUTH_URL=https://auth.nuts.services,MARKUP_BPS=1000,FLOOR_MSAT=21000"
#
# Response:
# Service [charon-gateway] revision [charon-gateway-00001-abc] has been deployed.
# Service URL: https://charon-gateway-xxxx-uc.a.run.app
```

**Important:** You must deploy with `--min-instances 1` to prevent cold-starts which break WebSocket connection negotiations, and `--session-affinity` to ensure that both the consumer and provider for a given `session_id` are routed to the same gateway instance (since connection state is currently stored in-memory).

### Domain mapping
Front the gateway with a custom domain mapping under `nuts.services`.

```bash
# Create the domain mapping
gcloud beta run domain-mappings create \
  --service charon-gateway \
  --domain charon.nuts.services \
  --region us-central1
#
# Response:
# Domain mapping created. Add a CNAME pointing to ghs.googlehosted.com.
```

**Note:** After creating the mapping, configure your DNS provider with a CNAME record mapping `charon` to `ghs.googlehosted.com.`. Google Cloud Run will automatically provision and renew the TLS certificate.

## Environment variables reference

| Variable | Default | Description |
| :--- | :--- | :--- |
| `BIND` | `0.0.0.0:8080` | Bind IP and Port. |
| `PORT` | None | Injected by Cloud Run. Overrides the port portion of `BIND`. |
| `GNOSIS_AUTH_URL` | `https://auth.nuts.services` | The base URL of the NUTS identity provider. |
| `DISABLE_AUTH` | `false` | Set to `true` to disable signature and token checks. |
| `MARKUP_BPS` | `1000` | Gateway fee markup in basis points (100 basis points = 1%). |
| `FLOOR_MSAT` | `21000` | Gateway floor/minimum fee per session in millisats. |

## HTTP control plane endpoints

Alongside the WebSocket relay under `/ws`, the gateway serves a series of HTTP API endpoints for configuration, discovery, and pricing under `/v1/`. Accessing these requires a Bearer JWT token in the `Authorization` header, validated against the configured NUTS identity server (except in `DISABLE_AUTH` mode).

- `GET /v1/directory` — List all currently registered providers and their model cards.
- `GET /v1/providers/{principal}/reputation` — Retrieve reputation stats for a provider principal.
- `POST /v1/quote` — Estimate the pricing breakdown for a specific model query.
- `POST /v1/wallet/deposit` — Deposit funds into the gateway wallet (mocked/returns 501).
- `GET /v1/wallet/balance` — Read the gateway wallet balance (mocked/returns 501).
- `POST /v1/ratings` — Post provider performance feedback (mocked/returns 501).
