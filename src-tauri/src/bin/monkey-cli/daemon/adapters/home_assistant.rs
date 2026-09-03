//! Home Assistant adapter: the operator's own instance — inbound over the
//! WebSocket API, outbound through the REST notify service.
//!
//! **This file is a stub.** `base_url`/`notify_service`/`event_type`
//! validation, the capability declaration and the transport classification are
//! already the real ones; the socket, the send and the probe are not
//! implemented yet and say so rather than reporting a health this adapter has
//! not earned.
//!
//! # Why the WebSocket API and not the webhook trigger
//!
//! Home Assistant can also drive a webhook trigger, but that needs a publicly
//! reachable callback through `callback_exposure`, a verification challenge and
//! Security Doctor's whole set of callback questions. `/api/websocket` is an
//! *outbound* connection from this machine and needs none of them, so it is
//! what [`inbound_transport_for`](super::inbound_transport_for) classifies this
//! kind as: `Socket`. One configured event type is subscribed to, and every
//! frame of any other type is ignored.
//!
//! # Trust boundary
//!
//! The long-lived access token lives in the OS keychain and reaches this
//! adapter already resolved; it is never in the account's configuration JSON.
//! `base_url` is pinned the way Mattermost's is — a bare origin, `https` unless
//! the host is loopback — because a bearer token goes out on every request made
//! to whatever that string names. `notify_service` is concatenated into a REST
//! path and `event_type` into a subscription, so both are validated as bare
//! identifiers rather than merely non-empty.
//!
//! Everything an event carries is untrusted operator-automation payload. This
//! adapter decides nothing about access: `channel_ingress` does, for this
//! provider exactly as for every other.
//!
//! # What it deliberately does not do
//!
//! A Home Assistant notify service has no upload and no per-recipient address:
//! a reply goes wherever that one service is configured to go, and no file can
//! ride along. `sends_attachments(HomeAssistant)` is therefore `false`, and the
//! send tool refuses a file rather than queueing a reply that would arrive
//! without it.

use async_trait::async_trait;
use little_monkey_lib::channels::types::{
    ChannelHealth, ChannelKind, InboundTransport, OutboundMessage, ProviderCapabilities,
    SendOutcome,
};

use crate::daemon::channel_adapter::{AdapterConfig, ChannelAdapter, InboundBatch};

/// The event type an account subscribes to when it names none. A Home
/// Assistant automation fires this with `event_type: little_monkey_message`.
const DEFAULT_EVENT_TYPE: &str = "little_monkey_message";

/// Home Assistant imposes no message length on `notify`; this is the host's own
/// chunking ceiling, chosen to be comfortably under what a push transport
/// behind a notify service will carry.
const HOME_ASSISTANT_MAX_TEXT_CHARS: usize = 4_000;

pub struct HomeAssistantAdapter {
    account_id: String,
    #[allow(dead_code)]
    base_url: String,
    #[allow(dead_code)]
    notify_service: String,
    #[allow(dead_code)]
    event_type: String,
    #[allow(dead_code)]
    token: String,
}

/// A bare origin, and `https` unless the host is this machine.
///
/// Same rule and same reason as Mattermost's: a long-lived access token is
/// attached to every request this adapter makes to whatever `base_url` names,
/// so a plain-`http` remote host would walk it across the network in the clear.
/// A path or a query string is refused because the adapter appends its own.
pub(crate) fn validate_base_url(raw: &str) -> Result<String, String> {
    let parsed = url::Url::parse(raw)
        .map_err(|_| "Home Assistant base_url is not a valid URL".to_string())?;
    if !matches!(parsed.path(), "" | "/") {
        return Err("Home Assistant base_url must not include a path".to_string());
    }
    if parsed.query().is_some() {
        return Err("Home Assistant base_url must not include a query string".to_string());
    }
    match parsed.scheme() {
        "https" => {}
        "http" => {
            let is_local = matches!(
                parsed.host_str(),
                Some("localhost") | Some("127.0.0.1") | Some("::1")
            );
            if !is_local {
                return Err(
                    "Home Assistant base_url must be https (plain http is only accepted for \
                     localhost). A stock http://homeassistant.local:8123 install has to be put \
                     behind TLS first, or the long-lived access token rides your network in the \
                     clear."
                        .to_string(),
                );
            }
        }
        _ => return Err("Home Assistant base_url must use http or https".to_string()),
    }
    Ok(raw.trim_end_matches('/').to_string())
}

