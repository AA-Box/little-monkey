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

pub mod ingress;
pub mod policy;
pub mod routing;
pub mod types;
