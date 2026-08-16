//! The channel provider a sandboxed executable extension speaks for.
//!
//! One adapter serves every extension-backed messaging provider. What differs
//! between two such accounts is not code here but which extension capability
//! the account names, so a new provider arrives as an installed extension
//! rather than as a new file in this directory.
//!
//! # What the extension does, and what it never does
//!
//! It translates. Inbound, it is handed the bytes a provider delivered and
//! answers with normalized envelopes; outbound, it is handed a normalized
//! message and makes the provider's own API call from inside the sandbox,
//! through the exact origins it was granted. That is the whole of its job —
//! the same two translations every adapter in this directory performs.
//!
//! It never opens a listening socket, never resolves a route, never decides
//! who is allowed to talk, and never touches the run ledger. Inbound traffic
//! arrives on the daemon's own HTTP ingress, authenticated by Little Monkey's
//! signature and recorded durably *before* any guest code runs (see
//! `daemon::trigger`'s extension target and `dispatch_extension_delivery`).
//! Access policy, pairing, dedupe, session mapping, the outbox and its retry
//! semantics are `channel_ingress`'s and `channel_worker`'s, for this provider
//! exactly as for Telegram.
//!
//! # Transport
//!
//! Declared `LongPoll`. An extension channel can be driven either way and
//! usually both: the poll below asks the guest for anything new, which is how
//! a provider with no callback URL works at all, and a webhook-delivered
//! provider additionally normalizes through the durable extension-trigger
//! path. Declaring `Webhook` instead would take this account out of the poll
//! loop entirely and strand every provider that has only an API to ask.

use std::collections::BTreeSet;

use async_trait::async_trait;
use little_monkey_lib::channels::types::{
    AttachmentKind, AttachmentSource, BoundedMetadata, ChannelAttachment, ChannelConversation,
    ChannelEnvelope, ChannelHealth, ChannelKind, ChannelSender, ConversationKind, HealthState,
    InboundTransport, OutboundMessage, ProviderCapabilities, SendOutcome,
};
use little_monkey_lib::executable_extensions::{
    CapabilityKind, ExtensionManager, InvocationResult,
};
use serde::Deserialize;
use serde_json::Value as JsonValue;

use crate::daemon::channel_adapter::{AdapterConfig, ChannelAdapter, InboundBatch};

/// How many envelopes one poll or one delivery may produce. The ingress path
/// deduplicates and bounds what it accepts anyway; this is the bound on what
/// is decoded in the first place.
pub(crate) const MAX_EXTENSION_ENVELOPES: usize = 256;
/// Provider-independent text ceiling. An extension whose provider allows less
/// says so through its own error; this is the ceiling the outbox splits at.
const MAX_TEXT_CHARS: usize = 16_000;

pub(crate) struct ExtensionChannelAdapter {
    manager: ExtensionManager,
    account_id: String,
    extension_id: String,
    capability_id: String,
    /// Bounded, non-secret account configuration handed to the guest on every
    /// call. Credentials are *not* in here: the extension authenticates with
    /// its own declared secret slots inside the sandbox, so this process never
    /// holds the provider's token at all.
    settings: JsonValue,
}

impl ExtensionChannelAdapter {
    /// `state` is the daemon's own paths when one is running.
    ///
    /// Taken from there rather than resolved ambiently because the daemon
    /// already knows which profile's data root it serves, and an extension
    /// registry is per-root. The CLI's one-shot `channels probe` has no daemon
    /// and falls back to the ambient resolution, which is the same root it
    /// would have reached anyway.
    pub(crate) fn new(
        config: &AdapterConfig<'_>,
        state: Option<&crate::daemon::store::DaemonPaths>,
    ) -> Result<Self, String> {
        let (extension_id, capability_id) = binding_from_config(&config.account.non_secret_config)?;
        let app_data = match state {
            Some(paths) => paths.app_data()?.to_path_buf(),
            None => little_monkey_lib::app_paths::data_dir().ok_or_else(|| {
                "Could not resolve the Little Monkey app-data directory".to_string()
            })?,
        };
        Ok(Self {
            manager: ExtensionManager::new(app_data)?,
            account_id: config.account.account_id.clone(),
            extension_id,
            capability_id,
            settings: config.account.non_secret_config.clone(),
        })
    }

