//! Provider-independent messaging subsystem.
//!
//! One core, many adapters. Everything Little Monkey knows about a messaging
//! provider is confined to an adapter that translates the provider's wire format
//! into [`types::ChannelEnvelope`] on the way in and back out of
//! [`types::OutboundMessage`] on the way out. Adapters never execute an agent,
//! never resolve a route, and never decide who is allowed to talk.
//!
//! The pipeline an inbound message travels:
//!
//! ```text
//! provider -> adapter (normalize) -> durable dedupe -> access + activation
//!          -> route resolution -> durable ingress record -> normal agent run
//! ```
//!
//! and on the way back:
//!
//! ```text
//! run result -> durable outbox -> adapter send -> delivery receipt
//! ```
//!
//! Modules here are deliberately I/O-free where they can be: [`policy`] and
//! [`routing`] are pure decision functions, which is what makes the security
//! properties (pairing, mention gating, loop bounds, route determinism) testable
//! without a provider.

use std::sync::LazyLock;

/// Keychain service every messaging credential is stored under.
///
/// Defined here rather than beside the adapters because two processes write it:
/// the daemon reads a credential to build an adapter, and the desktop stores one
/// when the operator pastes it in. Two definitions would eventually become two
/// different services, and the symptom would be a token that saves and can
/// never be read back.
pub static KEYCHAIN_SERVICE: LazyLock<String> =
    LazyLock::new(|| crate::profiles::keychain_service("com.littlemonkey.channels"));

/// Keychain account name for one messaging account's credential.
pub fn credential_ref(account_id: &str) -> String {
    format!("channel:{account_id}")
}

pub mod ingress;
pub mod policy;
pub mod routing;
pub mod types;
