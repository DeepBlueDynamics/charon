# Charon

A blind, end-to-end-encrypted marketplace for LLM inference, paid in bitcoin,
built on the NUTS ecosystem. Providers run Ollama and choose which models to
sell; consumers run any coding agent (via Nemesis8) against those models. A
central gateway authenticates both sides with NUTS tokens, matches them, relays
opaque ciphertext, settles payment, and takes a fee — without ever seeing a
prompt or a reply.

Charon is the ferryman: it carries traffic across, gets paid, and learns
nothing about its passengers.

- **Client** (`charon`) — one container, two roles. `charon consumer` exposes a
  plain OpenAI-compatible API locally and does all the marketplace work behind
  it (identity, encryption, payment). `charon provider` runs next to Ollama and
  sells chosen models.
- **Service** (`charon-gateway`) — the blind relay: authenticates both sides,
  matches them, relays ciphertext, settles payment, aggregates reputation.
- **Dashboard** (planned) — a web UI to log in (NUTS), browse provider
  advertisements, and fund/settle payments. See [spec 12](./spec/12-ui-dashboard.md).

## Payments

Wire unit is millisatoshi. Three rails (spec 05):

- **Lightning (L402)** — BOLT11 invoice + macaroon; provider payouts via BOLT12.
- **Cashu (ecash)** — recommended on top of Lightning because blind signatures
  keep the *payer* unlinkable even from the gateway (the "double-blind").
- **Balance** — prepaid custodial msat ledger for the simplest UX.

Lightning is first-class for both paying and getting paid; Cashu adds payer
privacy.

## Workspace

```
crates/
  core/      charon-core    wire contract, pricing, NUTS auth, Noise IK crypto
  gateway/   charon-gateway the blind relay (binary)
  client/    charon         consumer + provider proxies (binary)
spec/        00–12          authoritative protocol + deployment + UI specs
```

## Quickstart (dev)

Requires Rust and (for the provider) a local Ollama.

```bash
# Gateway — dev mode skips NUTS validation; never do this in production.
DISABLE_AUTH=true cargo run -p charon-gateway          # listens on :8080

# Consumer proxy — OpenAI-compatible API for any agent.
cargo run -p charon -- consumer --listen 127.0.0.1:8088

# Provider proxy — next to Ollama.
cargo run -p charon -- provider --ollama http://localhost:11434
```

`cargo test --workspace` runs the contract + pricing tests.

## Status

Early. The protocol crates compile and the gateway relays opaque frames; the
end-to-end inference path (consumer handshake → relay → Ollama) and real
payment rails are in progress. Tracking notes live in the commit history and
the specs; where code and spec disagree, the spec wins.

## Specifications

The authoritative description lives in [`spec/`](./spec). Start with
[`spec/README.md`](./spec/README.md), or jump to:

- [00 — Overview](./spec/00-overview.md)
- [01 — Architecture](./spec/01-architecture.md)
- [02 — Identity & Auth](./spec/02-identity-auth.md)
- [03 — Wire Protocol](./spec/03-wire-protocol.md)
- [04 — Encryption](./spec/04-encryption.md)
- [05 — Payments](./spec/05-payments.md)
- [06 — Provider](./spec/06-provider.md)
- [07 — Consumer & Nemesis8](./spec/07-consumer-nemesis8.md)
- [08 — Discovery & Reputation](./spec/08-discovery-reputation.md)
- [09 — Gateway](./spec/09-gateway.md)
- [10 — Security & Threat Model](./spec/10-security-threat-model.md)
- [11 — Deployment](./spec/11-deployment.md)
- [12 — UI / Dashboard](./spec/12-ui-dashboard.md)

## License

BSD 3-Clause. See [LICENSE](./LICENSE).
