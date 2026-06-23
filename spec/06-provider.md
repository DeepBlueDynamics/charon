# 06 — Provider

Satisfies objective (a): *a provider runs Ollama and picks models to share.*

## Responsibilities

The **provider proxy** is a process the provider runs next to Ollama. It:

1. Loads config: which local models to sell, at what price, and the payout
   wallet.
2. Connects out to the gateway and `register`s (03) with the provider's NUTS
   `ahp_` token, its signed X25519 key binding (02), and its model cards.
3. For each session: completes the Noise handshake (04), decrypts the request,
   **enforces the envelope match**, calls Ollama, encrypts the streamed reply.
4. Receives payouts to its wallet.

It exposes **no inbound port**. All connectivity is the outbound WS to the
gateway.

## Picking models to share (config)

```toml
[gateway]
url         = "wss://gateway.example.com/ws"
provider_id = "you@example.com"     # NUTS principal; ahp via NUTS_AHP_TOKEN env

[identity]
x25519_key_file = "/etc/charon/x25519.key"   # static E2EE key (02)
keybind_file    = "/etc/charon/keybind.json" # NUTS-signed binding

[wallet]
rail            = "bolt12"
receive_address = "lno1…"

[ollama]
base_url = "http://localhost:11434"

# Each [[models]] entry is one model offered for sale.
[[models]]
name                    = "qwen2.5-coder:32b"   # public name consumers request
ollama_model            = "qwen2.5-coder:32b"   # local Ollama tag (if different)
context_length          = 131072
price_msat_per_mtok_in  = 200000                # 200 sat / 1M input tokens
price_msat_per_mtok_out = 600000                # 600 sat / 1M output tokens

[[models]]
name                    = "llama3.3:70b"
price_msat_per_mtok_in  = 300000
price_msat_per_mtok_out = 900000
context_length          = 131072
```

- A model is sold **only if** it appears in `[[models]]`. The proxy SHOULD verify
  the model is actually present in Ollama (`/api/tags`) at startup and warn /
  skip otherwise.
- Pricing is the provider's call; the gateway adds its fee on top (05).

## Per-request handling (normative)

1. Complete the Noise handshake as responder (04).
2. Decrypt the request body.
3. **Envelope match (MUST):** reject with `envelope_mismatch` if the decrypted
   `model` ≠ envelope model, requested `max_tokens` > envelope cap, or input
   tokens exceed the paid estimate beyond tolerance.
4. Proxy to Ollama (`/api/chat` or OpenAI-compat `/v1/chat/completions`),
   rewriting the model name to `ollama_model`.
5. Stream the response: encrypt each chunk into `res` frames; send `res_head`
   first and `res_end` (with signed `usage`) last (03).
6. On Ollama failure, send `res_end` with an error status; the gateway settles
   per the rail's refund policy (05).

## Wallet / payout

- The provider advertises a payout address at `register` (03).
- The gateway pays `provider_msat` per settled session (05). With Cashu the
  payout MAY be locked to the provider's key (P2PK) so it is non-custodial.
- The proxy's wallet is "multimodal": it receives payouts and MAY spend/melt for
  the provider's own use. Reference backends: `ldk-node`, `tonic_lnd`,
  `cln-rpc`, or `cdk` (Cashu).

## Local passthrough (optional)

The proxy MAY also expose a localhost OpenAI endpoint so the provider's own apps
use the same models directly, bypassing the marketplace. This path is unpaid and
unencrypted (local trust) and MUST bind to loopback only.

## Packaging

Ships as a container and a single binary. Requires only outbound network +
reachable Ollama. `DISABLE_AUTH`/private mode is for self-contained test only.

## MUST / SHOULD summary

- MUST authenticate to the gateway with a valid NUTS token.
- MUST enforce the envelope match before inference.
- MUST NOT serve a model absent from `[[models]]`.
- SHOULD verify advertised models exist in Ollama at startup.
- SHOULD sign `usage` in `res_end`.
