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
    matches!(
        kind,
        ChannelKind::Telegram
            | ChannelKind::WhatsApp
            | ChannelKind::Discord
            | ChannelKind::Slack
            | ChannelKind::Mattermost
            | ChannelKind::Matrix
            | ChannelKind::Signal
    )
}

/// The JSON type one configuration key holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ConfigFieldKind {
    Text,
    Number,
    Boolean,
    TextList,
}

impl ConfigFieldKind {
    fn accepts(self, value: &serde_json::Value) -> bool {
        match self {
            ConfigFieldKind::Text => value.as_str().is_some_and(|text| !text.trim().is_empty()),
            ConfigFieldKind::Number => value.is_number(),
            ConfigFieldKind::Boolean => value.is_boolean(),
            ConfigFieldKind::TextList => value
                .as_array()
                .is_some_and(|items| items.iter().all(serde_json::Value::is_string)),
        }
    }

    fn describe(self) -> &'static str {
        match self {
            ConfigFieldKind::Text => "a non-empty string",
            ConfigFieldKind::Number => "a number",
            ConfigFieldKind::Boolean => "true or false",
            ConfigFieldKind::TextList => "an array of strings",
        }
    }
}

/// One non-secret configuration key an adapter actually reads.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ConfigField {
    pub key: &'static str,
    pub kind: ConfigFieldKind,
    pub required: bool,
}

const fn required(key: &'static str, kind: ConfigFieldKind) -> ConfigField {
    ConfigField {
        key,
        kind,
        required: true,
    }
}

const fn optional(key: &'static str, kind: ConfigFieldKind) -> ConfigField {
    ConfigField {
        key,
        kind,
        required: false,
    }
}

/// Keys every account accepts regardless of provider: the per-account
/// attachment knobs `AttachmentLimits::for_account` reads.
const UNIVERSAL_CONFIG_FIELDS: &[ConfigField] = &[
    optional("max_attachment_bytes", ConfigFieldKind::Number),
    optional("max_attachment_excerpt_chars", ConfigFieldKind::Number),
    optional("max_listed_attachments", ConfigFieldKind::Number),
];

/// The non-secret configuration schema of one provider — exactly the keys its
/// adapter reads, kept next to [`build_adapter`] so a new provider that adds a
/// key has one more place in this file to say so. This is what `channels add`
/// and `channels set-config` validate against; the desktop UI collects the
/// same fields but the daemon is the authority.
pub(crate) fn config_fields(kind: ChannelKind) -> &'static [ConfigField] {
    const NONE: &[ConfigField] = &[];
    const MATTERMOST: &[ConfigField] = &[required("base_url", ConfigFieldKind::Text)];
    const IRC: &[ConfigField] = &[
        required("server", ConfigFieldKind::Text),
        optional("port", ConfigFieldKind::Number),
        required("nick", ConfigFieldKind::Text),
        optional("channels", ConfigFieldKind::TextList),
        optional("use_sasl", ConfigFieldKind::Boolean),
        // The SASL *account*, which is not always the nick — a collision
        // changes the nick and must not change who we authenticate as.
        // Defaults to `nick`, so an account configured before this key existed
        // behaves exactly as it did.
        optional("sasl_username", ConfigFieldKind::Text),
    ];
    const MATRIX: &[ConfigField] = &[
        required("homeserver_url", ConfigFieldKind::Text),
        required("user_id", ConfigFieldKind::Text),
    ];
    const SIGNAL: &[ConfigField] = &[
        required("helper_path", ConfigFieldKind::Text),
        required("account", ConfigFieldKind::Text),
    ];
    // The helper path is required now: the daemon holds no Full Disk Access
    // and sends no Apple events, so an account with no helper has nothing to
    // talk to. `db_path`/`osascript_path` moved with that code — they are the
    // helper's own command-line overrides, not account configuration.
    const IMESSAGE: &[ConfigField] = &[
        required("handle", ConfigFieldKind::Text),
        required("helper_path", ConfigFieldKind::Text),
    ];
    const WHATSAPP: &[ConfigField] = &[required("phone_number_id", ConfigFieldKind::Text)];
    const TEAMS: &[ConfigField] = &[
        required("app_id", ConfigFieldKind::Text),
        required("tenant_id", ConfigFieldKind::Text),
        optional("open_id_metadata_url", ConfigFieldKind::Text),
    ];
    const GOOGLE_CHAT: &[ConfigField] = &[
        required("project_number", ConfigFieldKind::Text),
        optional("bot_user_name", ConfigFieldKind::Text),
    ];
    const SMS: &[ConfigField] = &[
        optional("webhook_public_key", ConfigFieldKind::Text),
        optional("session_scope", ConfigFieldKind::Text),
    ];
    match kind {
        // Secret-only providers: the token carries everything.
        ChannelKind::Telegram | ChannelKind::Discord | ChannelKind::Slack | ChannelKind::Line => {
            NONE
        }
        ChannelKind::Mattermost => MATTERMOST,
        ChannelKind::Irc => IRC,
        ChannelKind::Matrix => MATRIX,
        ChannelKind::Signal => SIGNAL,
        ChannelKind::IMessage => IMESSAGE,
        ChannelKind::WhatsApp => WHATSAPP,
        ChannelKind::Teams => TEAMS,
        ChannelKind::GoogleChat => GOOGLE_CHAT,
        ChannelKind::Sms => SMS,
    }
}