    async fn call(&self, input: JsonValue) -> Result<InvocationResult, String> {
        self.call_with_artifacts(input, Vec::new()).await
    }

    /// Invoke the capability, granting read access to exactly these artifacts.
    ///
    /// The grant is the host's, not the guest's: naming an artifact id in the
    /// request JSON gives no access at all, so an extension can only ever read
    /// the attachments this particular send actually carried.
    async fn call_with_artifacts(
        &self,
        input: JsonValue,
        artifact_ids: Vec<String>,
    ) -> Result<InvocationResult, String> {
        self.manager
            .invoke_owned_active_capability(
                CapabilityKind::Channel,
                &self.extension_id,
                &self.capability_id,
                serde_json::to_string(&input)
                    .map_err(|error| format!("Could not encode the channel request: {error}"))?,
                None,
                artifact_ids,
            )
            .await
    }
}

/// Read the owning extension and capability out of an account's non-secret
/// configuration. Both are required and validated, so an account can never
/// resolve to whichever extension happens to declare the id today.
pub(crate) fn binding_from_config(config: &JsonValue) -> Result<(String, String), String> {
    let extension_id = config
        .get("extension_id")
        .and_then(JsonValue::as_str)
        .ok_or_else(|| "An extension channel account must name its extension".to_string())?;
    let capability_id = config
        .get("capability_id")
        .and_then(JsonValue::as_str)
        .ok_or_else(|| "An extension channel account must name its capability".to_string())?;
    little_monkey_lib::executable_extensions::validate_extension_identifier(
        "extension id",
        extension_id,
    )?;
    little_monkey_lib::executable_extensions::validate_extension_identifier(
        "capability id",
        capability_id,
    )?;
    Ok((extension_id.to_string(), capability_id.to_string()))
}

