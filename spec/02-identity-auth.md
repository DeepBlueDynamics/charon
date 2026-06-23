# 02 — Identity & Authentication

## Principals

Every party is a **NUTS principal**, identified by the email in its nuts-auth
token claims. The gateway uses that principal id to scope reputation, settlement
accounts, and rate limits. The same principal owns a long-lived **static
encryption key** used for end-to-end encryption (04).

## NUTS tokens

Validated against `GNOSIS_AUTH_URL` (default `https://auth.nuts.services`;
self-hostable as `deepbluedynamics/nuts-auth`). Routing is by token shape:

| Token | Shape | Validate via | Returns |
|-------|-------|--------------|---------|
| API token | `ahp_<opaque>` | `POST {auth}/auth` form `token=…` | JWT; principal in `sub`/email |
| Browser JWT | `eyJ…` (3 parts) | `GET {auth}/api/verify` Bearer | claims, or 401 |

- Provider and consumer proxies authenticate with an `ahp_` token (long-lived,
  suited to daemons). The web dashboard uses the JWT flow.
- The gateway MUST validate the token on connect (the `Register` / first frame,
  03) and MUST reject a connection whose token fails validation.
- `DISABLE_AUTH=true` skips validation for private deployments and MUST NOT be
  set on a public gateway.

## The static encryption key

Each principal generates an **X25519 static keypair**. The public key is the
party's end-to-end address.

- A party MUST bind its X25519 public key to its NUTS identity with a signature
  the relying party can verify independently of the gateway. Concretely, the
  party signs `keybind = sign(nuts_identity, x25519_pub || principal || not_after)`
  using a key the NUTS identity controls (e.g. an LNURL-auth-style linking key,
  or a key whose pubkey is registered in nuts-auth). The exact NUTS signing
  primitive is an integration detail; the requirement is: **the binding is not
  forgeable by the gateway.**
- Providers publish the signed binding at registration (03) and in discovery
  (08). Consumers publish theirs in the handshake (04).

### Why the binding is load-bearing

If the consumer learned the provider's encryption key *from the gateway with no
independent check*, the gateway could substitute its own key and sit in the
middle (MITM), making "end-to-end" actually "end-to-gateway-to-end". The signed,
identity-bound key plus consumer-side **pinning** (08) closes this. See 10.

## Pinning and trust-on-first-use

- A consumer keeps a **pin set**: `{provider_principal → x25519_pub}` for
  providers it trusts. A pinned provider's key MUST match on every session;
  a mismatch MUST abort with a security error.
- First contact MAY use trust-on-first-use (accept the signed binding, pin it)
  or require an out-of-band/`BARKER`/Nostr-published binding. Strict consumers
  SHOULD require a published binding and refuse TOFU.

## Triple duty

The NUTS identity is deliberately the same object across three concerns:

1. **Connect** — gates the WS to the gateway (this document).
2. **Encrypt** — anchors the X25519 key used for E2EE (04).
3. **Reputation** — signs and accrues ratings (08).

One key compromise therefore affects all three; rotation and revocation are in
10.

## Key rotation

- A party MAY rotate its X25519 key by publishing a new signed binding with a
  later `not_after`. Consumers update pins on a validly signed rotation.
- The gateway MUST NOT be the sole source of a rotation; a rotation a consumer
  cannot verify against the NUTS identity MUST be rejected.
