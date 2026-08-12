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
pub(crate) mod jwt;
pub(crate) mod line;
pub(crate) mod matrix;
pub(crate) mod mattermost;
pub(crate) mod signal;
pub(crate) mod slack;
pub(crate) mod sms;
pub(crate) mod teams;
pub(crate) mod telegram;
pub(crate) mod whatsapp;

use std::sync::Arc;

use little_monkey_lib::channels::types::ChannelKind;

use super::channel_adapter::{AdapterConfig, ChannelAdapter};

/// Whether Little Monkey can actually upload a file to this provider.
///
/// The one source of truth for the `supports_attachments` capability, so the
/// flag means "this adapter implements the upload" rather than "the provider
/// has a file API". A provider whose API supports files but whose adapter does
/// not send them belongs on the false side: the difference is invisible to an
/// operator, who only sees whether the file arrived.
pub(crate) fn sends_attachments(kind: ChannelKind) -> bool {
    matches!(kind, ChannelKind::Telegram | ChannelKind::WhatsApp)
}

/// Build the adapter an account's provider needs.
///
/// The one place a `ChannelKind` becomes code. Exhaustive on purpose: a new
/// provider kind fails to compile here until somebody decides what it maps to,
/// rather than silently becoming an account that never runs.
pub(crate) fn build_adapter(config: &AdapterConfig<'_>) -> Result<Arc<dyn ChannelAdapter>, String> {
    Ok(match config.account.kind {
        ChannelKind::Telegram => Arc::new(telegram::TelegramAdapter::new(config)?),
        ChannelKind::Discord => Arc::new(discord::DiscordAdapter::new(config)?),
        ChannelKind::Slack => Arc::new(slack::SlackAdapter::new(config)?),
        ChannelKind::Mattermost => Arc::new(mattermost::MattermostAdapter::new(config)?),
        ChannelKind::Irc => Arc::new(irc::IrcAdapter::new(config)?),
        ChannelKind::WhatsApp => Arc::new(whatsapp::WhatsAppAdapter::new(config)?),
        ChannelKind::Line => Arc::new(line::LineAdapter::new(config)?),
        ChannelKind::Teams => Arc::new(teams::TeamsAdapter::new(config)?),
        ChannelKind::GoogleChat => Arc::new(google_chat::GoogleChatAdapter::new(config)?),
        ChannelKind::Matrix => Arc::new(matrix::MatrixAdapter::new(config)?),
        ChannelKind::Signal => Arc::new(signal::SignalAdapter::new(config)?),
        ChannelKind::IMessage => Arc::new(imessage::ImessageAdapter::new(config)?),
        // SMS is the one provider whose credential lives on a telephony account
        // rather than a channel account, so it is built from that row instead —
        // see `channel_worker::load_adapters`.
        ChannelKind::Sms => {
            return Err("An SMS account is built from its telephony account".to_string())
        }
    })
}

/// Build the delivered-to adapter an account's provider needs.
///
/// Separate from [`build_adapter`] because the two halves answer different
/// questions: this one is asked "who signed this body", and only the four
/// providers that are delivered to can answer it at all.
pub(crate) fn build_webhook_adapter(
    config: &AdapterConfig<'_>,
) -> Result<Box<dyn super::channel_adapter::WebhookChannelAdapter>, String> {
    Ok(match config.account.kind {
        ChannelKind::WhatsApp => Box::new(whatsapp::WhatsAppAdapter::new(config)?),
        ChannelKind::Line => Box::new(line::LineAdapter::new(config)?),
        ChannelKind::Teams => Box::new(teams::TeamsAdapter::new(config)?),
        ChannelKind::GoogleChat => Box::new(google_chat::GoogleChatAdapter::new(config)?),
        other => {
            return Err(format!(
                "{} is not delivered to over a webhook",
                other.label()
            ))
        }
    })
}
