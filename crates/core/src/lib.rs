//! charon-core — shared contract for the Charon encrypted inference marketplace.
//!
//! This crate is the load-bearing interface every Charon component is built
//! against. It is the spec (see `/spec`) made executable:
//!
//! - [`wire`]    — control frames + the cleartext routing envelope (spec 03).
//! - [`payment`] — pricing, the gateway cut, settlement units (spec 05).
//! - [`crypto`]  — Noise IK session, identity-bound key bindings, pinning (spec 04, 02).
//! - [`auth`]    — NUTS token validation against `auth.nuts.services` (spec 02).
//!
//! Where this crate and the spec disagree, the spec wins — fix the code.

pub mod auth;
pub mod crypto;
pub mod payment;
pub mod wire;

pub use wire::{Envelope, ErrorCode, Frame, Keybind, ModelCard, Payment, Usage};
