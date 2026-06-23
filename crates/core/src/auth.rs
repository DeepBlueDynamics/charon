//! NUTS token validation against `auth.nuts.services` (spec 02).
//!
//! ## Owner: Codex (Respective Bedbug). Implement the `todo!()` bodies.
//!
//! nuts-auth is Python/FastAPI; validate by token shape:
//!
//! | Token   | Endpoint            | Request                         | Principal              |
//! |---------|---------------------|---------------------------------|------------------------|
//! | `ahp_…` | `POST /api/validate`| JSON `{"token":"ahp_…"}`         | `subject` in response  |
//! | `eyJ…`  | `GET  /api/verify`  | `Authorization: Bearer <jwt>`   | `sub` claim (email)    |
//!
//! `POST /api/validate` returns `{"valid":bool,"subject":email,"actor":..,..}`.
//! `GET /api/verify` returns the decoded JWT claims (RS256, kid=nuts-auth-key-1).
//! Prefer `/api/validate` for `ahp_` daemon tokens (one round trip).
//! `DISABLE_AUTH=true` short-circuits to a fixed dev principal — test only.

use thiserror::Error;

/// A NUTS principal: the email from the token claims (`sub` / `subject`).
pub type Principal = String;

#[derive(Debug, Error)]
pub enum AuthError {
    #[error("token rejected by nuts-auth")]
    Invalid,
    #[error("auth service unreachable: {0}")]
    Transport(String),
}

/// Token shape (spec 02): daemons use `ahp_`, the dashboard uses a browser JWT.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenKind {
    /// `ahp_<opaque>` API token.
    Ahp,
    /// `eyJ…` browser JWT (three dot-separated parts).
    Jwt,
}

impl TokenKind {
    /// Classify a token by shape without contacting the auth service.
    pub fn of(token: &str) -> TokenKind {
        if token.starts_with("ahp_") {
            TokenKind::Ahp
        } else {
            TokenKind::Jwt
        }
    }
}

/// Validates NUTS tokens against an `auth.nuts.services`-compatible service.
#[derive(Clone)]
pub struct NutsAuth {
    /// Base URL, e.g. `https://auth.nuts.services` (env `GNOSIS_AUTH_URL`).
    pub auth_url: String,
    /// When true, skip validation and return [`NutsAuth::dev_principal`].
    /// MUST be false on a public deployment (spec 02).
    pub disable_auth: bool,
    http: reqwest::Client,
}

impl NutsAuth {
    pub fn new(auth_url: impl Into<String>, disable_auth: bool) -> Self {
        Self {
            auth_url: auth_url.into(),
            disable_auth,
            http: reqwest::Client::new(),
        }
    }

    /// Principal returned when `disable_auth` is set.
    pub const fn dev_principal() -> &'static str {
        "dev@charon.local"
    }

    /// Validate `token` and return its principal (email).
    ///
    /// TODO(Codex): if `disable_auth`, return [`Self::dev_principal`]. Else
    /// branch on [`TokenKind::of`]: POST `{auth_url}/api/validate` for `ahp_`
    /// (read `subject`), GET `{auth_url}/api/verify` for JWTs (read `sub`).
    pub async fn validate(&self, _token: &str) -> Result<Principal, AuthError> {
        let _ = &self.http;
        todo!("NUTS token validation — see module docs and spec 02")
    }
}
