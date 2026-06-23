//! Wire protocol — control frames and the cleartext routing envelope (spec 03).
//!
//! Control frames are JSON text messages on a single persistent WebSocket per
//! party. Encrypted payload chunks (`hs`/`req`/`res*` bodies) are **opaque** to
//! the gateway and are carried as base64 in v0.1 (`Blob`).

use serde::{Deserialize, Serialize};

/// Opaque, end-to-end-encrypted bytes the gateway relays but never parses.
/// Base64 in JSON for v0.1; binary frames in v0.2+.
pub type Blob = String;

/// The cleartext routing envelope — the ONLY application data the gateway is
/// entitled to read. MUST NOT contain any prompt content (spec 03).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Envelope {
    /// Target provider principal (NUTS identity / email).
    pub provider: String,
    /// The consumer's own NUTS principal. The gateway MUST verify this equals
    /// the authenticated consumer on the session (except in dev `DISABLE_AUTH`).
    /// Both parties mix it into the Noise prologue (04), so it must agree end to
    /// end — this is how the provider learns the consumer principal it cannot
    /// otherwise see.
    pub consumer: String,
    /// Model name as advertised by the provider.
    pub model: String,
    /// Billing cap on output tokens (spec 05).
    pub max_tokens: u32,
    /// Consumer's input-token estimate, for up-front pricing.
    pub est_input_tokens: u32,
    /// Payment for this session.
    pub payment: Payment,
    /// Consumer's identity-bound X25519 key (spec 02).
    pub consumer_keybind: Keybind,
}

/// A NUTS-identity-bound X25519 public key. The signature MUST be verifiable
/// against the party's NUTS identity independently of the gateway (spec 02).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Keybind {
    /// X25519 public key (base64).
    pub x25519_pub: String,
    /// Signature over `x25519_pub || principal || not_after` by the NUTS identity.
    pub sig: String,
    /// Unix expiry; `0` means no expiry.
    #[serde(default)]
    pub not_after: u64,
}

/// Payment rider on the envelope (spec 05). The gateway verifies/collects this
/// against the priced `total_msat` before reserving a route.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "rail", rename_all = "lowercase")]
pub enum Payment {
    /// Cashu ecash (recommended): a `cashuB` v4 token.
    Cashu { token: String },
    /// L402 / Lightning: macaroon + paid preimage.
    L402 { macaroon: String, preimage: String },
    /// Prepaid custodial balance token.
    Balance { token: String },
}

/// A model a provider offers for sale (spec 03 `register`, 06).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelCard {
    /// Public name consumers request.
    pub name: String,
    #[serde(default = "default_backend")]
    pub backend: String,
    pub context_length: u32,
    pub price_msat_per_mtok_in: u64,
    pub price_msat_per_mtok_out: u64,
}

fn default_backend() -> String {
    "ollama".to_string()
}

/// Signed usage report carried on `res_end` for settlement/analytics (spec 05).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Usage {
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    /// Provider signature over the usage + `session_id`.
    #[serde(default)]
    pub sig: String,
}

/// Provider payout destination advertised at `register` (spec 03/06).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Payout {
    pub rail: String,
    pub address: String,
}

/// Every control frame, tagged by `type` (spec 03 frame catalog).
///
/// `req_id` correlates a request; `session_id` groups the handshake and the
/// request/response of one paid call. Relayed bodies (`Hs`/`Req`/`Res*`) are
/// opaque [`Blob`]s.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Frame {
    // ---- Consumer → Gateway ----
    /// Open a paid session (carries the routing envelope).
    Open {
        session_id: String,
        #[serde(flatten)]
        envelope: Envelope,
    },
    /// A relayed Noise handshake message.
    Hs { session_id: String, blob: Blob },
    /// Encrypted request body chunk.
    Req { session_id: String, blob: Blob },
    /// Abort a session.
    Cancel { session_id: String },

    // ---- Provider → Gateway ----
    /// Authenticate + advertise model cards and the signed static key.
    Register {
        ahp_token: String,
        keybind: Keybind,
        models: Vec<ModelCard>,
        payout: Payout,
    },
    /// Encrypted response metadata (status, content-type). Opaque body.
    ResHead { session_id: String, blob: Blob },
    /// Encrypted response body chunk.
    Res { session_id: String, blob: Blob },
    /// End of response; carries signed `usage`.
    ResEnd {
        session_id: String,
        usage: Usage,
    },
    /// Keepalive.
    Pong,

    // ---- Gateway → proxy ----
    /// Registration accepted.
    Registered { provider: String },
    /// Payment accepted; route reserved.
    OpenOk { session_id: String, total_msat: u64 },
    /// Forward a relayed frame to the peer of this session.
    Deliver {
        session_id: String,
        frame: Box<Frame>,
    },
    /// Settlement result for a session (spec 05).
    Settled {
        session_id: String,
        total_msat: u64,
        gateway_msat: u64,
        provider_msat: u64,
        outcome: String,
    },
    /// Keepalive.
    Ping,
    /// Structured error.
    Error {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session_id: Option<String>,
        code: ErrorCode,
        message: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        http_status: Option<u16>,
    },
}

/// Structured error codes (spec 03/05/10).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    AuthFailed,
    PaymentRequired,
    Underpaid,
    UnknownModel,
    NoProvider,
    ProviderGone,
    EnvelopeMismatch,
    RateLimited,
    KeyUnverified,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_frame_roundtrips() {
        let f = Frame::Open {
            session_id: "s1".into(),
            envelope: Envelope {
                provider: "p@example.com".into(),
                consumer: "c@example.com".into(),
                model: "qwen2.5-coder:32b".into(),
                max_tokens: 2048,
                est_input_tokens: 850,
                payment: Payment::Cashu { token: "cashuB...".into() },
                consumer_keybind: Keybind {
                    x25519_pub: "abc".into(),
                    sig: "sig".into(),
                    not_after: 0,
                },
            },
        };
        let json = serde_json::to_string(&f).unwrap();
        assert!(json.contains("\"type\":\"open\""));
        assert!(json.contains("\"rail\":\"cashu\""));
        let back: Frame = serde_json::from_str(&json).unwrap();
        assert!(matches!(back, Frame::Open { .. }));
    }

    #[test]
    fn error_code_serializes_snake_case() {
        let j = serde_json::to_string(&ErrorCode::EnvelopeMismatch).unwrap();
        assert_eq!(j, "\"envelope_mismatch\"");
    }
}