/// Check a non-secret configuration object against what the provider's
/// adapter actually reads: required keys present, every present key known and
/// of the right type. Rejecting an unknown key is the point — a typo'd key
/// otherwise surfaces only as a probe failure with no hint why.
pub(crate) fn validate_non_secret_config(
    kind: ChannelKind,
    config: &serde_json::Value,
) -> Result<(), String> {
    let Some(object) = config.as_object() else {
        return Err("Provider configuration must be a JSON object.".to_string());
    };
    let fields = config_fields(kind);
    for field in fields {
        match object.get(field.key) {
            Some(value) if !field.kind.accepts(value) => {
                return Err(format!(
                    "'{}' must be {}.",
                    field.key,
                    field.kind.describe()
                ));
            }
            None if field.required => {
                return Err(format!(
                    "{} configuration requires '{}'.",
                    kind.label(),
                    field.key
                ));
            }
            _ => {}
        }
    }
    for (key, value) in object {
        let known = fields
            .iter()
            .chain(UNIVERSAL_CONFIG_FIELDS)
            .find(|field| field.key == key.as_str());
        match known {
            Some(field) => {
                if !field.kind.accepts(value) {
                    return Err(format!("'{key}' must be {}.", field.kind.describe()));
                }
            }
            None => {
                let mut known_keys: Vec<&str> = fields
                    .iter()
                    .chain(UNIVERSAL_CONFIG_FIELDS)
                    .map(|field| field.key)
                    .collect();
                known_keys.sort_unstable();
                return Err(format!(
                    "'{key}' is not a {} setting (known settings: {}).",
                    kind.label(),
                    known_keys.join(", ")
                ));
            }
        }
    }
    Ok(())
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
        // see `channel_worker::reconcile_workers`.
        ChannelKind::Sms => {
            return Err("An SMS account is built from its telephony account".to_string())
        }
    })
}

#[cfg(test)]
mod tests {
    use super::super::channel_adapter::AdapterConfig;
    use super::super::channel_store::ChannelAccountRecord;
    use super::*;
    use little_monkey_lib::channels::policy::ChannelAccessPolicy;
    use little_monkey_lib::channels::types::{ChannelHealth, HealthState};

    fn config(kind: ChannelKind, settings: serde_json::Value) -> ChannelAccountRecord {
        ChannelAccountRecord {
            account_id: "acct-1".into(),
            kind,
            label: "Test".into(),
            enabled: true,
            non_secret_config: settings,
            credential_ref: Some("channel:acct-1".into()),
            access_policy: ChannelAccessPolicy::default(),
            health: ChannelHealth {
                state: HealthState::Disconnected,
                detail: None,
                last_error: None,
                probed_at_ms: 1,
            },
            created_at_ms: 1,
            updated_at_ms: 1,
        }
    }

    #[test]
    fn a_required_setting_is_named_when_it_is_missing() {
        let error = validate_non_secret_config(ChannelKind::Mattermost, &serde_json::json!({}))
            .unwrap_err();
        assert!(error.contains("base_url"), "{error}");
    }

    #[test]
    fn a_setting_of_the_wrong_type_is_refused_before_it_is_stored() {
        // The daemon parses `port` as a number, so a quoted one would reach
        // the adapter as a type it cannot use.
        let error = validate_non_secret_config(
            ChannelKind::Irc,
            &serde_json::json!({"server": "irc.example.org", "nick": "monkey", "port": "6697"}),
        )
        .unwrap_err();
        assert!(error.contains("'port'"), "{error}");
        validate_non_secret_config(
            ChannelKind::Irc,
            &serde_json::json!({"server": "irc.example.org", "nick": "monkey", "port": 6697}),
        )
        .expect("a numeric port is what the adapter reads");
    }

    #[test]
    fn an_unknown_key_is_refused_rather_than_silently_ignored() {
        // A typo'd key otherwise surfaces only as a probe failure with no
        // hint why, because the adapter simply never finds the setting.
        let error = validate_non_secret_config(
            ChannelKind::Matrix,
            &serde_json::json!({
                "homeserver_url": "https://matrix.example.org",
                "user_id": "@you:example.org",
                "homserver": "typo",
            }),
        )
        .unwrap_err();
        assert!(error.contains("homserver"), "{error}");
        assert!(error.contains("homeserver_url"), "{error}");
    }

    #[test]
    fn the_per_account_attachment_knobs_are_accepted_everywhere() {
        // Not provider settings, but real keys `AttachmentLimits::for_account`
        // reads. Rejecting them would make an account configured from the
        // terminal uneditable.
        validate_non_secret_config(
            ChannelKind::Telegram,
            &serde_json::json!({"max_attachment_bytes": 1024}),
        )
        .expect("universal key");
    }

    #[test]
    fn an_edited_setting_is_what_the_next_adapter_is_built_from() {
        // What "reconfiguration" means in practice: `reconcile_workers`
        // rebuilds from the stored row when its fingerprint changes, so an
        // account whose settings were fixed becomes runnable without anything
        // else being touched.
        let broken = config(
            ChannelKind::Mattermost,
            serde_json::json!({"base_url": "http://chat.example.com"}),
        );
        let error = match build_adapter(&AdapterConfig {
            account: &broken,
            secret: "token".to_string(),
        }) {
            Err(error) => error,
            Ok(_) => panic!("plain http to a remote host must be refused"),
        };
        assert!(error.contains("https"), "{error}");

        let fixed = config(
            ChannelKind::Mattermost,
            serde_json::json!({"base_url": "https://chat.example.com"}),
        );
        let adapter = build_adapter(&AdapterConfig {
            account: &fixed,
            secret: "token".to_string(),
        })
        .expect("the edited row builds");
        assert_eq!(adapter.kind(), ChannelKind::Mattermost);
    }
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
