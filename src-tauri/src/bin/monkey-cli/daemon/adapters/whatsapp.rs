//! WhatsApp Business Cloud API adapter (official Meta integration only).
//!
//! Inbound is a webhook: Meta signs the raw POST body with the app secret and
//! sends the hex digest in `X-Hub-Signature-256`. There is no signed
//! timestamp on this transport — unlike LINE's `replyToken` or a JWT's `exp`,
//! Meta's webhook signature covers only the body, so there is nothing to
//! check a skew window against here, and redelivery safety instead comes
//! entirely from `provider_event_id` dedupe on the message id (Meta does
//! redeliver on a missed 200).
//!
//! Outbound is the Graph API `POST /<phone_number_id>/messages` endpoint with
//! a long-lived system-user access token.

use async_trait::async_trait;
use little_monkey_lib::channels::types::{
    AttachmentKind, AttachmentSource, ChannelAttachment, ChannelConversation, ChannelEnvelope,
    ChannelHealth, ChannelKind, ChannelSender, DeliveryReceipt, DeliveryState, InboundTransport,
    OutboundMessage, ProviderCapabilities, SendOutcome,
};
use serde::Deserialize;
use serde_json::Value as JsonValue;

use crate::daemon::channel_adapter::{
    AdapterConfig, ChannelAdapter, InboundBatch, WebhookAck, WebhookChannelAdapter,
};

const GRAPH_API_BASE: &str = "https://graph.facebook.com/v21.0";

/// Operator-supplied, non-secret configuration.
#[derive(Debug, Deserialize)]
struct WhatsAppNonSecretConfig {
    phone_number_id: String,
}

/// The two secrets this provider needs, bundled into the single opaque
/// `AdapterConfig::secret` string as JSON. Neither is ever logged.
#[derive(Debug, Deserialize)]
struct WhatsAppSecrets {
    app_secret: String,
    access_token: String,
    /// The token the operator typed into Meta's "Verify token" box. A shared
    /// secret like the other two, so it lives with them in the keychain and
    /// not in `non_secret_config`. Defaulted because an account configured
    /// before this handshake existed still has to build — it simply cannot
    /// answer a challenge until the operator saves one.
    #[serde(default)]
    verify_token: String,
}

pub struct WhatsAppAdapter {
    account_id: String,
    phone_number_id: String,
    app_secret: String,
    access_token: String,
    verify_token: String,
    /// The Graph API origin. Always [`GRAPH_API_BASE`] in production;
    /// swappable in tests so `send`/`probe` can be exercised against a
    /// loopback fixture instead of the real network.
    graph_api_base: String,
    /// Where an outbound attachment's bytes come from. The daemon's content
    /// store in production, a fixture in tests.
    blobs: std::sync::Arc<dyn crate::daemon::channel_adapter::BlobSource>,
}

impl WhatsAppAdapter {
    pub fn new(config: &AdapterConfig<'_>) -> Result<Self, String> {
        let non_secret: WhatsAppNonSecretConfig =
            serde_json::from_value(config.account.non_secret_config.clone())
                .map_err(|error| format!("Invalid WhatsApp account config: {error}"))?;
        if non_secret.phone_number_id.trim().is_empty() {
            return Err("WhatsApp account is missing phone_number_id".to_string());
        }
        let secrets: WhatsAppSecrets = serde_json::from_str(&config.secret)
            .map_err(|_| "WhatsApp account credential is missing or malformed".to_string())?;
        if secrets.app_secret.trim().is_empty() || secrets.access_token.trim().is_empty() {
            return Err(
                "WhatsApp account credential is missing app_secret or access_token".to_string(),
            );
        }
        Ok(Self {
            account_id: config.account.account_id.clone(),
            phone_number_id: non_secret.phone_number_id,
            app_secret: secrets.app_secret,
            access_token: secrets.access_token,
            verify_token: secrets.verify_token,
            graph_api_base: GRAPH_API_BASE.to_string(),
            blobs: std::sync::Arc::new(crate::daemon::channel_adapter::DaemonBlobs),
        })
    }

    #[cfg(test)]
    pub(crate) fn with_base_url(mut self, base: &str) -> Self {
        self.graph_api_base = base.to_string();
        self
    }

    #[cfg(test)]
    pub(crate) fn with_blobs(
        mut self,
        blobs: std::sync::Arc<dyn crate::daemon::channel_adapter::BlobSource>,
    ) -> Self {
        self.blobs = blobs;
        self
    }

    /// One entry of `value.statuses`. An unknown status word is dropped rather
    /// than guessed at: recording a delivery that may not have happened is
    /// worse than recording nothing.
    fn normalize_status(&self, status: &JsonValue, now_ms: i64) -> Option<DeliveryReceipt> {
        let provider_message_id = status.get("id").and_then(JsonValue::as_str)?;
        let state = match status.get("status").and_then(JsonValue::as_str)? {
            "sent" => DeliveryState::Sent,
            "delivered" => DeliveryState::Delivered,
            "read" => DeliveryState::Read,
            "failed" => DeliveryState::Failed,
            _ => return None,
        };
        // Meta nests the reason under `errors[0]`, and its `title` is the part
        // written for a human. The code goes with it because that is what the
        // provider's own documentation is indexed by.
        let error = status
            .get("errors")
            .and_then(JsonValue::as_array)
            .and_then(|errors| errors.first())
            .map(|first| {
                let title = first
                    .get("title")
                    .and_then(JsonValue::as_str)
                    .unwrap_or("Delivery failed");
                match first.get("code").and_then(JsonValue::as_i64) {
                    Some(code) => format!("{title} (WhatsApp error {code})"),
                    None => title.to_string(),
                }
            });
        Some(DeliveryReceipt {
            account_id: self.account_id.clone(),
            provider_message_id: provider_message_id.to_string(),
            state,
            error,
            observed_at_ms: now_ms,
        })
    }
}

impl WebhookChannelAdapter for WhatsAppAdapter {
    fn kind(&self) -> ChannelKind {
        ChannelKind::WhatsApp
    }

