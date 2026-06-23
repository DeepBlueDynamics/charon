# 03 — Wire Protocol

## Transport

- Each proxy opens one persistent **WebSocket** to the gateway and keeps it
  open, reconnecting with backoff. This gives NAT traversal (outbound only) and
  multiplexes all of that party's sessions.
- A deployment MAY front the gateway with `tunnel.nuts.services`; framing is
  unchanged.
- Control frames are JSON text messages. Encrypted payload chunks are carried as
  base64 in JSON frames (v0.1) or as binary frames (v0.2+); both are opaque to
  the gateway.
- Requests are correlated by `req_id` (UUIDv4 string). A `session_id` groups the
  handshake and the request/response of one paid call.

## Frame envelope

```json
{ "type": "<frame_type>", "req_id": "…", "session_id": "…", "...": "..." }
```

Unknown frame types MUST be ignored by gateways (forward-compat) but logged.

## The cleartext routing envelope

Sent by the consumer to open a paid session. This is the **only** application
data the gateway is entitled to read.

```json
{
  "type": "open",
  "session_id": "uuid",
  "provider": "provider@example.com",      // target principal
  "model": "qwen2.5-coder:32b",            // model name as advertised
  "max_tokens": 2048,                       // billing cap (05)
  "est_input_tokens": 850,                  // consumer estimate, for pricing
  "payment": { "rail": "cashu", "token": "cashuB…" },  // see 05
  "consumer_keybind": { "x25519_pub": "…", "sig": "…", "not_after": 0 }  // 02
}
```

The envelope MUST NOT contain any prompt content. `est_input_tokens` is a count,
not text. The gateway prices and routes from this object alone.

## Frame catalog

### Consumer → Gateway
| type | purpose |
|------|---------|
| `open` | Open a paid session (the envelope above). |
| `hs` | A relayed Noise handshake message (opaque blob). |
| `req` | Encrypted request body chunk (opaque). |
| `cancel` | Abort a session. |

### Provider → Gateway
| type | purpose |
|------|---------|
| `register` | Authenticate + advertise (below). |
| `hs` | Relayed Noise handshake message (opaque blob). |
| `res_head` | Encrypted response metadata (status, content-type). Opaque body. |
| `res` | Encrypted response body chunk (opaque). |
| `res_end` | End of response; carries signed `usage` for settlement (05). |
| `pong` | Keepalive. |

### Gateway → proxy
| type | purpose |
|------|---------|
| `registered` | Registration accepted; assigns `session_id` space. |
| `open_ok` / `402` | Payment accepted (route reserved) / challenge (05). |
| `deliver` | Forward a relayed frame (`hs`/`req`/`res`/…) to the peer. |
| `cancel` | Tell a peer to abort an in-flight session. |
| `settled` | Settlement result for a session (05). |
| `ping` | Keepalive. |
| `error` | Structured error (below). |

### `register` payload (provider)

```json
{
  "type": "register",
  "ahp_token": "ahp_…",
  "keybind": { "x25519_pub": "…", "sig": "…", "not_after": 0 },
  "models": [
    { "name": "qwen2.5-coder:32b", "backend": "ollama",
      "context_length": 131072,
      "price_msat_per_mtok_in": 0, "price_msat_per_mtok_out": 0 }
  ],
  "payout": { "rail": "bolt12", "address": "lno1…" }
}
```

The gateway MUST validate `ahp_token` (02) and SHOULD verify `keybind` is
internally consistent before listing the provider in the directory (08).

## Relay semantics

- `hs`, `req`, `res*` bodies are **opaque** to the gateway: it copies them
  between the two peers of a `session_id` and MUST NOT attempt to parse them.
- The gateway MUST enforce that only the two principals bound to a `session_id`
  can send/receive its frames.
- Heartbeats: gateway sends `ping` ≤ every 30 s; a missed `pong` window closes
  the session.

## Errors

```json
{ "type": "error", "req_id": "…", "code": "payment_required",
  "message": "…", "http_status": 402 }
```

Codes mirror 05/10: `auth_failed`, `payment_required`, `underpaid`,
`unknown_model`, `no_provider`, `provider_gone`, `envelope_mismatch`,
`rate_limited`, `key_unverified`.

## Versioning

The first frame in each direction MAY carry `"v": 1`. A gateway and proxy MUST
agree on a major version; unknown minor fields are ignored.
