# 10 — Security & Threat Model

A blunt accounting of what Charon defends, what it doesn't, and the residual risk
the design accepts.

## Trust boundaries (recap)

- **Gateway:** untrusted for content. Sees envelope metadata, payment, parties,
  and traffic shape. Cannot read prompts/replies or MITM (given key pinning).
- **Provider:** trusted with plaintext (it runs the model). Untrusted otherwise
  — bounded by payment, envelope match, and reputation.
- **Consumer:** untrusted by the provider — bounded by prepayment and the
  envelope-match check.
- **Network:** untrusted; everything is TLS + Noise.

## Threats and mitigations

### T1 — Gateway reads prompts/replies
Mitigated by E2EE (04): the gateway holds no session key. **Residual:** none for
content; it still sees metadata and shape.

### T2 — Gateway MITMs the handshake
A malicious gateway substitutes its key for the provider's. Mitigated by
identity-bound, **non-gateway-forgeable** key bindings (02) plus consumer
**pinning** and Noise IK (the consumer encrypts to the pinned key). **Residual:**
a consumer that uses naive TOFU on first contact and never verifies the binding
out-of-band is exposed on that first session; strict consumers require a
published binding (BARKER/Nostr).

### T3 — Provider sees plaintext
True by construction (E2EE protects content from the gateway, not the model
host). **Mitigation if unacceptable:** run the provider in confidential compute
(a TEE), as the NUTS ecosystem already does for a TEE-hosted Cashu mint; the
consumer would attest the enclave before sending. Out of scope for v0.1; noted
as the upgrade path.

### T4 — Traffic-shape side channel
Chunk sizes and timing leak response length and partial content to the gateway
and network. **Accepted** by project decision. Optional mitigation: fixed-size
chunk padding and/or micro-batching (04), at latency cost. A party needing shape
privacy runs its own gateway/provider.

### T5 — Consumer underpays via a cheap envelope
A consumer declares a small `max_tokens`/model then sends a bigger request.
Mitigated by the provider's **envelope match** (04/06): the decrypted request is
checked against the paid terms and rejected on mismatch; the Noise prologue binds
the terms so the gateway can't have altered them either.

### T6 — Payment fraud / replay
- L402 preimages are single-use and burned **before** inference; replay fails.
- Cashu proofs are swapped at the mint (double-spend caught there).
- Balances are debited atomically.
- **Residual:** L402 is non-refundable on provider failure by design; use Cashu/
  balance for refundable semantics (05).

### T7 — Sybil reputation
Fake providers/consumers inflating ratings. Mitigated by **settled-sat
weighting** (08): reputation weight requires real cleared payment (through the
fee), so faking it is net-lossy. NUTS-auth OIDC adds identity friction.
**Residual:** a well-funded attacker can still buy reputation; weighting raises
the cost, it doesn't eliminate it.

### T8 — Malicious provider returns garbage / withholds
Bounded by reputation (it tanks) and refundable rails (consumer recovers funds
on detectable failure). **Residual:** subtle quality degradation is not
automatically detectable; this is what relationships/ratings are for.

### T9 — Gateway denial of service / censorship
A gateway can drop, delay, or refuse. **Not prevented** — it's the relay.
**Mitigation:** fork-friendliness (09) and portable identity/reputation (02/08)
mean parties can switch or self-host; no trust is lost in the move.

### T10 — Key compromise
Compromise of a NUTS identity or its X25519 key affects connect, encryption, and
reputation together (the triple-duty cost, 02). **Mitigation:** signed key
rotation with `not_after`; consumers update pins only on validly signed
rotations; revocation lists SHOULD be publishable alongside bindings.

### T11 — Agent/tool abuse inside Nemesis8
A consumer's coding agent could exfiltrate or misbehave. Mitigated by Nemesis8's
**per-session network scoping** (07): grant the sealed container network access
to *only* the consumer-proxy endpoint, so the agent can reach the model and
nothing else.

## Custody & regulatory

Routing payments and (especially) holding `balance` ledgers is custodial money
movement with jurisdiction-dependent obligations (KYC/AML, money-transmission).
Favor per-request Cashu over standing balances to minimize custody. Reselling a
third-party API key (if a provider proxies an upstream instead of Ollama)
typically violates that provider's terms; **self-hosted Ollama models do not
have this problem** and are the intended provider backend. These are flagged for
the operator to resolve with counsel, not solved here.

## Residual risk, stated plainly

Charon makes the **gateway** blind to content and (with Cashu) to payer, and makes
trust portable and fork-resistant. It does **not** hide content from the
provider, does **not** hide traffic shape, and does **not** prevent a
provider from being lazy or a gateway from censoring. Those are addressed by
confidential compute (T3), padding (T4), and the freedom to leave (T9) —
deliberately pushed to the edges rather than promised by the relay.
