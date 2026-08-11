//! Provider adapters.
//!
//! One file per provider. Each implements `channel_adapter::ChannelAdapter`
//! (polling and socket transports) or `channel_adapter::WebhookChannelAdapter`
//! (providers that are delivered to), and nothing else: normalization, sending
//! and a health probe. No adapter runs an agent, resolves a route, or decides
//! who may talk — `channel_ingress` owns all of that for every provider at
//! once, which is why a new provider cannot bring its own security posture.

pub(crate) mod discord;
pub(crate) mod google_chat;
pub(crate) mod imessage;
pub(crate) mod irc;
pub(crate) mod line;
pub(crate) mod matrix;
pub(crate) mod mattermost;
pub(crate) mod signal;
pub(crate) mod slack;
pub(crate) mod teams;
pub(crate) mod telegram;
pub(crate) mod whatsapp;
