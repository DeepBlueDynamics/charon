# Charon

A blind, end-to-end-encrypted marketplace for LLM inference, paid in bitcoin,
built on the NUTS ecosystem. Providers run Ollama and choose which models to
sell; consumers run any coding agent (via Nemesis8) against those models. A
central gateway authenticates both sides with NUTS tokens, matches them, relays
opaque ciphertext, settles payment, and takes a fee — without ever seeing a
prompt or a reply.

Charon is the ferryman: it carries traffic across, gets paid, and learns
nothing about its passengers.

- **Client** — the consumer/provider proxies. The consumer proxy exposes a
  plain OpenAI-compatible API locally and does all the marketplace work behind
  it (identity, encryption, payment). The provider proxy runs next to Ollama.
- **Service** — the gateway (relay): a blind matchmaker that connects, relays
  ciphertext, settles payment, and aggregates reputation.

## Specifications

The authoritative description of the protocol lives in [`spec/`](./spec). Start
with [`spec/README.md`](./spec/README.md) for the reading order, or jump to:

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

## License

BSD 3-Clause. See [LICENSE](./LICENSE).
