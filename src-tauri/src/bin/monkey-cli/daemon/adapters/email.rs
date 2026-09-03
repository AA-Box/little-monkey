//! Email adapter: the operator's own mailbox — IMAP in, SMTP out, implicit TLS
//! on both legs.
//!
//! **This file is a stub.** Construction, capabilities and the transport
//! classification are already the truthful ones; `poll`, `send` and `probe` are
//! not implemented yet and say so rather than reporting a health this adapter
//! has not earned. The shape below is the contract the real implementation
//! fills in, and is documented here so nothing downstream has to guess.
//!
//! # Transport
//!
//! Inbound is a poll, not IMAP IDLE: the worker's own cadence drives
//! `UID FETCH`, and [`InboundTransport::LongPoll`] is therefore the honest
//! classification. IDLE is a latency optimization over a correct poll and
//! would add a background task, a `TransportStatus` and a reconnect ladder for
//! a handful of seconds; if that ever matters, it goes in beside the polling
//! path rather than replacing it. Outbound is a plain SMTP dialogue.
//!
//! # TLS is structural, not configured
//!
//! IMAP and SMTP are not HTTP, so neither leg can go through
//! `egress::hardened()` — the socket is a raw `tokio_rustls` client stream,
//! exactly as `irc.rs` opens one, and hostname verification is rustls's rather
//! than anything hand-written here. There is deliberately no STARTTLS path and
//! no cleartext path: [`EmailAdapter::new`] refuses the cleartext ports (IMAP
//! 143, SMTP 25) by number, so a downgrade is not a configuration mistake an
//! operator can make — it is a state this adapter cannot be built in.
//!
//! # Threading and trust
//!
//! A conversation is one correspondent address, lowercased; the thread is the
//! root of `References` (falling back to `In-Reply-To`, then to the message's
//! own `Message-ID`), and a reply carries both headers so it lands in the same
//! thread. A mailing list or a multi-recipient thread is therefore answered to
//! the sender alone — reply-to-list versus reply-to-sender is a policy decision
//! nobody has made, and guessing it is how a private answer reaches a list.
//!
//! Everything in a message is untrusted: headers, display names, subject and
//! body are provider payload, normalized into a [`ChannelEnvelope`] and never
//! concatenated into instructions. This adapter grants no access — pairing,
//! routing and whether anything runs at all stay `channel_ingress`'s.

use async_trait::async_trait;
use little_monkey_lib::channels::types::{
    ChannelHealth, ChannelKind, InboundTransport, OutboundMessage, ProviderCapabilities,
    SendOutcome,
};

use crate::daemon::channel_adapter::{AdapterConfig, ChannelAdapter, InboundBatch};

/// What one outbound mail may carry. Nothing outside this file reads
/// `ProviderCapabilities::max_text_chars` — there is no worker-side chunker —
/// so this is a declaration to the agent's tool schema and the setup UI, and
/// `send` is what has to honour it.
const EMAIL_MAX_TEXT_CHARS: usize = 16_384;

/// Ports whose only protocol is cleartext. Refused by number so there is no
/// downgrade to negotiate away.
const IMAP_CLEARTEXT_PORT: u16 = 143;
const SMTP_CLEARTEXT_PORT: u16 = 25;

const DEFAULT_IMAP_PORT: u16 = 993;
const DEFAULT_SMTP_PORT: u16 = 465;

pub struct EmailAdapter {
    account_id: String,
    #[allow(dead_code)]
    imap: Endpoint,
    #[allow(dead_code)]
    smtp: Endpoint,
    #[allow(dead_code)]
    username: String,
    #[allow(dead_code)]
    from_address: String,
    #[allow(dead_code)]
    mailbox: String,
}

#[allow(dead_code)]
pub(crate) struct Endpoint {
    pub host: String,
    pub port: u16,
}