/// A bare Home Assistant identifier: lowercase letters, digits, underscore.
///
/// Applied to `notify_service`, which is concatenated into
/// `/api/services/notify/<service>`, and to `event_type`, which names the
/// subscription. Refusing `/` and `.` here is what keeps a configuration string
/// from selecting a different endpoint.
pub(crate) fn validate_identifier(kind: &str, raw: &str) -> Result<String, String> {
    let value = raw.trim();
    if value.is_empty() || value.len() > 128 {
        return Err(format!("Home Assistant {kind} must be 1-128 characters"));
    }
    if !value.chars().all(|character| {
        character.is_ascii_lowercase() || character.is_ascii_digit() || character == '_'
    }) {
        return Err(format!(
            "Home Assistant {kind} must be a bare name of lowercase letters, digits and \
             underscores (got '{value}')"
        ));
    }
    Ok(value.to_string())
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(0)
}

impl HomeAssistantAdapter {
    pub fn new(config: &AdapterConfig<'_>) -> Result<Self, String> {
        let setting = |key: &str| {
            config
                .account
                .non_secret_config
                .get(key)
                .and_then(serde_json::Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_string)
        };
        let base_url = validate_base_url(
            &setting("base_url")
                .ok_or_else(|| "Home Assistant configuration requires 'base_url'".to_string())?,
        )?;
        let notify_service = validate_identifier(
            "notify_service",
            &setting("notify_service").ok_or_else(|| {
                "Home Assistant configuration requires 'notify_service'".to_string()
            })?,
        )?;
        let event_type = validate_identifier(
            "event_type",
            &setting("event_type").unwrap_or_else(|| DEFAULT_EVENT_TYPE.to_string()),
        )?;
        let token = config.secret.trim().to_string();
        if token.is_empty() {
            return Err(
                "Home Assistant needs a long-lived access token; store it with \
                 `monkey channels set-token`"
                    .to_string(),
            );
        }
        Ok(Self {
            account_id: config.account.account_id.clone(),
            base_url,
            notify_service,
            event_type,
            token,
        })
    }
}

#[async_trait]
impl ChannelAdapter for HomeAssistantAdapter {
    fn kind(&self) -> ChannelKind {
        ChannelKind::HomeAssistant
    }

    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            max_text_chars: HOME_ASSISTANT_MAX_TEXT_CHARS,
            ..ProviderCapabilities::minimal(ChannelKind::HomeAssistant, InboundTransport::Socket)
        }
    }

    async fn probe(&self) -> ChannelHealth {
        ChannelHealth::error(
            now_ms(),
            format!(
                "The Home Assistant adapter is not implemented yet, so account {} cannot reach an \
                 instance.",
                self.account_id
            ),
        )
    }

    async fn poll(&self, _cursor: Option<&str>) -> Result<InboundBatch, String> {
        Ok(InboundBatch::default())
    }

    async fn send(&self, _message: &OutboundMessage) -> SendOutcome {
        SendOutcome::PermanentFailure {
            error: "The Home Assistant adapter is not implemented yet".to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_base_url_that_could_walk_the_token_somewhere_else_is_refused() {
        for raw in [
            "https://ha.example.org/api",
            "https://ha.example.org/?token=x",
            "http://192.168.1.9:8123",
            "http://homeassistant.local:8123",
            "ftp://ha.example.org",
        ] {
            assert!(validate_base_url(raw).is_err(), "{raw}");
        }
        assert_eq!(
            validate_base_url("https://ha.example.org/").expect("https"),
            "https://ha.example.org"
        );
        // Loopback is the one place plain http cannot leave the machine.
        assert!(validate_base_url("http://localhost:8123").is_ok());
    }

    #[test]
    fn a_service_name_cannot_select_a_different_endpoint() {
        // `notify_service` is concatenated into `/api/services/notify/<it>`.
        for raw in [
            "notify/persistent_notification",
            "../states",
            "mobile_app.pixel",
            "Mobile_App",
            "",
        ] {
            assert!(validate_identifier("notify_service", raw).is_err(), "{raw}");
        }
        assert_eq!(
            validate_identifier("notify_service", " mobile_app_pixel ").expect("bare name"),
            "mobile_app_pixel"
        );
    }
}
