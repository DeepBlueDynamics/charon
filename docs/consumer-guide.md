# Consumer guide

Run the consumer proxy to expose a standard OpenAI-compatible API to your coding agents. The proxy wraps vanilla LLM requests, handles automatic payment negotiations, and establishes end-to-end encrypted tunnels to providers.

## Run the consumer proxy

Start the consumer proxy using the `consumer` subcommand.

```bash
# Start the consumer proxy listening on port 8088
NUTS_AHP_TOKEN="ahp_my_consumer_token" \
CHARON_PROVIDER="you@provider-domain.com" \
CHARON_PROVIDER_X25519_PUB="5N4zY9Z5Sg3/2837482394782397489237498237489=" \
CHARON_MODELS="qwen2.5-coder:32b,llama3.3:70b" \
cargo run -p charon -- consumer \
  --listen 127.0.0.1:8088 \
  --gateway wss://charon.nuts.services/ws
#
# Response:
# info: charon consumer listening
```

### CLI flags and environment variables

| Environment Variable | CLI Flag | Default | Description |
| :--- | :--- | :--- | :--- |
| `CHARON_LISTEN` | `--listen` | `0.0.0.0:8088` | Local HTTP address to bind the OpenAI-compatible proxy. |
| `CHARON_GATEWAY` | `--gateway` | `wss://charon.nuts.services/ws` | Gateway WebSocket URL. |
| `NUTS_AHP_TOKEN` | `--ahp-token` | None | NUTS API authentication token. Required in non-dev mode. |
| `CHARON_CONSUMER_X25519_PRIV` | None | Base64 of `[7; 32]` | Your static X25519 private key for E2EE session handshake. |
| `CHARON_PROVIDER` | None | `dev@charon.local` | The target provider principal (NUTS email) to pin for request routing. |
| `CHARON_PROVIDER_X25519_PUB` | None | None | The pinned X25519 public key of the provider. **Important:** The proxy aborts if the provider's key mismatch. |
| `CHARON_MODELS` | None | `qwen2.5-coder:32b` | Comma-separated list of models the proxy routes to this provider. |
| `CHARON_PRICE_IN_MSAT_PER_MTOK` | None | `200000` | Mock input token price in millisats per million tokens. |
| `CHARON_PRICE_OUT_MSAT_PER_MTOK` | None | `600000` | Mock output token price in millisats per million tokens. |
| `GNOSIS_AUTH_URL` | None | `https://auth.nuts.services` | The base URL of the NUTS identity provider. |

## OpenAI-compatible endpoints

Once running, you can target the local consumer proxy using any standard OpenAI client or `curl`. The proxy supports the following endpoints:

- `GET /v1/models` — Returns the list of pinned models.
- `POST /v1/estimate-cost` — Estimates input and output token costs for a prompt.
- `POST /v1/chat/completions` — Executes chat completion (supports streaming and buffering).

## Nemesis8 sidecar wiring

In production, the consumer proxy is typically deployed as a sidecar container alongside a sealed coding agent container within the same Docker network namespace.

Define the Charon provider in your project's `.nemesis8.toml` file:

```toml
# .nemesis8.toml

[providers.charon]
type     = "openai"
base_url = "http://charon-proxy:8088/v1"   # sidecar; host mode: http://host.docker.internal:8088/v1
api_key  = "unused"                        # proxy authenticates upstream via NUTS, not this
models   = ["qwen2.5-coder:32b", "llama3.3:70b"]

[routing]
default = "charon"
```

**Security:** When running in sidecar mode, only the coding agent container should have access to the proxy listener. The proxy must not be exposed to the public internet.