    /// Meta asks for a `200` and reads nothing else. Anything else — including
    /// a `202` — counts as a failed delivery, is retried, and eventually gets
    /// the callback URL disabled.
    fn ack(&self) -> WebhookAck {
        WebhookAck::empty_ok()
    }

    fn verify_and_normalize(
        &self,
        headers: &[(String, String)],
        body: &[u8],
        _public_base_url: Option<&str>,
        now_ms: i64,
    ) -> Result<Vec<ChannelEnvelope>, String> {
        // WhatsApp's signature covers only the body, so `public_base_url` is
        // unused here — it exists for providers whose signature *does* cover
        // the delivery URL, and reconstructing one from request headers would
        // be exactly the attacker-controlled shortcut this adapter must not
        // take.
        let signature = headers
            .iter()
            .find(|(name, _)| name == "x-hub-signature-256")
            .map(|(_, value)| value.as_str())
            .ok_or_else(|| "Missing X-Hub-Signature-256 header".to_string())?;
        verify_hmac_sha256_hex(&self.app_secret, body, signature)
            .map_err(|_| "WhatsApp webhook signature verification failed".to_string())?;

        let payload: JsonValue = serde_json::from_slice(body)
            .map_err(|error| format!("WhatsApp webhook body is not valid JSON: {error}"))?;
        Ok(normalize_payload(&payload, &self.account_id, now_ms))
    }

    /// Meta reports what happened to a message we sent — `sent`, `delivered`,
    /// `read` or `failed` — in the same webhook, alongside (or instead of) any
    /// inbound messages.
    ///
    /// A failure carries the reason, and the reasons matter operationally: a
    /// 24-hour session window that closed, or a recipient who never opted in,
    /// looks exactly like a successful send until the status arrives.
    fn delivery_receipts(&self, body: &[u8], now_ms: i64) -> Vec<DeliveryReceipt> {
        let Ok(payload) = serde_json::from_slice::<JsonValue>(body) else {
            return Vec::new();
        };
        payload
            .get("entry")
            .and_then(JsonValue::as_array)
            .into_iter()
            .flatten()
            .filter_map(|entry| entry.get("changes").and_then(JsonValue::as_array))
            .flatten()
            .filter_map(|change| {
                change
                    .get("value")
                    .and_then(|value| value.get("statuses"))
                    .and_then(JsonValue::as_array)
            })
            .flatten()
            .filter_map(|status| self.normalize_status(status, now_ms))
            .collect()
    }

    /// Meta's subscription handshake: it GETs the callback URL with a verify
    /// token the operator chose, and only saves the URL if the exact
    /// `hub.challenge` comes back.
    ///
    /// The comparison is constant-time and an unconfigured token never
    /// matches, so this endpoint cannot be turned into an oracle for guessing
    /// the token one request at a time. Nothing else about the account is
    /// revealed either way — the caller answers a mismatch with a flat 403.
    fn verification_challenge(&self, query: &str) -> Option<String> {
        if self.verify_token.is_empty() {
            return None;
        }
        let params: std::collections::BTreeMap<String, String> =
            url::form_urlencoded::parse(query.as_bytes())
                .into_owned()
                .collect();
        if params.get("hub.mode").map(String::as_str) != Some("subscribe") {
            return None;
        }
        let offered = params.get("hub.verify_token")?;
        // Digests are compared, not the tokens: two SHA-256 outputs compare in
        // a way that leaks nothing about the inputs, so a plain `==` here is
        // not the timing side channel that comparing the tokens directly
        // would be.
        let expected = ring::digest::digest(&ring::digest::SHA256, self.verify_token.as_bytes());
        let actual = ring::digest::digest(&ring::digest::SHA256, offered.as_bytes());
        if expected.as_ref() != actual.as_ref() {
            return None;
        }
        params.get("hub.challenge").cloned()
    }
}

