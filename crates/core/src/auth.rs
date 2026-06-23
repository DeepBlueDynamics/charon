//! NUTS token validation against `auth.nuts.services` (spec 02).
//!
//! ## Owner: Codex (Respective Bedbug).
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
    pub async fn validate(&self, token: &str) -> Result<Principal, AuthError> {
        if self.disable_auth {
            return Ok(Self::dev_principal().to_string());
        }

        match TokenKind::of(token) {
            TokenKind::Ahp => {
                #[derive(serde::Serialize)]
                struct ValidateRequest<'a> {
                    token: &'a str,
                }

                #[derive(serde::Deserialize)]
                struct ValidateResponse {
                    valid: bool,
                    subject: Option<String>,
                }

                let url = format!("{}/api/validate", self.auth_url.trim_end_matches('/'));
                let response = self
                    .http
                    .post(url)
                    .json(&ValidateRequest { token })
                    .send()
                    .await
                    .map_err(|err| AuthError::Transport(err.to_string()))?;

                if !response.status().is_success() {
                    return Err(AuthError::Invalid);
                }

                let body: ValidateResponse = response
                    .json()
                    .await
                    .map_err(|err| AuthError::Transport(err.to_string()))?;
                match (body.valid, body.subject) {
                    (true, Some(subject)) if !subject.is_empty() => Ok(subject),
                    _ => Err(AuthError::Invalid),
                }
            }
            TokenKind::Jwt => {
                #[derive(serde::Deserialize)]
                struct Claims {
                    sub: Option<String>,
                }

                let url = format!("{}/api/verify", self.auth_url.trim_end_matches('/'));
                let response = self
                    .http
                    .get(url)
                    .bearer_auth(token)
                    .send()
                    .await
                    .map_err(|err| AuthError::Transport(err.to_string()))?;

                if !response.status().is_success() {
                    return Err(AuthError::Invalid);
                }

                let claims: Claims = response
                    .json()
                    .await
                    .map_err(|err| AuthError::Transport(err.to_string()))?;
                claims.sub.filter(|sub| !sub.is_empty()).ok_or(AuthError::Invalid)
            }
        }
    }
}
