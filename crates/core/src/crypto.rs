//! End-to-end encryption: Noise IK + identity-bound keys + pinning (spec 04, 02).
//!
//! ## Owner: Codex (Respective Bedbug). Implement the `todo!()` bodies.
//!
//! - Suite: `Noise_IK_25519_ChaChaPoly_BLAKE2s` (use the `snow` crate).
//! - Initiator = consumer, Responder = provider. IK because the initiator
//!   already knows the responder's static key (the pinned X25519 key).
//! - The Noise **prologue** MUST bind the paid terms so the gateway cannot
//!   tamper (spec 04):
//!   `prologue = H(provider_principal || consumer_principal || model || max_tokens || session_id)`.
//!   Use [`prologue`] for a canonical serialization both sides recompute.
//! - Handshake bytes travel as `Hs` frames, relayed opaquely by the gateway.
//! - Transport: ChaCha20-Poly1305, nonces managed by Noise (one key/session ok in v0.1).
//! - MITM defense: the responder static key is the consumer's **pinned** key
//!   (spec 02 keybind). A gateway key substitution yields an unpinned key →
//!   handshake fails. Implement [`verify_keybind`] and enforce pinning above.

use crate::wire::Keybind;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum CryptoError {
    #[error("noise handshake failed: {0}")]
    Handshake(String),
    #[error("pinned key mismatch — possible MITM (spec 02/10 T2)")]
    PinMismatch,
    #[error("keybind signature does not verify against the NUTS identity")]
    BadKeybind,
    #[error("decryption / AEAD failure")]
    Decrypt,
}

/// Canonical prologue binding the encrypted channel to the paid terms (spec 04).
/// Both consumer and provider MUST compute this identically.
pub fn prologue(
    provider_principal: &str,
    consumer_principal: &str,
    model: &str,
    max_tokens: u32,
    session_id: &str,
) -> Vec<u8> {
    // BLAKE2s hash of the canonical concatenation. TODO(Codex): finalize the
    // exact framing (length-prefix each field to avoid ambiguity) and document
    // it here so both proxies agree byte-for-byte.
    let _ = (provider_principal, consumer_principal, model, max_tokens, session_id);
    todo!("canonical, length-prefixed prologue per spec 04")
}

/// Verify a [`Keybind`] signature against the named NUTS principal (spec 02).
/// MUST NOT be forgeable by the gateway. Returns the verified X25519 pubkey bytes.
pub fn verify_keybind(_principal: &str, _keybind: &Keybind) -> Result<[u8; 32], CryptoError> {
    todo!("verify NUTS-identity signature over x25519_pub||principal||not_after")
}

/// A consumer's set of trusted, pinned provider keys (spec 02/08).
/// A pinned provider's key MUST match on every session; a mismatch aborts.
pub trait PinStore {
    /// The pinned X25519 pubkey for a provider, if any.
    fn pinned(&self, provider: &str) -> Option<[u8; 32]>;
    /// Pin (or re-pin on a validly signed rotation) a provider's key.
    fn pin(&mut self, provider: &str, x25519_pub: [u8; 32]);
}

/// One established E2EE session (wraps a `snow` transport state).
/// TODO(Codex): build with `snow::Builder` using the suite + prologue above;
/// `seal`/`open` map to `req`/`res` payload chunks.
pub struct Session {
    _priv: (),
}

impl Session {
    /// Seal a plaintext application message into an opaque ciphertext blob.
    pub fn seal(&mut self, _plaintext: &[u8]) -> Result<Vec<u8>, CryptoError> {
        todo!("Noise transport encrypt")
    }
    /// Open an opaque ciphertext blob back to plaintext.
    pub fn open(&mut self, _ciphertext: &[u8]) -> Result<Vec<u8>, CryptoError> {
        todo!("Noise transport decrypt")
    }
}
