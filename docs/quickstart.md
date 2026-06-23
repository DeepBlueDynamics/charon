# Quickstart

Run the entire Charon marketplace locally in development mode. In this setup, token authentication is disabled, and all components run on localhost.

## Prerequisites

You need the Rust toolchain installed. Ollama must also be running locally.

```bash
# Check if Ollama is running
curl http://localhost:11434/
#
# Response:
# Ollama is running
```

## Step 1: Start the gateway

The gateway coordinates connections, quotes fees, and routes encrypted traffic. Run it with `DISABLE_AUTH=true` to bypass NUTS token validation.

```bash
# Run the gateway on port 8080 in dev mode
DISABLE_AUTH=true cargo run -p charon-gateway -- --bind 127.0.0.1:8080
#
# Response:
# info: charon-gateway starting
# info: Listening on address
```

## Step 2: Configure and start the provider

The provider daemon sits next to your Ollama engine. It requires a private key file, a keybind JSON file, and a configuration TOML.

### Create identity files
First, generate a private key and its corresponding keybind file.

```bash
# Generate a 32-byte private key in base64 format
echo "BwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwc=" > x25519.key

# Create a keybind JSON file mapping the key to a dev principal
# Note: For development, the signature is dummy but the public key must match your private key.
# For the key above, the public key is "hV644x/b8v5lq5o45G5p7c+g9mCg9g==" (dev-canned)
cat << 'EOF' > keybind.json
{
  "x25519_pub": "5N4zY9Z5Sg3/2837482394782397489237498237489=",
  "sig": "dev-keybind",
  "not_after": 0
}
EOF
```

### Create the configuration
Create a file named `charon-provider.toml` in your current directory.

```toml
# charon-provider.toml
[gateway]
url = "ws://localhost:8080/ws"
provider_id = "provider_a"

[identity]
x25519_key_file = "x25519.key"
keybind_file = "keybind.json"

[wallet]
rail = "dev"
receive_address = "dev_address"

[ollama]
base_url = "http://localhost:11434"

[[models]]
name = "qwen2.5-coder:32b"
ollama_model = "qwen2.5-coder:32b"
context_length = 4096
price_msat_per_mtok_in = 200000
price_msat_per_mtok_out = 600000
```

### Start the provider daemon
Run the provider command. It will connect to the local gateway and register the model.

```bash
# Start the provider daemon
cargo run -p charon -- provider --config charon-provider.toml
#
# Response:
# info: charon provider connecting
# info: provider registered
```

**Note:** On startup, the provider prints its actual public key in the logs, e.g. `provider_x25519_pub = "VfG...="`. Update your `keybind.json` `"x25519_pub"` field with this printed key if needed, though dev mode accepts mismatched signatures.

## Step 3: Start the consumer proxy

The consumer proxy exposes an OpenAI-compatible API to your agents. It intercepts OpenAI requests, handles gateway negotiations, and establishes end-to-end encrypted sessions to the provider.

Retrieve the provider's public key from the provider's startup logs (e.g. `VfG...=`). Use it to start the consumer proxy.

```bash
# Start the consumer proxy on port 8088
CHARON_PROVIDER=provider_a \
CHARON_PROVIDER_X25519_PUB="5N4zY9Z5Sg3/2837482394782397489237498237489=" \
CHARON_MODELS="qwen2.5-coder:32b" \
cargo run -p charon -- consumer --listen 127.0.0.1:8088
#
# Response:
# info: charon consumer listening
```

## Step 4: Interact with the consumer proxy

Now query the consumer proxy using any standard OpenAI client or `curl`.

### Estimate costs
Preview the cost of a chat prompt.

```bash
# Query the estimate-cost endpoint
curl -X POST http://127.0.0.1:8088/v1/estimate-cost \
  -H "Content-Type: application/json" \
  -d '{
    "model": "qwen2.5-coder:32b",
    "messages": [{"role": "user", "content": "hello"}],
    "max_tokens": 100
  }'
#
# Response:
# {
#   "model": "qwen2.5-coder:32b",
#   "provider": "provider_a",
#   "est_input_tokens": 8,
#   "max_tokens": 100,
#   "provider_msat": 76000,
#   "gateway_msat": 21000,
#   "total_msat": 97000
# }
```

### Run inference
Send a chat completion request. The proxy opens a session on the gateway, runs the Noise handshake, encrypts the request, gets the stream, and decrypts it.

```bash
# Query the chat completions endpoint
curl -X POST http://127.0.0.1:8088/v1/chat/completions \
  -H "Content-Type: application/json" \
  -d '{
    "model": "qwen2.5-coder:32b",
    "messages": [{"role": "user", "content": "Say hello!"}],
    "stream": false
  }'
#
# Response:
# {
#   "id": "chatcmpl-a96d19ef-505f-45a8-b649-165f972b9a71",
#   "object": "chat.completion",
#   "created": 1719168400,
#   "model": "qwen2.5-coder:32b",
#   "choices": [
#     {
#       "index": 0,
#       "message": {
#         "role": "assistant",
#         "content": "charon dev provider response"
#       },
#       "finish_reason": "stop"
#     }
#   ],
#   "usage": {
#     "prompt_tokens": 8,
#     "completion_tokens": 4,
#     "total_tokens": 12
#   }
# }
```
