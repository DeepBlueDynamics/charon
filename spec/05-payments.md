# 05 — Payments

## Unit and pricing

- Wire unit is **millisatoshi (msat)**. Display MAY be sats.
- Pricing follows the cap-based model (validated against `llm402.ai`): a request
  is priced **up front** from `est_input_tokens` plus the **`max_tokens` cap**,
  not from actual output. Sizing the cap is the consumer's main cost lever.

```
provider_msat = price_msat_per_mtok_in  * est_input_tokens / 1e6
              + price_msat_per_mtok_out * max_tokens       / 1e6
```

### The gateway fee (the "cut")

The gateway applies a markup and a floor:

```
total_msat    = max(provider_msat * (10_000 + markup_bps) / 10_000, floor_msat)
gateway_msat  = total_msat - provider_msat
provider_msat = provider_msat            // paid out to the provider
```

Defaults: `markup_bps = 1000` (+10%), `floor_msat = 21_000` (21 sat floor).
The consumer pays `total_msat`; the provider receives `provider_msat`; the
gateway keeps `gateway_msat`.

### `estimate-cost`

The consumer proxy SHOULD expose a free local `estimate-cost` that returns
`{ model, est_input_tokens, max_tokens, total_msat }` so an agent/user sees the
price before paying. This is also the antidote to "one question cost a nickel":
the consumer can see a heavyweight model's price and pick a cheaper one.

## Where payment lives in the flow

Because the request body is encrypted (04), payment is **not** carried in HTTP
headers on the body. It rides the cleartext envelope (`payment` field, 03):

```json
"payment": { "rail": "cashu",   "token": "cashuB…" }
"payment": { "rail": "l402",    "macaroon": "Ag…", "preimage": "…" }
"payment": { "rail": "balance", "token": "bal_…" }
```

The gateway verifies/collects payment against the envelope's `total_msat` before
reserving the route, independent of (and unable to read) the encrypted body.

## Rails

A deployment MUST support at least one rail. **Cashu is RECOMMENDED as primary**
because it pairs with E2EE to make the gateway blind on both content *and* payer
(see "double-blind").

### Cashu (ecash) — recommended
- Consumer puts a `cashuB` (v4) sat-denominated token in the envelope.
- Gateway swaps it at an allowlisted mint, splitting `gateway_msat` (kept) from
  `provider_msat` (forwarded / locked to the provider, e.g. P2PK to the
  provider's key per NUT-11), returning change to the consumer if overpaid.
- No streaming constraint at the Charon layer (change is computed at settlement on
  the envelope, not from the body).

### L402 (Lightning)
- Gateway issues a `402` with a BOLT11 invoice + macaroon bound to
  `{model, max_tokens, session_id, est_input}` caveats (mirrors llm402).
- Consumer pays, returns `macaroon:preimage` in the envelope.
- Preimage MUST be single-use and burned atomically **before** relaying the
  request. Non-refundable on failure (use balance/Cashu for refundable).

### Balance (prepaid)
- Consumer funds a `bal_` token (via Lightning or Cashu) held as a custodial
  msat ledger keyed to the consumer principal; debited per session, with
  refund-on-failure. Highest custody, simplest UX.

## The double-blind

The prize: stack **Cashu** (payer unlinkable via blind signatures) with **E2EE**
(content unreadable). The gateway can then prove it was paid `total_msat` and
prove nothing about *who asked what*. It is a paid blind matchmaker. This
combination is the recommended target posture.

## Settlement

1. On `res_end` (03) the provider includes a **signed `usage`** report
   `{prompt_tokens, completion_tokens}` over the `session_id`.
2. v0.1 settles on the **paid envelope** (cap), so the provider payout is fixed
   at quote time; `usage` is recorded for reputation/analytics only.
3. Actual-usage (post-paid) billing is OPTIONAL and requires trusting the
   provider's signed `usage` or a fraud-proof; defer to a later version.
4. Failure semantics by rail: L402 non-refundable; Cashu/balance refundable
   (consumer is made whole on provider/upstream failure). The gateway MUST emit
   a `settled` frame with the outcome.

## Custody & regulatory note

Holding consumer balances or routing payments is custodial activity with real
regulatory weight. Minimizing custody (favor per-request Cashu over long-lived
balances) reduces exposure. This is flagged, not solved, here (see 10).