#[async_trait]
impl ChannelAdapter for ExtensionChannelAdapter {
    fn kind(&self) -> ChannelKind {
        ChannelKind::Extension
    }

    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            kind: ChannelKind::Extension,
            inbound_transport: InboundTransport::LongPoll,
            max_text_chars: MAX_TEXT_CHARS,
            supports_threads: true,
            // The adapter can carry an attachment reference in both
            // directions, and the default `fetch_attachment` downloads a URL
            // through the same hardened client every other provider uses.
            supports_attachments: true,
            supports_mention_metadata: true,
            supports_idempotency_key: true,
            supports_delivery_receipts: false,
        }
    }

    /// Ask the provider who we are — through the guest, which is the only
    /// thing that holds the credential.
    ///
    /// A configuration that parses is never `Connected` here: this returns
    /// what the guest actually answered, so a revoked token or a disabled
    /// extension shows as an error rather than as a healthy account that
    /// silently receives nothing.
    async fn probe(&self) -> ChannelHealth {
        let now = crate::daemon::now_ms().unwrap_or_default() as i64;
        match self
            .call(serde_json::json!({
                "op": "probe",
                "account_id": self.account_id,
                "settings": self.settings,
            }))
            .await
        {
            Ok(result) => match serde_json::from_str::<ExtensionProbe>(&result.output_json) {
                Ok(probe) if probe.ok => ChannelHealth {
                    state: HealthState::Connected,
                    detail: probe.identity,
                    last_error: None,
                    probed_at_ms: now,
                },
                Ok(probe) => ChannelHealth {
                    state: HealthState::Error,
                    detail: None,
                    last_error: Some(
                        probe
                            .error
                            .unwrap_or_else(|| "The provider refused this account".to_string()),
                    ),
                    probed_at_ms: now,
                },
                Err(error) => ChannelHealth {
                    state: HealthState::Error,
                    detail: None,
                    last_error: Some(format!("The extension returned an unusable probe: {error}")),
                    probed_at_ms: now,
                },
            },
            Err(error) => ChannelHealth {
                state: HealthState::Error,
                detail: None,
                last_error: Some(error),
                probed_at_ms: now,
            },
        }
    }

    async fn poll(&self, cursor: Option<&str>) -> Result<InboundBatch, String> {
        let result = self
            .call(serde_json::json!({
                "op": "poll",
                "account_id": self.account_id,
                "settings": self.settings,
                "cursor": cursor,
            }))
            .await?;
        let batch: ExtensionInbound = serde_json::from_str(&result.output_json)
            .map_err(|error| format!("The extension returned an unusable batch: {error}"))?;
        Ok(InboundBatch {
            envelopes: normalize_envelopes(&self.account_id, batch.messages)?,
            cursor: batch.cursor.filter(|cursor| cursor.len() <= 1024),
        })
    }

    async fn send(&self, message: &OutboundMessage) -> SendOutcome {
        let attachments: Vec<JsonValue> = message
            .attachments
            .iter()
            .map(|attachment| {
                serde_json::json!({
                    "filename": attachment.filename,
                    "mime_type": attachment.mime_type,
                    "artifact_id": attachment.artifact_id,
                })
            })
            .collect();
        let request = serde_json::json!({
            "op": "send",
            "account_id": self.account_id,
            "settings": self.settings,
            "conversation_id": message.conversation_id,
            "thread_id": message.thread_id,
            "text": message.text,
            "attachments": attachments,
            "reply_to_provider_id": message.reply_to_provider_id,
            // The outbox's own key, handed through unchanged so a retried row
            // reaches the provider as the same request rather than as a second
            // message. A provider with no idempotency concept ignores it; one
            // that has it must use exactly this value.
            "idempotency_key": message.idempotency_key,
        });
        let artifact_ids: Vec<String> = message
            .attachments
            .iter()
            .map(|attachment| attachment.artifact_id.clone())
            .collect();
        let result = match self.call_with_artifacts(request, artifact_ids).await {
            Ok(result) => result,
            // The invocation itself failed: a trap, a timeout, a disabled
            // extension, a cancelled call. None of that proves the provider
            // was not reached, because the guest may have completed its HTTP
            // request and then died. Reconciliation, not retry.
            Err(error) => return SendOutcome::NeedsReconciliation { error },
        };
        match serde_json::from_str::<ExtensionSendResult>(&result.output_json) {
            Ok(outcome) => outcome.into_send_outcome(),
            Err(error) => SendOutcome::NeedsReconciliation {
                error: format!("The extension returned an unusable send result: {error}"),
            },
        }
    }
}

#[derive(Deserialize)]
struct ExtensionProbe {
    #[serde(default)]
    ok: bool,
    #[serde(default)]
    identity: Option<String>,
    #[serde(default)]
    error: Option<String>,
}

#[derive(Deserialize)]
pub(crate) struct ExtensionInbound {
    #[serde(default)]
    pub messages: Vec<ExtensionMessage>,
    #[serde(default)]
    pub cursor: Option<String>,
}

/// One inbound message as an extension normalizes it.
///
/// Deliberately the provider-independent vocabulary and nothing more: an
/// extension that wants to pass its provider's raw payload through has
/// nowhere to put it, which is the point — the rest of the app must never
/// receive provider-shaped JSON it would then have to guess at.
#[derive(Deserialize)]
pub(crate) struct ExtensionMessage {
    /// The provider's own event id. Half of the dedupe key, so it must be
    /// stable across a redelivery and must never be random.
    pub provider_event_id: String,
    pub conversation_id: String,
    #[serde(default)]
    pub conversation_kind: Option<String>,
    #[serde(default)]
    pub thread_id: Option<String>,
    #[serde(default)]
    pub conversation_title: Option<String>,
    pub sender_id: String,
    #[serde(default)]
    pub sender_label: Option<String>,
    #[serde(default)]
    pub sender_is_self: bool,
    #[serde(default)]
    pub sender_is_bot: bool,
    #[serde(default)]
    pub text: String,
    #[serde(default)]
    pub mentions_self: bool,
    #[serde(default)]
    pub reply_to_provider_id: Option<String>,
    #[serde(default)]
    pub received_at_ms: Option<i64>,
    #[serde(default)]
    pub attachments: Vec<ExtensionAttachment>,
}

