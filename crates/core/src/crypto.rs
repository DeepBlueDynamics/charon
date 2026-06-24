//! End-to-end encryption: Noise IK + identity-bound keys + pinning (spec 04, 02).
//!
//! ## Owner: Codex (Respective Bedbug).
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
use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
use blake2::{Blake2s256, Digest};
use snow::{params::NoiseParams, Builder, HandshakeState, TransportState};
use std::{
    collections::HashMap,
    time::{SystemTime, UNIX_EPOCH},
};
use thiserror::Error;
use x25519_dalek::{PublicKey, StaticSecret};

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
    #[error("invalid key material: {0}")]
    InvalidKey(String),
}

const NOISE_PATTERN: &str = "Noise_IK_25519_ChaChaPoly_BLAKE2s";
const TAG_LEN: usize = 16;

/// Canonical prologue binding the encrypted channel to the paid terms (spec 04).
/// Both consumer and provider MUST compute this identically.
pub fn prologue(
    provider_principal: &str,
    consumer_principal: &str,
    model: &str,
    max_tokens: u32,
    session_id: &str,
) -> Vec<u8> {
    // Framing is:
    // b"charon-prologue-v1" ||
    // u32be(len(provider_principal)) || provider_principal UTF-8 bytes ||
    // u32be(len(consumer_principal)) || consumer_principal UTF-8 bytes ||
    // u32be(len(model))              || model UTF-8 bytes ||
    // max_tokens as u32be ||
    // u32be(len(session_id))         || session_id UTF-8 bytes.
    // The returned Noise prologue is BLAKE2s-256 over those bytes.
    let mut framed = Vec::new();
    framed.extend_from_slice(b"charon-prologue-v1");
    push_len_prefixed(&mut framed, provider_principal.as_bytes());
    push_len_prefixed(&mut framed, consumer_principal.as_bytes());
    push_len_prefixed(&mut framed, model.as_bytes());
    framed.extend_from_slice(&max_tokens.to_be_bytes());
    push_len_prefixed(&mut framed, session_id.as_bytes());
    Blake2s256::digest(&framed).to_vec()
}

/// Verify a [`Keybind`] signature against the named NUTS principal (spec 02).
/// MUST NOT be forgeable by the gateway. Returns the verified X25519 pubkey bytes.
use secp256k1::{Secp256k1, Message, Keypair, XOnlyPublicKey};
use secp256k1::hashes::{sha256, Hash};

/// Sign a keybind using a Nostr secret key (BIP340 Schnorr signature).
pub fn sign_keybind(
    x25519_pub: [u8; 32],
    principal: &str,
    not_after: u64,
    nostr_secret: [u8; 32],
) -> Keybind {
    let mut msg_bytes = Vec::new();
    msg_bytes.extend_from_slice(&x25519_pub);
    msg_bytes.extend_from_slice(principal.as_bytes());
    msg_bytes.extend_from_slice(&not_after.to_le_bytes());

    let msg_hash = sha256::Hash::hash(&msg_bytes);
    let message = Message::from_digest(msg_hash.to_byte_array());

    let secp = Secp256k1::new();
    let keypair = Keypair::from_seckey_slice(&secp, &nostr_secret)
        .expect("Invalid Nostr secret key");

    let sig = secp.sign_schnorr_no_aux_rand(&message, &keypair);
    let sig_hex = format!("{:x}", sig);

    Keybind {
        x25519_pub: BASE64.encode(x25519_pub),
        sig: sig_hex,
        not_after,
    }
}

/// Verify a [`Keybind`] signature against the named NUTS principal's Nostr X-only public key.
pub fn verify_keybind(
    kb: &Keybind,
    principal: &str,
    nostr_xonly_pub: [u8; 32],
) -> bool {
    if kb.not_after != 0 {
        let now = match SystemTime::now().duration_since(UNIX_EPOCH) {
            Ok(d) => d.as_secs(),
            Err(_) => return false,
        };
        if now > kb.not_after {
            return false;
        }
    }

    let x25519_pub_bytes = match BASE64.decode(&kb.x25519_pub) {
        Ok(b) => b,
        Err(_) => return false,
    };
    if x25519_pub_bytes.len() != 32 {
        return false;
    }
    let mut x25519_pub = [0u8; 32];
    x25519_pub.copy_from_slice(&x25519_pub_bytes);

    let mut msg_bytes = Vec::new();
    msg_bytes.extend_from_slice(&x25519_pub);
    msg_bytes.extend_from_slice(principal.as_bytes());
    msg_bytes.extend_from_slice(&kb.not_after.to_le_bytes());

    let msg_hash = sha256::Hash::hash(&msg_bytes);
    let message = Message::from_digest(msg_hash.to_byte_array());

    let pubkey = match XOnlyPublicKey::from_slice(&nostr_xonly_pub) {
        Ok(pk) => pk,
        Err(_) => return false,
    };

    let sig = match kb.sig.parse::<secp256k1::schnorr::Signature>() {
        Ok(s) => s,
        Err(_) => return false,
    };

    let secp = Secp256k1::new();
    secp.verify_schnorr(&sig, &message, &pubkey).is_ok()
}

/// A consumer's set of trusted, pinned provider keys (spec 02/08).
/// A pinned provider's key MUST match on every session; a mismatch aborts.
pub trait PinStore {
    /// The pinned X25519 pubkey for a provider, if any.
    fn pinned(&self, provider: &str) -> Option<[u8; 32]>;
    /// Pin (or re-pin on a validly signed rotation) a provider's key.
    fn pin(&mut self, provider: &str, x25519_pub: [u8; 32]);
}

