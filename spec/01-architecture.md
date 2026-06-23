# 01 — Architecture

## Topology

Both proxies dial **out** to the gateway over a persistent connection, so
neither needs an inbound port or a public IP (NAT traversal). The gateway holds
both connections and relays between them.

```
  Nemesis8 sealed container                                   Provider host
  ┌───────────────────────┐                                  ┌───────────────────────┐
  │  coding agent          │                                  │  Ollama (local models)│
  │  (OpenAI client)       │                                  └───────────▲───────────┘
  └──────────┬────────────┘                                               │
             │ http://…/v1 (plaintext, localhost)                          │ http://localhost:11434
  ┌──────────▼────────────┐        ┌───────────────────┐       ┌──────────┴────────────┐
  │  consumer proxy        │  WS    │     GATEWAY       │   WS  │   provider proxy       │
  │  - NUTS token + wallet │◄──────►│   (blind relay)   │◄─────►│   - NUTS token + wallet│
  │  - trusted-provider    │  ciphertext + cleartext    │       │   - decrypts, serves   │
  │    pins                │   envelope frames          │       │     envelope-matched   │
  └────────────────────────┘        └───────────────────┘       └────────────────────────┘
                          E2EE session (Noise) tunnelled through the gateway
                          ── prompt/reply ciphertext: gateway cannot read ──
```

## Two planes on one connection

Every request splits into:

- **Cleartext envelope** — what the gateway needs to do its job: which provider,
  which model, the `max_tokens` cap, the payment, a request id. (Schema in 03.)
- **Encrypted payload** — the actual messages going up and the completion chunks
  coming back, sealed under a consumer↔provider session key. (Crypto in 04.)

The gateway reads envelopes and relays payloads. It learns *that* a paid call to
model X with a 2k cap happened between two identities; it does not learn the
prompt or the answer.

## End-to-end request lifecycle

1. **Connect.** Provider proxy and consumer proxy each open a WS to the gateway
   and authenticate with their NUTS token (02). Provider registers its model
   cards and its signed static key; consumer is now able to request.
2. **Select (consumer-side).** The consumer (or Nemesis8) picks model + provider
   from its trusted set. No gateway involvement.
3. **Quote.** Consumer asks for / computes a price from the provider's published
   rate and the `max_tokens` cap (05). `estimate-cost` MAY be used.
4. **Pay + envelope.** Consumer sends an envelope (provider, model, cap, payment)
   to the gateway. Gateway validates the token, verifies/collects payment, and
   reserves a route to the provider.
5. **Handshake.** Consumer and provider run a Noise handshake whose bytes the
   gateway relays opaquely (04). A session key is established end-to-end.
6. **Request.** Consumer encrypts the OpenAI request body under the session key;
   gateway relays the ciphertext. Provider decrypts, **verifies the request
   matches the paid envelope** (model, cap), and calls Ollama.
7. **Stream back.** Provider encrypts each SSE chunk; gateway relays ciphertext;
   consumer proxy decrypts and streams plaintext to the agent.
8. **Settle.** On completion the gateway settles: provider is paid the agreed
   amount minus the gateway fee (05). Consumer MAY publish a signed rating (08).

## Trust boundaries — who sees what

| Data | Consumer | Gateway | Provider | Network |
|------|:--------:|:-------:|:--------:|:-------:|
| Prompt text | ✅ | ❌ | ✅ | ❌ |
| Reply text | ✅ | ❌ | ✅ | ❌ |
| Model name | ✅ | ✅ | ✅ | ❌ |
| `max_tokens` cap / price | ✅ | ✅ | ✅ | ❌ |
| Party identities (emails/keys) | ✅ | ✅ | ✅ | ❌ |
| Payment token | ✅ | ✅* | ✅* | ❌ |
| Traffic shape (sizes/timing) | ✅ | ✅ | ✅ | ✅ |

\* Payment visibility to the gateway depends on the rail; Cashu can keep the
*payer* unlinkable even from the gateway (05, 10).

## Deployment

- Gateway: a container, public TLS endpoint, horizontally scalable; holds only
  transient relay state plus a settlement ledger and a reputation store (09).
- Provider proxy: a container or binary on the provider's machine, next to
  Ollama. No inbound ports.
- Consumer proxy (the client): a container, run as a **sidecar** in the
  Nemesis8 session network (reachable at `http://charon-proxy:8088`) or as a
  single binary on the host (`host.docker.internal`). Same image as the provider
  proxy, selected by role (07).

## NUTS ecosystem mapping

| Concern | Component | Spec |
|---------|-----------|------|
| Identity / tokens | `auth.nuts.services` (self-hostable) | 02 |
| Provider model backend | Ollama | 06 |
| Consumer agent harness | Nemesis8 | 07 |
| Optional discovery/advertising | BARKER (keysend ads) | 08 |
| Optional managed tunnel fronting | `tunnel.nuts.services` | 03 |
