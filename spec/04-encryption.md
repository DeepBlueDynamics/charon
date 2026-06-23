# 04 — End-to-End Encryption

## Goal

The prompt (request body) and the reply (response stream) MUST be unreadable to
the gateway and to any network observer. They are encrypted between the consumer
proxy and the provider proxy. The gateway relays ciphertext only and never holds
a session key.

This protects content from the **gateway** and the **wire**. It does not protect
content from the **provider**, which decrypts to run the model (see 00 §non-goals,
10 for the TEE path).

## Handshake: Noise IK

Charon uses the **Noise Protocol Framework**, pattern **IK**, suite
`Noise_IK_25519_ChaChaPoly_BLAKE2s`.

- **Initiator:** consumer proxy. **Responder:** provider proxy.
- IK is chosen because the initiator already knows the responder's static key
  (the consumer has the provider's pinned X25519 key, 02), giving mutual
  authentication and forward secrecy in one round trip. It is the same family as
  Lightning's transport (BOLT-8, `Noise_XK`), which fits the ecosystem.
- Handshake messages travel as `hs` frames (03), relayed opaquely by the
  gateway. The gateway sees ephemeral public values but cannot derive the shared
  secret (it never holds either static private key).

### Prologue binds the deal

The Noise **prologue** MUST be set to a canonical serialization of the paid
envelope's binding fields:

```
prologue = H(provider_principal || consumer_principal || model || max_tokens || session_id)
```

Because the prologue is mixed into the handshake hash, a gateway that tampered
with any of those fields (e.g. swapped the model or the counterparty) causes the
handshake to fail. This ties the *encrypted channel* to the *paid terms*.

## Transport encryption

After the handshake, each direction has a Noise CipherState. Application
messages (request body, each response chunk) are AEAD-sealed with
**ChaCha20-Poly1305**. Nonces are managed by Noise (monotonic per direction).
The plaintext is the OpenAI-format JSON body / SSE chunk.

- A rekey SHOULD occur per Noise rules; sessions are short-lived (one request +
  its stream), so a single key per session is acceptable in v0.1.
- The response `res_head`/`res`/`res_end` bodies are sealed the same way.

## Envelope binding on the provider side

After decrypting the request, the provider MUST verify the decrypted body
matches the paid envelope before doing any work:

- `model` in the body == envelope `model` (or absent → use envelope).
- requested `max_tokens` ≤ envelope `max_tokens`.
- input size ≤ what was priced (the provider MAY recompute tokens; if the real
  input exceeds the paid estimate beyond tolerance, reject with
  `envelope_mismatch`).

This stops a consumer from declaring a cheap envelope and then sending an
expensive request. It is the encrypted-channel equivalent of L402 macaroon
caveats (05).

## MITM resistance

The only way the gateway could read content is to MITM the handshake by
substituting its own static key for the provider's. This is prevented by:

1. The provider's X25519 key is **bound to its NUTS identity by a signature the
   gateway cannot forge** (02).
2. The consumer **pins** that key and, in IK, encrypts to it directly. A gateway
   key substitution yields a key the consumer never pinned → handshake/verify
   fails.

Therefore, with pinning enforced, a fully compromised gateway can deny service
and observe traffic shape, but cannot read prompts or replies. See 10.

## Accepted leak: traffic shape

Ciphertext sizes and timing of `req`/`res` chunks are visible to the gateway and
network. This is an **accepted** side channel (per project decision): chunk
sizes and inter-chunk timing can leak response length and, with effort, partial
content.

- Implementations MAY pad chunks to fixed-size buckets and/or micro-batch tokens
  to blunt this, at a latency cost.
- v0.1 does not require padding. The trade-off is documented in 10; a consumer
  who needs shape privacy runs its own gateway/provider.

## What the gateway can and cannot do

| Capability | Gateway |
|------------|:-------:|
| Read prompt / reply | ❌ (no session key) |
| Forge provider identity to MITM | ❌ (signed pinned key) |
| Observe chunk sizes / timing | ✅ (accepted) |
| Drop / delay / deny | ✅ (DoS; out of scope to prevent) |
| Learn model, cap, price, parties | ✅ (envelope, by design) |
