# Charon — Specifications

**Status:** Draft v0.1 · An encrypted inference marketplace

A blind, end-to-end-encrypted marketplace for LLM inference, paid in bitcoin,
built on the NUTS ecosystem. Providers run Ollama and choose which models to
sell. Consumers run any coding agent (via Nemesis8) against those models. A
central gateway authenticates both sides with NUTS tokens, matches them, relays
opaque ciphertext, settles payment, and takes a fee — without ever seeing a
prompt or a reply.

## The two objectives these specs serve

1. A **provider** runs Ollama and picks models to share. See [06-provider.md](./06-provider.md).
2. A **consumer** uses **Nemesis8** to fire up any coding agent and point it at a shared model. See [07-consumer-nemesis8.md](./07-consumer-nemesis8.md).

## Reading order

| # | Document | What it covers |
|---|----------|----------------|
| 00 | [overview](./00-overview.md) | Goals, actors, principles, glossary, non-goals |
| 01 | [architecture](./01-architecture.md) | Topology, planes, request lifecycle, trust boundaries |
| 02 | [identity-auth](./02-identity-auth.md) | NUTS tokens, the per-identity static key, pinning |
| 03 | [wire-protocol](./03-wire-protocol.md) | Tunnel frames, the cleartext envelope, correlation |
| 04 | [encryption](./04-encryption.md) | Noise handshake, AEAD, envelope binding, MITM defense |
| 05 | [payments](./05-payments.md) | Pricing, rails, the fee cut, settlement, double-blind |
| 06 | [provider](./06-provider.md) | Provider proxy, Ollama, model-share config |
| 07 | [consumer-nemesis8](./07-consumer-nemesis8.md) | Consumer proxy, OpenAI surface, Nemesis8 wiring |
| 08 | [discovery-reputation](./08-discovery-reputation.md) | Directory, BARKER, portable signed ratings |
| 09 | [gateway](./09-gateway.md) | Blind-relay responsibilities, state, rate limits |
| 10 | [security-threat-model](./10-security-threat-model.md) | Threats, mitigations, residual risk |
| 11 | [deployment](./11-deployment.md) | Container roster, Cloud Run (nuts.services), auth wiring |
| 12 | [ui-dashboard](./12-ui-dashboard.md) | Web UI: NUTS login, provider ads, payments; gateway HTTP API |

## Conventions

The key words MUST, MUST NOT, SHOULD, SHOULD NOT, and MAY are used as in
RFC 2119. JSON is the canonical encoding for control messages. All sizes are
bytes unless noted; all monetary amounts are millisatoshis (msat) on the wire.

## Relationship to the reference scaffold

A Rust workspace scaffold (`charon`) exists with the gateway, the
provider proxy, the shared wire types, and stubbed payment rails. These specs
are the authoritative description; the scaffold is one implementation in
progress. Where they disagree, the spec wins.