#[async_trait]
impl ChannelAdapter for WhatsAppAdapter {
    fn kind(&self) -> ChannelKind {
        ChannelKind::WhatsApp
    }

    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            // The Cloud API has no group concept: every conversation is a 1:1
            // thread with a phone number.
            max_text_chars: 4096,
            supports_threads: false,
            supports_attachments: true,
            supports_mention_metadata: false,
            supports_idempotency_key: false,
            supports_delivery_receipts: true,
            ..ProviderCapabilities::minimal(ChannelKind::WhatsApp, InboundTransport::Webhook)
        }
    }

    async fn probe(&self) -> ChannelHealth {
        let now = now_ms();
        let client = match little_monkey_lib::egress::hardened().build() {
            Ok(client) => client,
            Err(error) => {
                return ChannelHealth::error(now, format!("Failed to build client: {error}"))
            }
        };
        let url = format!("{}/{}", self.graph_api_base, self.phone_number_id);
        let request = client.get(url).bearer_auth(&self.access_token);
        match little_monkey_lib::egress::send(request).await {
            Ok(response) if response.status().is_success() => {
                ChannelHealth::connected(now, Some("Phone number reachable".to_string()))
            }
            Ok(response) => ChannelHealth::error(
                now,
                format!("WhatsApp probe failed with status {}", response.status()),
            ),
            Err(error) => ChannelHealth::error(now, sanitize_transport_error(&error)),
        }
    }

    async fn poll(&self, _cursor: Option<&str>) -> Result<InboundBatch, String> {
        // WhatsApp is delivered to, not polled — see the module doc.
        Ok(InboundBatch::default())
    }

    /// Meta hands out a media id, not a URL. The id is exchanged for a
    /// short-lived download URL on the Graph API, and that URL still needs the
    /// same bearer token — an unauthenticated GET of it returns nothing.
    async fn fetch_attachment(
        &self,
        attachment: &ChannelAttachment,
        limits: crate::daemon::channel_adapter::AttachmentLimits,
    ) -> Result<Vec<u8>, String> {
        let AttachmentSource::ProviderHandle { handle } = &attachment.source else {
            return Err("That WhatsApp attachment carries no media id".to_string());
        };
        let client = little_monkey_lib::egress::hardened()
            .build()
            .map_err(|error| format!("Failed to build client: {error}"))?;
        let request = client
            .get(format!("{}/{handle}", self.graph_api_base))
            .bearer_auth(&self.access_token);
        let response = little_monkey_lib::egress::send(request)
            .await
            .map_err(|_| "WhatsApp did not answer the media lookup".to_string())?;
        if !response.status().is_success() {
            return Err(format!(
                "WhatsApp returned {} for the media lookup",
                response.status()
            ));
        }
        let body: JsonValue = response
            .json()
            .await
            .map_err(|_| "WhatsApp's media lookup was not JSON".to_string())?;
        let url = body
            .get("url")
            .and_then(JsonValue::as_str)
            .ok_or_else(|| "WhatsApp named no download URL for that media id".to_string())?;
        crate::daemon::channel_adapter::fetch_url(url, Some(&self.access_token), limits.max_bytes)
            .await
    }

    async fn send(&self, message: &OutboundMessage) -> SendOutcome {
        let client = match little_monkey_lib::egress::hardened().build() {
            Ok(client) => client,
            Err(error) => {
                return SendOutcome::PermanentFailure {
                    error: format!("Failed to build client: {error}"),
                }
            }
        };

        // Text first, then one message per file. The Cloud API has no way to
        // put several files in one message, and a caption is limited to 1024
        // characters — far below what a reply may contain — so the text is not
        // folded into the first attachment.
        let mut last = SendOutcome::Sent {
            provider_message_id: None,
        };
        // WhatsApp quotes exactly one message, so the reply marker goes on the
        // first thing sent and the rest of the burst follows it unquoted —
        // quoting the same message three times reads as three separate answers
        // to the same question.
        let mut quote = message
            .reply_to_provider_id
            .as_deref()
            .filter(|id| !id.is_empty());
        if !message.text.trim().is_empty() {
            let mut body = serde_json::json!({
                "messaging_product": "whatsapp",
                "to": message.conversation_id,
                "type": "text",
                "text": { "body": message.text },
            });
            if let Some(quoted) = quote.take() {
                body["context"] = serde_json::json!({ "message_id": quoted });
            }
            last = self.post_message(&client, body).await;
            if !matches!(last, SendOutcome::Sent { .. }) {
                return last;
            }
        }
        for attachment in &message.attachments {
            let media_id = match self.upload_media(&client, attachment).await {
                Ok(id) => id,
                Err(outcome) => return outcome,
            };
            let media_type =
                media_type_for(crate::daemon::channel_adapter::attachment_mime(attachment));
            let mut media = serde_json::json!({ "id": media_id });
            if media_type == "document" {
                if let Some(filename) = attachment.filename.clone() {
                    media["filename"] = JsonValue::from(filename);
                }
            }
            let mut body = serde_json::json!({
                "messaging_product": "whatsapp",
                "to": message.conversation_id,
                "type": media_type,
                media_type: media,
            });
            if let Some(quoted) = quote.take() {
                body["context"] = serde_json::json!({ "message_id": quoted });
            }
            last = self.post_message(&client, body).await;
            if !matches!(last, SendOutcome::Sent { .. }) {
                return last;
            }
        }
        last
    }
}

impl WhatsAppAdapter {
    /// Upload one file and return the media id the Cloud API assigns it.
    ///
    /// Meta will not accept bytes inline in a message: a file is uploaded
    /// first, and the id it returns is what a message carries. The id expires
    /// (30 days at the time of writing), which is fine — it is used within the
    /// same send.
    async fn upload_media(
        &self,
        client: &reqwest::Client,
        attachment: &little_monkey_lib::channels::types::OutboundAttachment,
    ) -> Result<String, SendOutcome> {
        let bytes = self
            .blobs
            .read(&attachment.artifact_id)
            .map_err(|error| SendOutcome::PermanentFailure { error })?;
        let mime = crate::daemon::channel_adapter::attachment_mime(attachment).to_string();
        let filename = attachment
            .filename
            .clone()
            .unwrap_or_else(|| "attachment".to_string());
        let part = reqwest::multipart::Part::bytes(bytes)
            .file_name(filename)
            .mime_str(&mime)
            .map_err(|_| SendOutcome::PermanentFailure {
                error: format!("'{mime}' is not a MIME type WhatsApp can be given"),
            })?;
        let form = reqwest::multipart::Form::new()
            .text("messaging_product", "whatsapp")
            .part("file", part);
        let url = format!("{}/{}/media", self.graph_api_base, self.phone_number_id);
        let request = client
            .post(url)
            .bearer_auth(&self.access_token)
            .multipart(form);
        let response = match little_monkey_lib::egress::send(request).await {
            Ok(response) => response,
            Err(error) => return Err(map_transport_error(&error)),
        };
        let status = response.status();
        let bytes = match response.bytes().await {
            Ok(bytes) => bytes,
            Err(error) => return Err(map_transport_error(&error)),
        };
        let parsed: JsonValue = serde_json::from_slice(&bytes).unwrap_or(JsonValue::Null);
        if !status.is_success() {
            let error = parsed
                .get("error")
                .and_then(|error| error.get("message"))
                .and_then(JsonValue::as_str)
                .unwrap_or("WhatsApp rejected the upload")
                .to_string();
            return Err(if status.is_server_error() || status.as_u16() == 429 {
                SendOutcome::RetryableFailure {
                    error,
                    retry_after_ms: None,
                }
            } else {
                SendOutcome::PermanentFailure { error }
            });
        }
        parsed
            .get("id")
            .and_then(JsonValue::as_str)
            .map(str::to_string)
            .ok_or_else(|| SendOutcome::NeedsReconciliation {
                error: "WhatsApp accepted the upload but named no media id".to_string(),
            })
    }

