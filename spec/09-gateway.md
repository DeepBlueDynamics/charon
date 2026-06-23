# 09 — Gateway (the Relay)

The gateway is deliberately small: a **blind matchmaker** that connects, relays,
settles, and aggregates — and reads no content.

## Responsibilities

1. **Authenticate** both proxies by NUTS token on connect (02).
2. **Directory.** Track connected providers and their model cards + signed key
   bindings (03, 08).
3. **Match.** Reserve a route from a consumer's `open` envelope to the named
   provider. (No content-based routing — selection already happened
   consumer-side, 07.)
4. **Relay.** Copy `hs`/`req`/`res*` frames between the two principals of a
   `session_id`, treating bodies as opaque (03, 04).
5. **Settle.** Verify/collect payment from the envelope, take `gateway_msat`,
   pay out `provider_msat`, emit `settled` (05).
6. **Aggregate reputation.** Store and serve signed attestations; optionally
   countersign settlement receipts (08).
7. **Rate-limit & protect.** Per-principal and per-IP limits; session caps.

## State

| Stored | Notes |
|--------|-------|
| Connected-provider directory | Ephemeral; rebuilt on (re)connect. |
| In-flight session table | `session_id → {consumer, provider, envelope, payment}`. Ephemeral. |
| Settlement ledger | Per-session `{parties, total, fee, payout, outcome, ts}`. Durable. |
| Reputation store | Signed attestations + optional receipts (08). Durable. |
| Balances (if `balance` rail) | Custodial msat ledger keyed by principal (05). Durable. |

| **Never stored / never seen** |
|---|
| Prompt or reply plaintext |
| Noise session keys |
| Static private keys of either party |

A gateway MUST NOT log payload frames' decrypted content (it has none) and
SHOULD avoid logging payload sizes/timing beyond what ops requires (shape leak,
04/10).

## Configuration

```
BIND=0.0.0.0:8080
GNOSIS_AUTH_URL=https://auth.nuts.services   # or self-hosted nuts-auth
DISABLE_AUTH=false                           # MUST be false in production
MARKUP_BPS=1000                              # +10% fee
FLOOR_MSAT=21000                             # 21 sat floor
PAYMENT_RAILS=cashu,l402,balance             # at least one
CASHU_MINT_ALLOWLIST=https://mint.example.com
```

## Rate limits (defaults, per principal/IP)

| Class | Limit |
|-------|-------|
| Connect / register | 30 / min |
| Open session (paid) | 60 / min |
| Concurrent sessions | 5 / principal |
| Directory / reputation reads (free) | 60 / min |

Exceeding returns `rate_limited` with a retry hint.

## Scaling

The gateway is mostly I/O relay + settlement. It scales horizontally behind a
load balancer; the in-flight table is sticky to the node holding both WS
connections of a session (or shared via a fast store). The durable stores
(ledger, reputation, balances) are a shared database.

## Fork posture

The gateway is self-hostable and small. A provider/consumer cluster MAY run its
own for its own relationship graph; the public gateway is a default rendezvous,
not a requirement. Because reputation is portable (08) and identity/keys are
NUTS-anchored (02), forking does not fragment trust. The gateway's leverage is
convenience and liquidity, not lock-in.

## What the gateway is NOT

- Not a router that reads prompts (can't; 04).
- Not the owner of reputation (aggregator only; 08).
- Not a required intermediary for trust (pins are consumer-side; 02/08).
- Not a content moderator of inference (it sees no content). Abuse handling is
  necessarily limited to identity/payment/rate signals — see 10.
