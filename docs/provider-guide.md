# Provider guide

Set up and run a Charon provider node next to your local Ollama engine to sell model inference capacity.

## Prerequisites

You need the `charon` client binary, access to a running Ollama server, and a NUTS `ahp_` token.

- **Ollama:** Install Ollama and pull the models you want to host (e.g. `ollama pull qwen2.5-coder:32b`).
- **NUTS token:** Obtain an `ahp_` API token by logging into the NUTS dashboard at `https://auth.nuts.services`.

## Identity and key binding

To protect consumer prompts, the marketplace enforces end-to-end encryption. You must generate an X25519 keypair and bind it to your NUTS identity.

1. **X25519 keypair:** Create a 32-byte private key file. You can encode the key as Base64 or prefix it with `hex:` for hexadecimal.
2. **Keybind file:** Create a JSON file containing your public key and its NUTS signature. **Important:** The consumer proxy will verify this signature against your identity. If it is invalid, connections are aborted.

```json
// keybind.json
{
  "x25519_pub": "5N4zY9Z5Sg3/2837482394782397489237498237489=",
  "sig": "signature_from_nuts_auth_services",
  "not_after": 1729168400
}
```

## Configuration file

The provider daemon is configured via a TOML file (default is `charon-provider.toml`).

```toml
# charon-provider.toml

[gateway]
# The WebSocket URL of the gateway
url = "wss://charon.nuts.services/ws"
# Your registered NUTS principal (identity email)
provider_id = "you@example.com"

[identity]
# Paths to your cryptographic identity files
x25519_key_file = "x25519.key"
keybind_file = "keybind.json"

[wallet]
# The payment rail to receive fees (e.g. "cashu", "bolt12", or "dev")
rail = "bolt12"
# Payout destination (Lightning Offer or Cashu mint P2PK key)
receive_address = "lno1..."

[ollama]
# URL of your local Ollama API
base_url = "http://localhost:11434"

# Define each model you wish to offer for sale
[[models]]
# The public name consumers request
name = "qwen2.5-coder:32b"
# The local Ollama model name (if different)
ollama_model = "qwen2.5-coder:32b"
# Max tokens allowed for a single session
context_length = 4096
# Price per 1M input tokens in millisats
price_msat_per_mtok_in = 200000
# Price per 1M output tokens in millisats
price_msat_per_mtok_out = 600000
```

### Configuration fields reference

| Section | Field | Type | Required | Description |
| :--- | :--- | :--- | :--- | :--- |
| `[gateway]` | `url` | `String` | No | Gateway WebSocket URL. Overridden by CLI or env. |
| `[gateway]` | `provider_id` | `String` | No | Your NUTS identity principal. Defaults to `dev@charon.local`. |
| `[identity]` | `x25519_key_file` | `String` | Yes | Path to your static X25519 private key. |
| `[identity]` | `keybind_file` | `String` | Yes | Path to the NUTS-signed keybind JSON. |
| `[wallet]` | `rail` | `String` | No | Payout rail (e.g., `bolt12`, `cashu`). Defaults to `dev`. |
| `[wallet]` | `receive_address` | `String` | No | Destination for payouts. Defaults to `dev`. |
| `[ollama]` | `base_url` | `String` | No | Ollama URL. Overridden by CLI or env. |
| `[[models]]` | `name` | `String` | Yes | Model identifier advertised to the gateway. |
| `[[models]]` | `ollama_model` | `String` | No | Actual local Ollama model identifier. Defaults to `name`. |
| `[[models]]` | `context_length` | `u32` | No | Allowed context length. Defaults to `4096`. |
| `[[models]]` | `price_msat_per_mtok_in` | `u64` | No | Cost per 1,000,000 input tokens. Defaults to `0`. |
| `[[models]]` | `price_msat_per_mtok_out` | `u64` | No | Cost per 1,000,000 output tokens. Defaults to `0`. |

## CLI flags and environment variables

You can override configuration settings using CLI arguments or environment variables on startup.

```bash
# Start the provider daemon with overrides
NUTS_AHP_TOKEN="ahp_my_provider_token" \
OLLAMA_BASE_URL="http://localhost:11434" \
cargo run -p charon -- provider \
  --config charon-provider.toml \
  --gateway wss://charon.nuts.services/ws
#
# Response:
# info: charon provider connecting
# info: provider registered
```

### CLI reference

- `--config <PATH>`: Path to provider configuration file (env: `CHARON_PROVIDER_CONFIG`). Defaults to `charon-provider.toml`.
- `--ollama <URL>`: Ollama base URL override (env: `OLLAMA_BASE_URL`). Defaults to `http://localhost:11434`.
- `--gateway <URL>`: Gateway WebSocket URL (env: `CHARON_GATEWAY`). Defaults to `wss://charon.nuts.services/ws`.
- `--ahp-token <TOKEN>`: NUTS API authentication token (env: `NUTS_AHP_TOKEN`). Required in non-dev mode.