#[derive(Deserialize)]
pub(crate) struct ExtensionAttachment {
    #[serde(default)]
    pub provider_id: Option<String>,
    #[serde(default)]
    pub filename: Option<String>,
    #[serde(default)]
    pub mime_type: Option<String>,
    #[serde(default)]
    pub declared_size_bytes: Option<u64>,
    /// An https URL the daemon's own hardened client will fetch. A guest
    /// cannot hand over bytes here: attachment download stays on the host, so
    /// the same size cap, the same SSRF guard and the same content store apply
    /// to this provider as to every other.
    pub url: String,
}

#[derive(Deserialize)]
struct ExtensionSendResult {
    #[serde(default)]
    status: String,
    #[serde(default)]
    provider_message_id: Option<String>,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    retry_after_ms: Option<i64>,
}

impl ExtensionSendResult {
    fn into_send_outcome(self) -> SendOutcome {
        let error = self
            .error
            .unwrap_or_else(|| "The provider rejected the message".to_string());
        match self.status.as_str() {
            "sent" => SendOutcome::Sent {
                provider_message_id: self
                    .provider_message_id
                    .filter(|id| !id.is_empty() && id.len() <= 512),
            },
            "retry" => SendOutcome::RetryableFailure {
                error,
                retry_after_ms: self.retry_after_ms.filter(|ms| *ms >= 0),
            },
            "failed" => SendOutcome::PermanentFailure { error },
            // Anything else — including an extension that answered with a
            // status this build does not know — is treated as "we do not know
            // whether the provider saw it". Guessing "failed" would let the
            // outbox drop a message that was delivered; guessing "retry" would
            // let it send a second one.
            _ => SendOutcome::NeedsReconciliation { error },
        }
    }
}

fn attachment_kind(mime_type: Option<&str>) -> AttachmentKind {
    match mime_type.map(|value| value.to_ascii_lowercase()) {
        Some(value) if value.starts_with("image/") => AttachmentKind::Image,
        Some(value) if value.starts_with("audio/") => AttachmentKind::Audio,
        Some(value) if value.starts_with("video/") => AttachmentKind::Video,
        Some(value)
            if value.starts_with("text/")
                || value.starts_with("application/pdf")
                || value.starts_with("application/vnd.") =>
        {
            AttachmentKind::Document
        }
        _ => AttachmentKind::Other,
    }
}

