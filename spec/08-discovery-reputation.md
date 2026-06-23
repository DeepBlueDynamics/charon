# 08 — Discovery & Reputation

## Discovery

Three layers, increasingly decentralized; a deployment MAY use any subset.

1. **Gateway directory.** The gateway lists currently-connected providers and
   their model cards (from `register`, 03). A consumer queries it to find who
   serves a model and at what price. Simplest; gateway-scoped.
2. **Trusted-provider pins (primary trust path).** A consumer carries its own
   curated set of provider principals + pinned X25519 keys (02). In normal
   operation a consumer transacts only with pinned providers and ignores the
   open directory. Trust is client-side and relational.
3. **BARKER (optional, Lightning-native).** Providers MAY advertise on BARKER
   via keysend ads (`category:"inference"`, `capabilities:[model names]`,
   `cost_range`), and consumers MAY discover/subscribe there. BARKER's
   satoshi-weighted ranking and TTL apply. This is the cross-gateway,
   "discovery IS the network" path.

Discovery and the encrypted data plane are separate: discovery returns a
provider principal + signed key binding; the actual call is the E2EE session
(04). Keysend/BARKER is for small control/discovery messages only, never the
inference stream.

## Reputation

### Requirements
- Ratings are authored by **consumers**, not the gateway.
- A rating is a **signed attestation** bound to the rated provider's identity and
  a specific settled session.
- Ratings are **weighted by settled sats** (the one thing a blind gateway can
  verify), which makes fake reputation cost real money.
- Reputation is **portable**: it survives a gateway fork because it is anchored
  to the provider's NUTS identity and publishable outside the gateway.

### Attestation schema

```json
{
  "v": 1,
  "type": "charon-rating",
  "subject": "provider@example.com",        // rated provider principal
  "subject_key": "<x25519_pub>",            // pin at time of session
  "rater": "consumer@example.com",          // NUTS principal of author
  "session_id": "uuid",
  "settled_msat": 4200,                      // sats that cleared this session
  "model": "qwen2.5-coder:32b",
  "score": 5,                                // 1..5
  "tags": ["fast", "reliable"],
  "ts": 1750000000,
  "sig": "<rater NUTS-identity signature over the above>"
}
```

- The `sig` MUST be verifiable against the `rater` NUTS identity, independent of
  the gateway.
- An aggregator MUST discard an attestation whose `session_id` it cannot
  corroborate as settled (the gateway corroborates from its ledger; an external
  aggregator MAY require a gateway-countersigned settlement receipt).

### Weighting

A provider's score is a `settled_msat`-weighted aggregate of valid attestations,
optionally time-decayed. Because weight requires real cleared payment, a Sybil
flooding fake ratings must actually pay (through the fee) for each — net-lossy.
The gateway MAY also countersign a **settlement receipt** per session so
attestations are verifiable by parties who don't trust the gateway's ledger
read.

### Ownership and portability

- The gateway is an **aggregator**, not the **owner**, of reputation.
- Consumers SHOULD publish attestations where they outlive any one gateway —
  e.g. BARKER reputation records or Nostr events keyed to the provider identity.
- A forked/private gateway can import the same published attestations, so a
  provider carries reputation across gateways. This is what makes "fork it if you
  don't like it" (00 §7) consistent with having reputation at all.

## Mutuality

The envelope carries the consumer's validated identity (03), so providers MAY
likewise allowlist, price, or rate **consumers** — supporting the
"providers will have relationships with consumers" model in both directions.