fn text(config: &AdapterConfig<'_>, key: &str) -> Option<String> {
    config
        .account
        .non_secret_config
        .get(key)
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

fn port(config: &AdapterConfig<'_>, key: &str, default: u16) -> Result<u16, String> {
    match config.account.non_secret_config.get(key) {
        None | Some(serde_json::Value::Null) => Ok(default),
        Some(value) => value
            .as_u64()
            .filter(|number| *number > 0 && *number <= u64::from(u16::MAX))
            .map(|number| number as u16)
            .ok_or_else(|| format!("Email '{key}' must be a port number")),
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(0)
}

impl EmailAdapter {
    pub fn new(config: &AdapterConfig<'_>) -> Result<Self, String> {
        let required = |key: &str| {
            text(config, key).ok_or_else(|| format!("Email configuration requires '{key}'"))
        };
        let imap = Endpoint {
            host: required("imap_host")?,
            port: port(config, "imap_port", DEFAULT_IMAP_PORT)?,
        };
        let smtp = Endpoint {
            host: required("smtp_host")?,
            port: port(config, "smtp_port", DEFAULT_SMTP_PORT)?,
        };
        // Refused here rather than checked at connect time: there is no
        // cleartext code path in this adapter to reach, and an account that
        // cannot be built is a mistake an operator sees at once.
        if imap.port == IMAP_CLEARTEXT_PORT {
            return Err(format!(
                "Email imap_port {IMAP_CLEARTEXT_PORT} is the cleartext IMAP port; this adapter \
                 only speaks implicit TLS (usually {DEFAULT_IMAP_PORT})"
            ));
        }
        if smtp.port == SMTP_CLEARTEXT_PORT {
            return Err(format!(
                "Email smtp_port {SMTP_CLEARTEXT_PORT} is the cleartext SMTP port; this adapter \
                 only speaks implicit TLS (usually {DEFAULT_SMTP_PORT})"
            ));
        }
        Ok(Self {
            account_id: config.account.account_id.clone(),
            imap,
            smtp,
            username: required("username")?,
            from_address: required("from_address")?,
            mailbox: text(config, "mailbox").unwrap_or_else(|| "INBOX".to_string()),
        })
    }
}

#[async_trait]
impl ChannelAdapter for EmailAdapter {
    fn kind(&self) -> ChannelKind {
        ChannelKind::Email
    }

    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            max_text_chars: EMAIL_MAX_TEXT_CHARS,
            supports_threads: true,
            supports_attachments: true,
            // Mail carries no mention metadata at all; gating falls back to
            // substring matching, which is what `false` tells the ingress.
            supports_mention_metadata: false,
            ..ProviderCapabilities::minimal(ChannelKind::Email, InboundTransport::LongPoll)
        }
    }

    async fn probe(&self) -> ChannelHealth {
        ChannelHealth::error(
            now_ms(),
            format!(
                "The email adapter is not implemented yet, so account {} cannot reach a mailbox.",
                self.account_id
            ),
        )
    }

    async fn poll(&self, _cursor: Option<&str>) -> Result<InboundBatch, String> {
        Ok(InboundBatch::default())
    }

    async fn send(&self, _message: &OutboundMessage) -> SendOutcome {
        SendOutcome::PermanentFailure {
            error: "The email adapter is not implemented yet".to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::channel_store::ChannelAccountRecord;
    use little_monkey_lib::channels::policy::ChannelAccessPolicy;
    use little_monkey_lib::channels::types::{ChannelHealth as Health, HealthState};

    fn account(settings: serde_json::Value) -> ChannelAccountRecord {
        ChannelAccountRecord {
            account_id: "chan-email".into(),
            kind: ChannelKind::Email,
            label: "Mailbox".into(),
            enabled: true,
            non_secret_config: settings,
            credential_ref: Some("channel:chan-email".into()),
            access_policy: ChannelAccessPolicy::default(),
            health: Health {
                state: HealthState::Disconnected,
                detail: None,
                last_error: None,
                probed_at_ms: 1,
            },
            created_at_ms: 1,
            updated_at_ms: 1,
        }
    }

    fn valid() -> serde_json::Value {
        serde_json::json!({
            "imap_host": "imap.example.org",
            "smtp_host": "smtp.example.org",
            "username": "you@example.org",
            "from_address": "you@example.org",
        })
    }

    fn build_error(settings: serde_json::Value) -> String {
        build(settings).err().expect("expected a refusal")
    }

    fn build(settings: serde_json::Value) -> Result<EmailAdapter, String> {
        let record = account(settings);
        EmailAdapter::new(&AdapterConfig {
            account: &record,
            secret: "{\"imap_password\":\"pw\"}".to_string(),
        })
    }

    #[test]
    fn a_cleartext_port_is_refused_at_construction() {
        // Not a runtime check that could be bypassed by a server offering
        // STARTTLS: there is no cleartext socket in this adapter to open.
        let mut settings = valid();
        settings["imap_port"] = serde_json::json!(143);
        let error = build_error(settings);
        assert!(error.contains("implicit TLS"), "{error}");

        let mut settings = valid();
        settings["smtp_port"] = serde_json::json!(25);
        let error = build_error(settings);
        assert!(error.contains("implicit TLS"), "{error}");
    }

    #[test]
    fn the_tls_ports_are_the_defaults() {
        let adapter = build(valid()).ok().expect("valid mailbox");
        assert_eq!(adapter.imap.port, DEFAULT_IMAP_PORT);
        assert_eq!(adapter.smtp.port, DEFAULT_SMTP_PORT);
        assert_eq!(adapter.mailbox, "INBOX");
    }

    #[test]
    fn attachments_and_threads_are_declared_because_both_halves_are_the_plan() {
        let capabilities = build(valid()).ok().expect("valid mailbox").capabilities();
        assert!(capabilities.supports_threads);
        assert!(capabilities.supports_attachments);
        assert!(!capabilities.supports_mention_metadata);
        assert_eq!(capabilities.inbound_transport, InboundTransport::LongPoll);
    }
}