    /// POST one already-built message body to the Cloud API.
    async fn post_message(&self, client: &reqwest::Client, body: JsonValue) -> SendOutcome {
        let url = format!("{}/{}/messages", self.graph_api_base, self.phone_number_id);
        let request = client.post(url).bearer_auth(&self.access_token).json(&body);
        let response = match little_monkey_lib::egress::send(request).await {
            Ok(response) => response,
            Err(error) => return map_transport_error(&error),
        };
        let status = response.status();
        let retry_after_ms = response
            .headers()
            .get(reqwest::header::RETRY_AFTER)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<i64>().ok())
            .map(|seconds| seconds * 1000);
        let bytes = match response.bytes().await {
            Ok(bytes) => bytes,
            Err(error) => return map_transport_error(&error),
        };
        let parsed: JsonValue = serde_json::from_slice(&bytes).unwrap_or(JsonValue::Null);

        if status.is_success() {
            let provider_message_id = parsed
                .get("messages")
                .and_then(JsonValue::as_array)
                .and_then(|messages| messages.first())
                .and_then(|message| message.get("id"))
                .and_then(JsonValue::as_str)
                .map(str::to_string);
            return SendOutcome::Sent {
                provider_message_id,
            };
        }

        let error_code = parsed
            .get("error")
            .and_then(|error| error.get("code"))
            .and_then(JsonValue::as_i64);
        let error_message = parsed
            .get("error")
            .and_then(|error| error.get("message"))
            .and_then(JsonValue::as_str)
            .unwrap_or("WhatsApp send failed")
            .to_string();

        if error_code == Some(131047) {
            return SendOutcome::PermanentFailure {
                error: format!(
                    "{error_message} — the 24-hour customer service window has closed; \
                     a template message must be used to start a new conversation"
                ),
            };
        }
        if status.as_u16() == 429 {
            return SendOutcome::RetryableFailure {
                error: error_message,
                retry_after_ms,
            };
        }
        if status.as_u16() == 401 || status.as_u16() == 403 {
            return SendOutcome::PermanentFailure {
                error: error_message,
            };
        }
        if status.is_server_error() {
            return SendOutcome::RetryableFailure {
                error: error_message,
                retry_after_ms,
            };
        }
        SendOutcome::PermanentFailure {
            error: error_message,
        }
    }
}

/// Which Cloud API message type carries this MIME type.
///
/// SVG is a document rather than an image: WhatsApp's image types are JPEG and
/// PNG only, and offering it as an image is a rejected send instead of a file
/// that arrives.
fn media_type_for(mime: &str) -> &'static str {
    if mime == "image/jpeg" || mime == "image/png" {
        "image"
    } else if mime.starts_with("audio/") {
        "audio"
    } else if mime.starts_with("video/") {
        "video"
    } else {
        "document"
    }
}

/// Verifies `sha256=<hex>` over `body` with a constant-time comparison.
fn verify_hmac_sha256_hex(secret: &str, body: &[u8], header_value: &str) -> Result<(), ()> {
    let hex_digest = header_value.strip_prefix("sha256=").unwrap_or(header_value);
    let expected = decode_hex(hex_digest).ok_or(())?;
    let key = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, secret.as_bytes());
    ring::hmac::verify(&key, body, &expected).map_err(|_| ())
}

fn decode_hex(value: &str) -> Option<Vec<u8>> {
    if value.len() % 2 != 0 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    (0..value.len())
        .step_by(2)
        .map(|index| u8::from_str_radix(&value[index..index + 2], 16).ok())
        .collect()
}

/// Never includes a token, an app secret, or a raw `Authorization` header —
/// only what reqwest itself classifies the failure as.
fn sanitize_transport_error(error: &reqwest::Error) -> String {
    if error.is_timeout() {
        "WhatsApp request timed out".to_string()
    } else if error.is_connect() {
        "Could not connect to the WhatsApp API".to_string()
    } else {
        "WhatsApp request failed".to_string()
    }
}