#[derive(Debug, Clone, Default)]
pub struct SimplePinStore {
    pins: HashMap<String, [u8; 32]>,
}

impl SimplePinStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn verify_or_pin(&mut self, provider: &str, x25519_pub: [u8; 32]) -> Result<(), CryptoError> {
        match self.pinned(provider) {
            Some(pinned) if pinned == x25519_pub => Ok(()),
            Some(_) => Err(CryptoError::PinMismatch),
            None => {
                self.pin(provider, x25519_pub);
                Ok(())
            }
        }
    }
}

impl PinStore for SimplePinStore {
    fn pinned(&self, provider: &str) -> Option<[u8; 32]> {
        self.pins.get(provider).copied()
    }

    fn pin(&mut self, provider: &str, x25519_pub: [u8; 32]) {
        self.pins.insert(provider.to_string(), x25519_pub);
    }
}

/// One established E2EE session (wraps a `snow` transport state).
pub struct Session {
    transport: TransportState,
}

impl Session {
    /// Seal a plaintext application message into an opaque ciphertext blob.
    pub fn seal(&mut self, plaintext: &[u8]) -> Result<Vec<u8>, CryptoError> {
        let mut out = vec![0; plaintext.len() + TAG_LEN];
        let len = self
            .transport
            .write_message(plaintext, &mut out)
            .map_err(|err| CryptoError::Handshake(err.to_string()))?;
        out.truncate(len);
        Ok(out)
    }

    /// Open an opaque ciphertext blob back to plaintext.
    pub fn open(&mut self, ciphertext: &[u8]) -> Result<Vec<u8>, CryptoError> {
        let mut out = vec![0; ciphertext.len()];
        let len = self
            .transport
            .read_message(ciphertext, &mut out)
            .map_err(|_| CryptoError::Decrypt)?;
        out.truncate(len);
        Ok(out)
    }
}

pub struct InitiatorHandshake {
    state: HandshakeState,
}

impl InitiatorHandshake {
    pub fn first_message(&mut self) -> Result<Vec<u8>, CryptoError> {
        write_handshake(&mut self.state, &[])
    }

    pub fn finish(mut self, responder_message: &[u8]) -> Result<Session, CryptoError> {
        read_handshake(&mut self.state, responder_message)?;
        into_session(self.state)
    }
}

pub struct ResponderHandshake {
    state: HandshakeState,
}

impl ResponderHandshake {
    pub fn respond(mut self, initiator_message: &[u8]) -> Result<(Vec<u8>, Session), CryptoError> {
        read_handshake(&mut self.state, initiator_message)?;
        let response = write_handshake(&mut self.state, &[])?;
        let session = into_session(self.state)?;
        Ok((response, session))
    }
}

pub fn initiator_handshake(
    local_static_private: &[u8; 32],
    remote_static_public: &[u8; 32],
    prologue: &[u8],
) -> Result<InitiatorHandshake, CryptoError> {
    Ok(InitiatorHandshake {
        state: builder(prologue)
            .local_private_key(local_static_private)
            .remote_public_key(remote_static_public)
            .build_initiator()
            .map_err(|err| CryptoError::Handshake(err.to_string()))?,
    })
}

pub fn responder_handshake(
    local_static_private: &[u8; 32],
    prologue: &[u8],
) -> Result<ResponderHandshake, CryptoError> {
    Ok(ResponderHandshake {
        state: builder(prologue)
            .local_private_key(local_static_private)
            .build_responder()
            .map_err(|err| CryptoError::Handshake(err.to_string()))?,
    })
}

pub fn public_from_private(private: &[u8; 32]) -> [u8; 32] {
    let secret = StaticSecret::from(*private);
    *PublicKey::from(&secret).as_bytes()
}

fn push_len_prefixed(out: &mut Vec<u8>, value: &[u8]) {
    out.extend_from_slice(&(value.len() as u32).to_be_bytes());
    out.extend_from_slice(value);
}

fn keybind_message(x25519_pub: &[u8; 32], principal: &str, not_after: u64) -> Vec<u8> {
    let mut message = Vec::with_capacity(32 + principal.len() + 8);
    message.extend_from_slice(x25519_pub);
    message.extend_from_slice(principal.as_bytes());
    message.extend_from_slice(&not_after.to_be_bytes());
    message
}

fn builder(prologue: &[u8]) -> Builder<'_> {
    let params: NoiseParams = NOISE_PATTERN.parse().expect("static Noise params are valid");
    Builder::new(params).prologue(prologue)
}

fn write_handshake(state: &mut HandshakeState, payload: &[u8]) -> Result<Vec<u8>, CryptoError> {
    let mut out = vec![0; 512 + payload.len()];
    let len = state
        .write_message(payload, &mut out)
        .map_err(|err| CryptoError::Handshake(err.to_string()))?;
    out.truncate(len);
    Ok(out)
}

fn read_handshake(state: &mut HandshakeState, message: &[u8]) -> Result<Vec<u8>, CryptoError> {
    let mut out = vec![0; message.len()];
    let len = state
        .read_message(message, &mut out)
        .map_err(|err| CryptoError::Handshake(err.to_string()))?;
    out.truncate(len);
    Ok(out)
}

fn into_session(state: HandshakeState) -> Result<Session, CryptoError> {
    Ok(Session {
        transport: state
            .into_transport_mode()
            .map_err(|err| CryptoError::Handshake(err.to_string()))?,
    })
}