/// Turn an extension's normalized messages into envelopes.
///
/// Shared by the poll above and by the durable webhook path, so a message
/// reaches `channel_ingress` in exactly the same shape whichever way it
/// arrived. Every bound is applied here rather than trusted from the guest.
pub(crate) fn normalize_envelopes(
    account_id: &str,
    messages: Vec<ExtensionMessage>,
) -> Result<Vec<ChannelEnvelope>, String> {
    if messages.len() > MAX_EXTENSION_ENVELOPES {
        return Err(format!(
            "An extension channel may return at most {MAX_EXTENSION_ENVELOPES} messages at once"
        ));
    }
    let mut seen = BTreeSet::new();
    let mut envelopes = Vec::with_capacity(messages.len());
    for message in messages {
        if message.provider_event_id.is_empty()
            || message.provider_event_id.len() > 512
            || message.conversation_id.is_empty()
            || message.conversation_id.len() > 512
            || message.sender_id.is_empty()
            || message.sender_id.len() > 512
        {
            return Err("An extension message is missing its identifiers".to_string());
        }
        // A batch that repeats one event id would collapse to a single durable
        // row anyway; saying so is more useful than silently indexing over it.
        if !seen.insert(message.provider_event_id.clone()) {
            return Err(format!(
                "The extension repeated event id '{}' within one batch",
                message.provider_event_id
            ));
        }
        if message.text.len() > 256 * 1024 {
            return Err("An extension message exceeds the inbound text limit".to_string());
        }
        let kind = match message.conversation_kind.as_deref() {
            Some("direct") | None => ConversationKind::Direct,
            Some("group") => ConversationKind::Group,
            Some("channel") => ConversationKind::Channel,
            Some(other) => {
                return Err(format!("Unknown conversation kind '{other}'"));
            }
        };
        let attachments = message
            .attachments
            .into_iter()
            .take(32)
            .map(|attachment| ChannelAttachment {
                // Classified from the declared MIME type rather than taken as
                // a word from the guest: the kind drives how the rest of the
                // app treats the bytes, so it is derived from something the
                // host can also check once the file is downloaded.
                kind: attachment_kind(attachment.mime_type.as_deref()),
                provider_id: attachment.provider_id,
                filename: attachment.filename,
                mime_type: attachment.mime_type,
                declared_size_bytes: attachment.declared_size_bytes,
                stored_size_bytes: None,
                source: AttachmentSource::Url {
                    url: attachment.url,
                },
                stored_artifact_id: None,
                text_excerpt: None,
                fetch_error: None,
            })
            .collect();
        envelopes.push(ChannelEnvelope {
            account_id: account_id.to_string(),
            kind: ChannelKind::Extension,
            provider_event_id: message.provider_event_id,
            conversation: ChannelConversation {
                conversation_id: message.conversation_id,
                kind,
                thread_id: message.thread_id.filter(|id| id.len() <= 512),
                title: message.conversation_title.filter(|t| t.len() <= 512),
            },
            sender: ChannelSender {
                sender_id: message.sender_id,
                display_label: message.sender_label.filter(|label| label.len() <= 256),
                is_self: message.sender_is_self,
                is_bot: message.sender_is_bot,
            },
            text: message.text,
            attachments,
            reply_to_provider_id: message.reply_to_provider_id.filter(|id| id.len() <= 512),
            mentions_self: message.mentions_self,
            received_at_ms: message
                .received_at_ms
                .unwrap_or_else(|| crate::daemon::now_ms().unwrap_or_default() as i64),
            metadata: BoundedMetadata::default(),
        });
    }
    Ok(envelopes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message(event_id: &str) -> ExtensionMessage {
        serde_json::from_value(serde_json::json!({
            "provider_event_id": event_id,
            "conversation_id": "room-1",
            "sender_id": "user-1",
            "text": "hello",
        }))
        .expect("fixture parses")
    }

    #[test]
    fn a_binding_needs_both_halves() {
        assert!(binding_from_config(&serde_json::json!({"extension_id": "dev.example"})).is_err());
        assert!(binding_from_config(&serde_json::json!({"capability_id": "chat"})).is_err());
        let (extension_id, capability_id) = binding_from_config(
            &serde_json::json!({"extension_id": "dev.example", "capability_id": "chat"}),
        )
        .expect("both halves present");
        assert_eq!(extension_id, "dev.example");
        assert_eq!(capability_id, "chat");
    }

    #[test]
    fn a_repeated_event_id_in_one_batch_is_refused() {
        let error = normalize_envelopes("acct-1", vec![message("evt-1"), message("evt-1")])
            .expect_err("a duplicate is refused");
        assert!(error.contains("evt-1"), "{error}");
    }

    #[test]
    fn an_envelope_carries_the_accounts_identity_not_the_guests() {
        let envelopes = normalize_envelopes("acct-1", vec![message("evt-1")]).expect("normalizes");
        assert_eq!(envelopes[0].account_id, "acct-1");
        assert_eq!(envelopes[0].kind, ChannelKind::Extension);
        assert_eq!(envelopes[0].dedupe_key(), "acct-1:evt-1");
    }

    #[test]
    fn an_unknown_send_status_needs_reconciliation() {
        let outcome: ExtensionSendResult =
            serde_json::from_value(serde_json::json!({"status": "probably"}))
                .expect("fixture parses");
        assert!(matches!(
            outcome.into_send_outcome(),
            SendOutcome::NeedsReconciliation { .. }
        ));
    }

    #[test]
    fn a_send_that_reports_delivery_carries_the_provider_id() {
        let outcome: ExtensionSendResult = serde_json::from_value(
            serde_json::json!({"status": "sent", "provider_message_id": "m-1"}),
        )
        .expect("fixture parses");
        assert_eq!(
            outcome.into_send_outcome(),
            SendOutcome::Sent {
                provider_message_id: Some("m-1".to_string())
            }
        );
    }
}
