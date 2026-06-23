# 11 — Deployment

Charon ships as **two container images** branded Charon, plus the shared core
they are built from:

| Image | Role | Runs |
|-------|------|------|
| `charon` (the **client**) | `charon provider` next to Ollama; `charon consumer` as the OpenAI-compatible proxy | provider host / Nemesis8 session sidecar / consumer host |
| `charon-gateway` (the **service**) | blind relay, settlement, reputation (09) | Cloud Run |

One client image, two roles selected by subcommand. A single static binary is
also produced for host runs.

## Container topology

- **Gateway** — public `wss://` endpoint on Cloud Run. No inbound state beyond
  the durable ledger/reputation stores (09).
- **Provider proxy** — container on the provider host, next to Ollama
  (`http://localhost:11434`). Outbound WS only; no inbound port (06).
- **Consumer proxy** — container, **primary** deployment is a **sidecar** in the
  Nemesis8 session network namespace (`http://charon-proxy:8088`); host-binary
  mode (`host.docker.internal:8088`) is the alternative (07).

## Cloud Run (nuts.services pattern)

The gateway deploys exactly like the other nuts.services Rust services
(`nuts-proxy`, `sdrrand`, `shivvr`): a two-stage Rust → distroless image, pushed
to GCR, deployed to Cloud Run, fronted by a `*.nuts.services` domain mapping.

- **Project:** `gnosis-459403` · **Region:** `us-central1`
- **Service name:** `charon-gateway` · **Domain:** `charon.nuts.services`
- **Image:** `gcr.io/gnosis-459403/charon-gateway`
- **Listen:** `0.0.0.0:$PORT` (Cloud Run injects `PORT`; default `8080`).

### WebSocket-specific settings

The gateway holds long-lived WS connections, so it MUST be deployed with:

- `--timeout 3600` — max request/stream duration (default 300 s is too short).
- `--min-instances 1` — always warm; avoids cold-start handshake failures.
- `--session-affinity` — pin a consumer + provider of one `session_id` to the
  same instance (the in-flight table is node-local; 09 §scaling). A shared
  store removes this need later.
- `--concurrency 80`, `--cpu 1`, `--memory 512Mi` as a starting point.

### Deploy

```bash
gcloud config set project gnosis-459403
gcloud builds submit --tag gcr.io/gnosis-459403/charon-gateway
gcloud run deploy charon-gateway \
  --image gcr.io/gnosis-459403/charon-gateway \
  --region us-central1 --allow-unauthenticated \
  --port 8080 --timeout 3600 --min-instances 1 \
  --session-affinity --concurrency 80 --cpu 1 --memory 512Mi \
  --set-env-vars "GNOSIS_AUTH_URL=https://auth.nuts.services,\
NUTS_AUTH_JWKS_URL=https://auth.nuts.services/.well-known/jwks.json,\
MARKUP_BPS=1000,FLOOR_MSAT=21000,PAYMENT_RAILS=cashu,l402,balance"
```

### Domain mapping (one-time)

```bash
gcloud beta run domain-mappings create \
  --service charon-gateway --domain charon.nuts.services --region us-central1
# DNS: CNAME charon -> ghs.googlehosted.com.  (Google-managed TLS, auto-renew)
```

## Auth wiring (02)

The gateway validates NUTS tokens against `auth.nuts.services` (Python/FastAPI,
self-hostable as `deepbluedynamics/nuts-auth`):

| Token shape | Endpoint | Request | Principal |
|-------------|----------|---------|-----------|
| `ahp_…` | `POST /api/validate` | JSON `{"token":"ahp_…"}` | `subject` in `{valid,subject,actor,…}` |
| `ahp_…` (alt) | `POST /auth` | form `token=…` | JWT; `sub` claim |
| `eyJ…` JWT | `GET /api/verify` | `Authorization: Bearer` | `sub` claim (email) |

JWTs are RS256; the public key set is at `/.well-known/jwks.json`
(`kid=nuts-auth-key-1`). The gateway SHOULD prefer `POST /api/validate` for
`ahp_` daemon tokens (one round trip → principal). `DISABLE_AUTH=true` skips
validation for private/test deployments only and MUST NOT be set in production.

## Config / secrets

- Non-secret config via `--set-env-vars` (above).
- Secrets (mint keys, payout credentials) via Secret Manager, mounted to the
  Cloud Run service account with `roles/secretmanager.secretAccessor` — never
  baked into the image or passed as plain env vars.