fn map_transport_error(error: &reqwest::Error) -> SendOutcome {
    if error.is_connect() {
        // The connection never established, so the request provably never
        // reached the provider.
        SendOutcome::RetryableFailure {
            error: sanitize_transport_error(error),
            retry_after_ms: None,
        }
    } else {
        // A timeout or body error after the connection was established means
        // the request may already have reached WhatsApp.
        SendOutcome::NeedsReconciliation {
            error: sanitize_transport_error(error),
        }
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(0)
}

/// Normalizes `entry[].changes[].value.messages[]` into envelopes.
///
/// A `value` that carries only `statuses[]` (a delivery/read receipt with no
/// `messages[]`) normalizes to nothing: it is not a message, and callers must
/// not synthesize one.
fn normalize_payload(
    payload: &JsonValue,
    account_id: &str,
    received_at_ms: i64,
) -> Vec<ChannelEnvelope> {
    let mut envelopes = Vec::new();
    let Some(entries) = payload.get("entry").and_then(JsonValue::as_array) else {
        return envelopes;
    };
    for entry in entries {
        let Some(changes) = entry.get("changes").and_then(JsonValue::as_array) else {
            continue;
        };
        for change in changes {
            let Some(value) = change.get("value") else {
                continue;
            };
            let Some(messages) = value.get("messages").and_then(JsonValue::as_array) else {
                continue;
            };
            let contact_names = contact_name_lookup(value);
            for message in messages {
                if let Some(envelope) =
                    normalize_message(message, &contact_names, account_id, received_at_ms)
                {
                    envelopes.push(envelope);
                }
            }
        }
    }
    envelopes
}

fn contact_name_lookup(value: &JsonValue) -> std::collections::BTreeMap<String, String> {
    let mut names = std::collections::BTreeMap::new();
    if let Some(contacts) = value.get("contacts").and_then(JsonValue::as_array) {
        for contact in contacts {
            let wa_id = contact.get("wa_id").and_then(JsonValue::as_str);
            let name = contact
                .get("profile")
                .and_then(|profile| profile.get("name"))
                .and_then(JsonValue::as_str);
            if let (Some(wa_id), Some(name)) = (wa_id, name) {
                names.insert(wa_id.to_string(), name.to_string());
            }
        }
    }
    names
}

fn normalize_message(
    message: &JsonValue,
    contact_names: &std::collections::BTreeMap<String, String>,
    account_id: &str,
    fallback_received_at_ms: i64,
) -> Option<ChannelEnvelope> {
    let from = message.get("from").and_then(JsonValue::as_str)?.to_string();
    let provider_event_id = message.get("id").and_then(JsonValue::as_str)?.to_string();
    let message_type = message
        .get("type")
        .and_then(JsonValue::as_str)
        .unwrap_or("");

    let text = if message_type == "text" {
        message
            .get("text")
            .and_then(|text| text.get("body"))
            .and_then(JsonValue::as_str)
            .unwrap_or_default()
            .to_string()
    } else {
        String::new()
    };

    let mut attachments = Vec::new();
    if let Some(kind) = attachment_kind_for(message_type) {
        if let Some(media) = message.get(message_type) {
            let provider_id = media
                .get("id")
                .and_then(JsonValue::as_str)
                .map(str::to_string);
            let mime_type = media
                .get("mime_type")
                .and_then(JsonValue::as_str)
                .map(str::to_string);
            let filename = media
                .get("filename")
                .and_then(JsonValue::as_str)
                .map(str::to_string);
            if let Some(handle) = provider_id.clone() {
                attachments.push(ChannelAttachment {
                    stored_artifact_id: None,
                    text_excerpt: None,
                    fetch_error: None,
                    provider_id,
                    kind,
                    filename,
                    mime_type,
                    declared_size_bytes: None,
                    source: AttachmentSource::ProviderHandle { handle },
                });
            }
        }
    }

    let received_at_ms = message
        .get("timestamp")
        .and_then(JsonValue::as_str)
        .and_then(|value| value.parse::<i64>().ok())
        .map(|seconds| seconds * 1000)
        .unwrap_or(fallback_received_at_ms);

    let mut sender = ChannelSender::new(from.clone());
    sender = sender.with_label(contact_names.get(&from).cloned());

    Some(ChannelEnvelope {
        account_id: account_id.to_string(),
        kind: ChannelKind::WhatsApp,
        provider_event_id,
        // The Cloud API has no group support: every conversation is the
        // customer's own phone number as a direct message.
        conversation: ChannelConversation::direct(from),
        sender,
        text,
        attachments,
        reply_to_provider_id: message
            .get("context")
            .and_then(|context| context.get("id"))
            .and_then(JsonValue::as_str)
            .map(str::to_string),
        mentions_self: false,
        received_at_ms,
        metadata: little_monkey_lib::channels::types::BoundedMetadata::sanitized([(
            "wa_message_type".to_string(),
            message_type.to_string(),
        )]),
    })
}

fn attachment_kind_for(message_type: &str) -> Option<AttachmentKind> {
    match message_type {
        "image" => Some(AttachmentKind::Image),
        "audio" => Some(AttachmentKind::Audio),
        "video" => Some(AttachmentKind::Video),
        "document" => Some(AttachmentKind::Document),
        _ => None,
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::daemon::channel_store::ChannelAccountRecord;
    use little_monkey_lib::channels::policy::ChannelAccessPolicy;
    use little_monkey_lib::channels::types::HealthState;
    use std::io::{Read, Write};

    // --- Test fixtures -----------------------------------------------------

    pub(crate) fn test_account(non_secret_config: JsonValue) -> ChannelAccountRecord {
        ChannelAccountRecord {
            account_id: "acct-wa".to_string(),
            kind: ChannelKind::WhatsApp,
            label: "Test WhatsApp".to_string(),
            enabled: true,
            non_secret_config,
            credential_ref: Some("wa-cred".to_string()),
            access_policy: ChannelAccessPolicy::default(),
            health: ChannelHealth {
                state: HealthState::Unconfigured,
                detail: None,
                last_error: None,
                probed_at_ms: 0,
            },
            created_at_ms: 0,
            updated_at_ms: 0,
        }
    }

    fn adapter(app_secret: &str, access_token: &str) -> WhatsAppAdapter {
        let account = test_account(serde_json::json!({ "phone_number_id": "1234567890" }));
        let secret = serde_json::json!({
            "app_secret": app_secret,
            "access_token": access_token,
        })
        .to_string();
        let config = AdapterConfig {
            account: &account,
            secret,
        };
        WhatsAppAdapter::new(&config).expect("adapter builds")
    }

    fn sign(secret: &str, body: &[u8]) -> String {
        let key = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, secret.as_bytes());
        let tag = ring::hmac::sign(&key, body);
        format!("sha256={}", hex_encode(tag.as_ref()))
    }

    fn hex_encode(bytes: &[u8]) -> String {
        bytes.iter().map(|byte| format!("{byte:02x}")).collect()
    }

    fn text_message_body() -> Vec<u8> {
        serde_json::json!({
            "entry": [{
                "id": "waba-1",
                "changes": [{
                    "value": {
                        "messaging_product": "whatsapp",
                        "contacts": [{"profile": {"name": "Ada Lovelace"}, "wa_id": "15550001111"}],
                        "messages": [{
                            "from": "15550001111",
                            "id": "wamid.ABC123",
                            "timestamp": "1700000000",
                            "type": "text",
                            "text": {"body": "hello there"}
                        }]
                    },
                    "field": "messages"
                }]
            }]
        })
        .to_string()
        .into_bytes()
    }

    fn status_only_body() -> Vec<u8> {
        serde_json::json!({
            "entry": [{
                "id": "waba-1",
                "changes": [{
                    "value": {
                        "messaging_product": "whatsapp",
                        "statuses": [{"id": "wamid.XYZ", "status": "delivered", "timestamp": "1700000000"}]
                    },
                    "field": "messages"
                }]
            }]
        })
        .to_string()
        .into_bytes()
    }

    // --- Signature verification ---------------------------------------------

    #[test]
    fn a_correctly_signed_body_verifies_and_normalizes() {
        let adapter = adapter("app-secret-value", "token-value");
        let body = text_message_body();
        let signature = sign("app-secret-value", &body);
        let envelopes = adapter
            .verify_and_normalize(
                &[("x-hub-signature-256".to_string(), signature)],
                &body,
                None,
                0,
            )
            .expect("verifies");
        assert_eq!(envelopes.len(), 1);
        assert_eq!(envelopes[0].provider_event_id, "wamid.ABC123");
        assert_eq!(envelopes[0].sender.sender_id, "15550001111");
        assert_eq!(
            envelopes[0].sender.display_label.as_deref(),
            Some("Ada Lovelace")
        );
        assert_eq!(envelopes[0].text, "hello there");
        assert!(matches!(
            envelopes[0].conversation.kind,
            little_monkey_lib::channels::types::ConversationKind::Direct
        ));
    }

    #[test]
    fn a_one_byte_changed_body_fails_verification_and_yields_no_envelopes() {
        let adapter = adapter("app-secret-value", "token-value");
        let body = text_message_body();
        let signature = sign("app-secret-value", &body);
        let mut tampered = body.clone();
        let last = tampered.len() - 1;
        tampered[last] ^= 0x01;
        let result = adapter.verify_and_normalize(
            &[("x-hub-signature-256".to_string(), signature)],
            &tampered,
            None,
            0,
        );
        assert!(result.is_err());
    }

    #[test]
    fn a_signature_from_the_wrong_secret_fails() {
        let adapter = adapter("app-secret-value", "token-value");
        let body = text_message_body();
        let signature = sign("wrong-secret", &body);
        let result = adapter.verify_and_normalize(
            &[("x-hub-signature-256".to_string(), signature)],
            &body,
            None,
            0,
        );
        assert!(result.is_err());
    }

    /// The adapter as Meta's dashboard meets it during setup: same secrets
    /// plus the verify token the operator typed into the subscription form.
    fn adapter_with_verify_token(verify_token: &str) -> WhatsAppAdapter {
        let account = test_account(serde_json::json!({ "phone_number_id": "1234567890" }));
        let secret = serde_json::json!({
            "app_secret": "s3cret",
            "access_token": "tok",
            "verify_token": verify_token,
        })
        .to_string();
        WhatsAppAdapter::new(&AdapterConfig {
            account: &account,
            secret,
        })
        .expect("adapter builds")
    }

    fn upload_adapter(base: &str, bytes: &[u8]) -> WhatsAppAdapter {
        let account = test_account(serde_json::json!({ "phone_number_id": "1234567890" }));
        let secret = serde_json::json!({
            "app_secret": "s3cret",
            "access_token": "tok",
        })
        .to_string();
        WhatsAppAdapter::new(&AdapterConfig {
            account: &account,
            secret,
        })
        .expect("adapter builds")
        .with_base_url(base)
        .with_blobs(std::sync::Arc::new(
            crate::daemon::channel_adapter::test_http::FixtureBlobs(bytes.to_vec()),
        ))
    }

    fn message_with_file(filename: &str) -> OutboundMessage {
        OutboundMessage {
            account_id: "acct-wa".into(),
            kind: ChannelKind::WhatsApp,
            conversation_id: "15550001111".into(),
            thread_id: None,
            text: String::new(),
            attachments: vec![little_monkey_lib::channels::types::OutboundAttachment {
                artifact_id: "blob-1".into(),
                filename: Some(filename.to_string()),
                mime_type: None,
            }],
            reply_to_provider_id: None,
            idempotency_key: "reply-1".into(),
        }
    }

    #[tokio::test]
    async fn a_file_is_uploaded_first_and_then_sent_by_its_media_id() {
        let (base, requests) = crate::daemon::channel_adapter::test_http::serve(vec![
            (200, r#"{"id":"media-77"}"#.to_string()),
            (200, r#"{"messages":[{"id":"wamid.OUT9"}]}"#.to_string()),
        ]);
        let adapter = upload_adapter(&base, b"%PDF-1.7 report");

        let outcome = adapter.send(&message_with_file("report.pdf")).await;
        assert!(
            matches!(&outcome, SendOutcome::Sent { provider_message_id } if provider_message_id.as_deref() == Some("wamid.OUT9")),
            "{outcome:?}"
        );

        let upload = requests
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("upload request");
        let upload_text = String::from_utf8_lossy(&upload);
        assert!(
            upload_text.starts_with("POST /1234567890/media"),
            "{upload_text}"
        );
        assert!(upload_text.contains("multipart/form-data"), "{upload_text}");
        assert!(
            upload_text.contains("name=\"messaging_product\""),
            "the Cloud API rejects an upload that does not name the product"
        );
        assert!(
            upload_text.contains("filename=\"report.pdf\""),
            "{upload_text}"
        );
        assert!(
            upload
                .windows(b"%PDF-1.7 report".len())
                .any(|w| w == b"%PDF-1.7 report"),
            "the uploaded bytes are missing from the request"
        );

        let message = requests
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("message request");
        let message_text = String::from_utf8_lossy(&message);
        assert!(
            message_text.starts_with("POST /1234567890/messages"),
            "{message_text}"
        );
        // The id the upload returned is what the message carries — bytes are
        // never inlined into a message body on this API.
        assert!(
            message_text.contains(r#""id":"media-77""#),
            "{message_text}"
        );
        assert!(
            message_text.contains(r#""type":"document""#),
            "{message_text}"
        );
        assert!(
            message_text.contains(r#""filename":"report.pdf""#),
            "{message_text}"
        );
    }

    #[tokio::test]
    async fn a_rejected_upload_never_becomes_a_message() {
        let (base, requests) = crate::daemon::channel_adapter::test_http::serve(vec![(
            400,
            r#"{"error":{"message":"Unsupported file type","code":100}}"#.to_string(),
        )]);
        let adapter = upload_adapter(&base, b"bytes");

        let outcome = adapter.send(&message_with_file("thing.xyz")).await;
        assert!(
            matches!(&outcome, SendOutcome::PermanentFailure { error } if error.contains("Unsupported file type")),
            "{outcome:?}"
        );
        requests
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("upload request");
        assert!(
            requests
                .recv_timeout(std::time::Duration::from_millis(200))
                .is_err(),
            "a failed upload must not be followed by a message naming a media id that does not exist"
        );
    }

    #[test]
    fn only_the_two_image_types_whatsapp_accepts_are_sent_as_images() {
        assert_eq!(media_type_for("image/png"), "image");
        assert_eq!(media_type_for("image/jpeg"), "image");
        // Accepted by the API as a file, rejected as an image.
        assert_eq!(media_type_for("image/svg+xml"), "document");
        assert_eq!(media_type_for("image/webp"), "document");
        assert_eq!(media_type_for("audio/mpeg"), "audio");
        assert_eq!(media_type_for("video/mp4"), "video");
        assert_eq!(media_type_for("application/pdf"), "document");
    }

    #[test]
    fn a_failed_status_reports_the_reason_the_message_never_arrived() {
        let body = serde_json::json!({
            "entry": [{
                "changes": [{
                    "value": {
                        "messaging_product": "whatsapp",
                        "statuses": [{
                            "id": "wamid.OUT1",
                            "status": "failed",
                            "recipient_id": "15550001111",
                            "errors": [{
                                "code": 131047,
                                "title": "Re-engagement message"
                            }]
                        }]
                    }
                }]
            }]
        })
        .to_string();
        let receipts = adapter("s3cret", "tok").delivery_receipts(body.as_bytes(), 1_700_000_000);
        assert_eq!(receipts.len(), 1);
        assert_eq!(receipts[0].provider_message_id, "wamid.OUT1");
        assert_eq!(receipts[0].state, DeliveryState::Failed);
        assert_eq!(
            receipts[0].error.as_deref(),
            Some("Re-engagement message (WhatsApp error 131047)")
        );
        assert_eq!(receipts[0].observed_at_ms, 1_700_000_000);
    }

    #[test]
    fn every_progress_state_is_reported_and_an_unknown_one_is_not() {
        let body = serde_json::json!({
            "entry": [{
                "changes": [{
                    "value": {
                        "statuses": [
                            {"id": "m1", "status": "sent"},
                            {"id": "m2", "status": "delivered"},
                            {"id": "m3", "status": "read"},
                            {"id": "m4", "status": "warped"},
                            {"status": "delivered"}
                        ]
                    }
                }]
            }]
        })
        .to_string();
        let receipts = adapter("s3cret", "tok").delivery_receipts(body.as_bytes(), 0);
        assert_eq!(
            receipts
                .iter()
                .map(|receipt| (receipt.provider_message_id.as_str(), receipt.state))
                .collect::<Vec<_>>(),
            vec![
                ("m1", DeliveryState::Sent),
                ("m2", DeliveryState::Delivered),
                ("m3", DeliveryState::Read),
            ],
            "an unrecognized status word and an id-less entry are both dropped"
        );
    }

    #[test]
    fn a_body_carrying_only_messages_reports_no_receipts() {
        let receipts = adapter("s3cret", "tok").delivery_receipts(&text_message_body(), 0);
        assert!(receipts.is_empty());
    }

    #[test]
    fn a_correct_verify_token_echoes_the_challenge() {
        let adapter = adapter_with_verify_token("chosen-by-the-operator");
        let answer = adapter.verification_challenge(
            "hub.mode=subscribe&hub.verify_token=chosen-by-the-operator&hub.challenge=1158201444",
        );
        assert_eq!(answer.as_deref(), Some("1158201444"));
    }

    #[test]
    fn a_percent_encoded_challenge_is_echoed_decoded() {
        let adapter = adapter_with_verify_token("tok en");
        let answer = adapter.verification_challenge(
            "hub.mode=subscribe&hub.verify_token=tok%20en&hub.challenge=a%2Bb%20c",
        );
        assert_eq!(answer.as_deref(), Some("a+b c"));
    }

    #[test]
    fn a_wrong_verify_token_answers_nothing() {
        let adapter = adapter_with_verify_token("chosen-by-the-operator");
        assert!(adapter
            .verification_challenge(
                "hub.mode=subscribe&hub.verify_token=guessed&hub.challenge=1158201444"
            )
            .is_none());
    }

    #[test]
    fn a_prefix_of_the_verify_token_is_not_close_enough() {
        let adapter = adapter_with_verify_token("chosen-by-the-operator");
        assert!(adapter
            .verification_challenge("hub.mode=subscribe&hub.verify_token=chosen&hub.challenge=x")
            .is_none());
    }

    #[test]
    fn an_account_with_no_verify_token_answers_nothing() {
        // Not even to an empty offered token: an unconfigured account must not
        // be the one endpoint that any challenge passes.
        let adapter = adapter_with_verify_token("");
        assert!(adapter
            .verification_challenge("hub.mode=subscribe&hub.verify_token=&hub.challenge=x")
            .is_none());
    }

    #[test]
    fn a_challenge_without_the_subscribe_mode_is_refused() {
        let adapter = adapter_with_verify_token("chosen-by-the-operator");
        assert!(adapter
            .verification_challenge(
                "hub.mode=unsubscribe&hub.verify_token=chosen-by-the-operator&hub.challenge=x"
            )
            .is_none());
        assert!(adapter
            .verification_challenge("hub.verify_token=chosen-by-the-operator&hub.challenge=x")
            .is_none());
    }

    #[test]
    fn an_account_saved_before_verify_tokens_existed_still_builds() {
        // The credential bundle predating this field must not become
        // unparseable — the account keeps working, it just cannot answer a
        // subscription handshake until the operator saves a token.
        let adapter = adapter("s3cret", "tok");
        assert!(adapter
            .verification_challenge("hub.mode=subscribe&hub.verify_token=&hub.challenge=x")
            .is_none());
    }

    #[test]
    fn a_missing_signature_header_fails() {
        let adapter = adapter("app-secret-value", "token-value");
        let body = text_message_body();
        let result = adapter.verify_and_normalize(&[], &body, None, 0);
        assert!(result.is_err());
    }

    #[test]
    fn an_empty_signature_header_fails() {
        let adapter = adapter("app-secret-value", "token-value");
        let body = text_message_body();
        let result = adapter.verify_and_normalize(
            &[("x-hub-signature-256".to_string(), String::new())],
            &body,
            None,
            0,
        );
        assert!(result.is_err());
    }

    // --- Normalization -------------------------------------------------------

    #[test]
    fn a_status_only_delivery_normalizes_to_no_envelopes() {
        let adapter = adapter("app-secret-value", "token-value");
        let body = status_only_body();
        let signature = sign("app-secret-value", &body);
        let envelopes = adapter
            .verify_and_normalize(
                &[("x-hub-signature-256".to_string(), signature)],
                &body,
                None,
                0,
            )
            .expect("verifies");
        assert!(envelopes.is_empty());
    }

    #[test]
    fn a_media_message_normalizes_to_a_provider_handle_attachment() {
        let adapter = adapter("app-secret-value", "token-value");
        let body = serde_json::json!({
            "entry": [{
                "id": "waba-1",
                "changes": [{
                    "value": {
                        "messages": [{
                            "from": "15550001111",
                            "id": "wamid.IMG1",
                            "timestamp": "1700000000",
                            "type": "image",
                            "image": {"id": "media-id-1", "mime_type": "image/jpeg"}
                        }]
                    },
                    "field": "messages"
                }]
            }]
        })
        .to_string()
        .into_bytes();
        let signature = sign("app-secret-value", &body);
        let envelopes = adapter
            .verify_and_normalize(
                &[("x-hub-signature-256".to_string(), signature)],
                &body,
                None,
                0,
            )
            .expect("verifies");
        assert_eq!(envelopes.len(), 1);
        assert_eq!(envelopes[0].attachments.len(), 1);
        match &envelopes[0].attachments[0].source {
            AttachmentSource::ProviderHandle { handle } => assert_eq!(handle, "media-id-1"),
            other => panic!("expected a provider handle, got {other:?}"),
        }
    }

    #[test]
    fn provider_event_ids_are_deterministic_from_the_message_id() {
        let adapter = adapter("app-secret-value", "token-value");
        let body = text_message_body();
        let signature = sign("app-secret-value", &body);
        let first = adapter
            .verify_and_normalize(
                &[("x-hub-signature-256".to_string(), signature.clone())],
                &body,
                None,
                0,
            )
            .unwrap();
        let second = adapter
            .verify_and_normalize(
                &[("x-hub-signature-256".to_string(), signature)],
                &body,
                None,
                0,
            )
            .unwrap();
        assert_eq!(first[0].provider_event_id, second[0].provider_event_id);
    }

    // --- No secret leakage ----------------------------------------------------

    #[test]
    fn no_secret_appears_in_any_rendered_error_string() {
        let adapter = adapter("super-secret-app-value", "super-secret-token-value");
        let body = text_message_body();
        let bad_signature = format!("sha256={}", "00".repeat(32));
        let error = adapter
            .verify_and_normalize(
                &[("x-hub-signature-256".to_string(), bad_signature)],
                &body,
                None,
                0,
            )
            .unwrap_err();
        assert!(!error.contains("super-secret-app-value"));
        assert!(!error.contains("super-secret-token-value"));
    }

    // --- Outbound mapping -------------------------------------------------

    fn serve_once(status: &str, body: &str) -> String {
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let response = format!(
            "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut scratch = [0u8; 2048];
                let _ = stream.read(&mut scratch);
                let _ = stream.write_all(response.as_bytes());
                let _ = stream.flush();
            }
        });
        format!("http://127.0.0.1:{port}")
    }

    fn outbound_message() -> OutboundMessage {
        OutboundMessage {
            account_id: "acct-wa".to_string(),
            kind: ChannelKind::WhatsApp,
            conversation_id: "15550001111".to_string(),
            thread_id: None,
            text: "hi".to_string(),
            attachments: Vec::new(),
            reply_to_provider_id: None,
            idempotency_key: "idem-1".to_string(),
        }
    }

    #[tokio::test]
    async fn a_429_response_maps_to_retryable_failure() {
        let base = serve_once(
            "429 Too Many Requests",
            r#"{"error":{"message":"rate limited","code":4}}"#,
        );
        let outcome = adapter("s", "t")
            .with_base_url(&base)
            .send(&outbound_message())
            .await;
        assert!(
            matches!(outcome, SendOutcome::RetryableFailure { .. }),
            "{outcome:?}"
        );
    }

    #[tokio::test]
    async fn a_401_response_maps_to_permanent_failure() {
        let base = serve_once(
            "401 Unauthorized",
            r#"{"error":{"message":"bad token","code":190}}"#,
        );
        let outcome = adapter("s", "t")
            .with_base_url(&base)
            .send(&outbound_message())
            .await;
        assert!(
            matches!(outcome, SendOutcome::PermanentFailure { .. }),
            "{outcome:?}"
        );
    }

    #[tokio::test]
    async fn error_code_131047_maps_to_an_actionable_permanent_failure() {
        let base = serve_once(
            "400 Bad Request",
            r#"{"error":{"message":"outside window","code":131047}}"#,
        );
        let outcome = adapter("s", "t")
            .with_base_url(&base)
            .send(&outbound_message())
            .await;
        match outcome {
            SendOutcome::PermanentFailure { error } => assert!(error.contains("template")),
            other => panic!("expected PermanentFailure, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_successful_send_extracts_the_provider_message_id() {
        let base = serve_once("200 OK", r#"{"messages":[{"id":"wamid.OUT1"}]}"#);
        let outcome = adapter("s", "t")
            .with_base_url(&base)
            .send(&outbound_message())
            .await;
        match outcome {
            SendOutcome::Sent {
                provider_message_id,
            } => {
                assert_eq!(provider_message_id.as_deref(), Some("wamid.OUT1"));
            }
            other => panic!("expected Sent, got {other:?}"),
        }
    }
}
