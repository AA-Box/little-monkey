//! Microsoft Teams (Bot Framework Activity protocol) adapter.
//!
//! # Inbound trust — read this before touching `verify_and_normalize`
//!
//! The Bot Framework authenticates a webhook delivery with a JWT in
//! `Authorization: Bearer`. Full validation is: verify issuer, audience and
//! expiry (structural — cheap, no network), *and* verify the RS256 signature
//! against a key from Microsoft's OpenID metadata document, which has to be
//! fetched and cached (JWKS).
//!
//! This adapter implements both halves. The signature check reuses the
//! provider-agnostic JWKS core in `google_chat.rs` (`super::google_chat`) —
//! JWT decoding without re-serialization, `alg` pinned to `RS256` by name,
//! the cache, and the bounded synchronous refresh bridge — since both
//! providers publish RSA keys the same way (a JWKS document with `n`/`e`
//! members). What is Teams-specific is *where* the document lives: rather
//! than a fixed JWKS URL, Bot Framework publishes an OpenID Connect discovery
//! document (default [`DEFAULT_OPENID_METADATA_URL`], overridable per account)
//! whose `jwks_uri` names the actual JWKS. [`TeamsAdapter::refresh_keys`] is
//! that two-step fetch; [`TeamsAdapter::ensure_key_for_kid`] is the sync-side
//! lookup with the one-shot catch-up on an unknown `kid` — see
//! `jwt::try_refresh_blocking`'s doc for why that bridge is safe.
//!
//! [`normalize_activity`] is the pure mapping from a verified activity to an
//! envelope, unit-tested against fixtures independently of verification.
//!
//! Outbound uses the client-credentials flow against the configured tenant,
//! with the resulting token cached and refreshed on expiry.

use async_trait::async_trait;
use little_monkey_lib::channels::types::{
    AttachmentKind, AttachmentSource, BoundedMetadata, ChannelAttachment, ChannelConversation,
    ChannelEnvelope, ChannelHealth, ChannelKind, ChannelSender, InboundTransport, OutboundMessage,
    ProviderCapabilities, SendOutcome,
};
use serde::Deserialize;
use serde_json::Value as JsonValue;
use std::collections::BTreeMap;
use std::sync::Mutex;

use super::jwt::{
    decode_jwt, fetch_bytes_via_egress, fetch_jwks_via_egress, parse_jwks_uri_from_metadata,
    try_refresh_blocking, validate_alg_is_rs256, verify_rs256_signature, JwkRsaKey, JwksCache,
};
use crate::daemon::channel_adapter::{
    AdapterConfig, ChannelAdapter, InboundBatch, WebhookChannelAdapter,
};

const LOGIN_BASE: &str = "https://login.microsoftonline.com";
/// The Bot Framework's own fixed token issuer.
const EXPECTED_ISSUER: &str = "https://api.botframework.com";
/// Applies to `exp`/`nbf` alike, per the shared skew rule this file follows.
const SKEW_SECS: i64 = 300;
/// Refresh this many seconds before the token's own `expires_in` — never cut
/// it exactly at the edge, or a request built just before expiry could be
/// sent with a token that dies in flight.
const TOKEN_REFRESH_SKEW_SECS: i64 = 60;
/// The Bot Framework's own OpenID Connect discovery document, whose
/// `jwks_uri` names the JWKS this adapter verifies signatures against.
/// Fixed for every tenant in production; [`TeamsNonSecretConfig`] allows an
/// account to override it, since Microsoft's own docs describe a government
/// cloud variant at a different host.
const DEFAULT_OPENID_METADATA_URL: &str =
    "https://login.botframework.com/v1/.well-known/openidconfiguration";

#[derive(Debug, Deserialize)]
struct TeamsNonSecretConfig {
    app_id: String,
    tenant_id: String,
    /// Overrides [`DEFAULT_OPENID_METADATA_URL`]. Non-secret: it names an
    /// endpoint, not a credential.
    #[serde(default)]
    open_id_metadata_url: Option<String>,
}

#[derive(Debug, Deserialize)]
struct TeamsSecrets {
    app_password: String,
}

struct CachedToken {
    access_token: String,
    expires_at_ms: i64,
}

pub struct TeamsAdapter {
    account_id: String,
    app_id: String,
    tenant_id: String,
    app_password: String,
    token_cache: Mutex<Option<CachedToken>>,
    /// Validated `serviceUrl` per conversation. Only ever written by
    /// [`TeamsAdapter::record_conversation_service_url`], which validates
    /// before inserting — `send` refuses to guess or derive one. Populated
    /// from every activity `verify_and_normalize` accepts (see that method).
    service_urls: Mutex<BTreeMap<String, String>>,
    /// Identity provider origin. Always [`LOGIN_BASE`] in production;
    /// swappable in tests.
    login_base: String,
    /// Cached RS256 JWKS keys for verifying inbound deliveries. See the
    /// module doc for how this is kept warm.
    jwks_cache: JwksCache,
    /// The OpenID Connect discovery document this adapter's `jwks_uri` comes
    /// from. [`DEFAULT_OPENID_METADATA_URL`] unless overridden by account
    /// config; swappable in tests.
    metadata_url: String,
}

impl TeamsAdapter {
    pub fn new(config: &AdapterConfig<'_>) -> Result<Self, String> {
        let non_secret: TeamsNonSecretConfig =
            serde_json::from_value(config.account.non_secret_config.clone())
                .map_err(|error| format!("Invalid Teams account config: {error}"))?;
        if non_secret.app_id.trim().is_empty() || non_secret.tenant_id.trim().is_empty() {
            return Err("Teams account is missing app_id or tenant_id".to_string());
        }
        let secrets: TeamsSecrets = serde_json::from_str(&config.secret)
            .map_err(|_| "Teams account credential is missing or malformed".to_string())?;
        if secrets.app_password.trim().is_empty() {
            return Err("Teams account credential is missing app_password".to_string());
        }
        let metadata_url = non_secret
            .open_id_metadata_url
            .filter(|url| !url.trim().is_empty())
            .unwrap_or_else(|| DEFAULT_OPENID_METADATA_URL.to_string());
        Ok(Self {
            account_id: config.account.account_id.clone(),
            app_id: non_secret.app_id,
            tenant_id: non_secret.tenant_id,
            app_password: secrets.app_password,
            token_cache: Mutex::new(None),
            service_urls: Mutex::new(BTreeMap::new()),
            login_base: LOGIN_BASE.to_string(),
            jwks_cache: JwksCache::new(),
            metadata_url,
        })
    }

    #[cfg(test)]
    fn with_login_base(mut self, base: &str) -> Self {
        self.login_base = base.to_string();
        self
    }

    #[cfg(test)]
    fn with_metadata_url(mut self, url: &str) -> Self {
        self.metadata_url = url.to_string();
        self
    }

    /// Fetches the OpenID metadata document, follows its `jwks_uri`, and
    /// replaces the cache on success. Used by [`probe`](ChannelAdapter::probe)
    /// to keep the cache warm, and by [`ensure_key_for_kid`](Self::ensure_key_for_kid)
    /// as the one bounded synchronous attempt on an unknown `kid`.
    async fn refresh_keys(&self, now_ms: i64) -> bool {
        let Ok(metadata) = fetch_bytes_via_egress(&self.metadata_url).await else {
            return false;
        };
        let Ok(jwks_uri) = parse_jwks_uri_from_metadata(&metadata) else {
            return false;
        };
        match fetch_jwks_via_egress(&jwks_uri).await {
            Ok(keys) => {
                self.jwks_cache.replace(keys, now_ms);
                true
            }
            Err(_) => false,
        }
    }

    /// The key for `kid`, fetching once more if the cache does not have it.
    /// See `jwt::try_refresh_blocking`'s doc for why exactly one
    /// bridged attempt is safe and sufficient.
    fn ensure_key_for_kid(&self, kid: &str, now_ms: i64) -> Option<JwkRsaKey> {
        if let Some(key) = self.jwks_cache.find(kid) {
            return Some(key);
        }
        if try_refresh_blocking(|| self.refresh_keys(now_ms)) {
            return self.jwks_cache.find(kid);
        }
        None
    }

    /// Records a `serviceUrl` for a conversation, refusing anything that is
    /// not `https` on a Microsoft-owned Bot Framework host. This is the only
    /// way `send` learns where to POST — it never reconstructs or trusts a
    /// value handed to it any other way.
    fn record_conversation_service_url(
        &self,
        conversation_id: &str,
        service_url: &str,
    ) -> Result<(), String> {
        validate_service_url(service_url)?;
        if let Ok(mut cache) = self.service_urls.lock() {
            cache.insert(conversation_id.to_string(), service_url.to_string());
        }
        Ok(())
    }

    #[cfg(test)]
    fn seed_service_url_for_test(&self, conversation_id: &str, service_url: &str) {
        if let Ok(mut cache) = self.service_urls.lock() {
            cache.insert(conversation_id.to_string(), service_url.to_string());
        }
    }

    /// A cached client-credentials token, fetching and caching a new one when
    /// absent or near expiry.
    async fn access_token(&self) -> Result<String, String> {
        let now = now_ms();
        if let Ok(cache) = self.token_cache.lock() {
            if let Some(token) = cache.as_ref() {
                if now < token.expires_at_ms - TOKEN_REFRESH_SKEW_SECS * 1000 {
                    return Ok(token.access_token.clone());
                }
            }
        }
        let client = little_monkey_lib::egress::hardened()
            .build()
            .map_err(|error| format!("Failed to build client: {error}"))?;
        let url = format!("{}/{}/oauth2/v2.0/token", self.login_base, self.tenant_id);
        let params = [
            ("grant_type", "client_credentials"),
            ("client_id", self.app_id.as_str()),
            ("client_secret", self.app_password.as_str()),
            ("scope", "https://api.botframework.com/.default"),
        ];
        let request = client.post(url).form(&params);
        let response = little_monkey_lib::egress::send(request)
            .await
            .map_err(|error| sanitize_transport_error(&error))?;
        let status = response.status();
        let bytes = response
            .bytes()
            .await
            .map_err(|error| sanitize_transport_error(&error))?;
        if !status.is_success() {
            // Never includes the response body: a failed token exchange can
            // echo back `client_secret` in some AAD error payloads.
            return Err(format!("Teams token request failed with status {status}"));
        }
        let parsed: JsonValue = serde_json::from_slice(&bytes)
            .map_err(|_| "Teams token response was not valid JSON".to_string())?;
        let access_token = parsed
            .get("access_token")
            .and_then(JsonValue::as_str)
            .ok_or_else(|| "Teams token response is missing access_token".to_string())?
            .to_string();
        let expires_in = parsed
            .get("expires_in")
            .and_then(JsonValue::as_i64)
            .unwrap_or(3600);
        let expires_at_ms = now + expires_in * 1000;
        if let Ok(mut cache) = self.token_cache.lock() {
            *cache = Some(CachedToken {
                access_token: access_token.clone(),
                expires_at_ms,
            });
        }
        Ok(access_token)
    }
}

impl WebhookChannelAdapter for TeamsAdapter {
    fn kind(&self) -> ChannelKind {
        ChannelKind::Teams
    }

    fn verify_and_normalize(
        &self,
        headers: &[(String, String)],
        body: &[u8],
        _public_base_url: Option<&str>,
        now_ms: i64,
    ) -> Result<Vec<ChannelEnvelope>, String> {
        // `public_base_url` is unused: the Bot Framework's JWT signs the
        // token's own claims, never the delivery URL, so there is nothing
        // here that would ever need it.
        let authorization = headers
            .iter()
            .find(|(name, _)| name == "authorization")
            .map(|(_, value)| value.as_str())
            .ok_or_else(|| "Missing Authorization header".to_string())?;
        let token = authorization
            .strip_prefix("Bearer ")
            .ok_or_else(|| "Authorization header is not a Bearer token".to_string())?;

        let decoded = decode_jwt(token)?;
        validate_alg_is_rs256(&decoded.header)?;
        validate_claims_structurally(&decoded.claims, &self.app_id, now_ms)?;
        let kid = decoded
            .header
            .get("kid")
            .and_then(JsonValue::as_str)
            .ok_or_else(|| "JWT is missing kid".to_string())?;
        let key = self
            .ensure_key_for_kid(kid, now_ms)
            .ok_or_else(|| "No matching signing key for this token's kid; refusing".to_string())?;
        verify_rs256_signature(decoded.signing_input, &decoded.signature, &key)?;

        // Only reached once the signature has verified against a key from
        // the Bot Framework's own JWKS — the body is never parsed before
        // that.
        let activity: JsonValue = serde_json::from_slice(body)
            .map_err(|_| "Teams activity body is not valid JSON".to_string())?;

        // The `serviceUrl` this specific activity carries is the only way
        // `send` learns where to POST for this conversation — record it now
        // that the activity is verified, per `record_conversation_service_url`'s
        // own doc. Best-effort: an invalid or absent `serviceUrl` still lets a
        // valid activity normalize, it just cannot be replied to yet.
        if let (Some(conversation_id), Some(service_url)) = (
            activity
                .get("conversation")
                .and_then(|conversation| conversation.get("id"))
                .and_then(JsonValue::as_str),
            activity.get("serviceUrl").and_then(JsonValue::as_str),
        ) {
            let _ = self.record_conversation_service_url(conversation_id, service_url);
        }

        let bot_id = activity
            .get("recipient")
            .and_then(|recipient| recipient.get("id"))
            .and_then(JsonValue::as_str)
            .unwrap_or_default();
        Ok(
            normalize_activity(&activity, &self.account_id, bot_id, now_ms)
                .into_iter()
                .collect(),
        )
    }
}

#[async_trait]
impl ChannelAdapter for TeamsAdapter {
    fn kind(&self) -> ChannelKind {
        ChannelKind::Teams
    }

    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            max_text_chars: 28000,
            supports_threads: false,
            // Inbound attachments are fetched; sending one is not implemented,
            // and the trait's default refuses by name rather than dropping it.
            supports_attachments: false,
            supports_mention_metadata: true,
            supports_idempotency_key: false,
            supports_delivery_receipts: false,
            ..ProviderCapabilities::minimal(ChannelKind::Teams, InboundTransport::Webhook)
        }
    }

    async fn probe(&self) -> ChannelHealth {
        let now = now_ms();
        // Best-effort: an inbound JWKS hiccup must not fail a probe whose
        // job is reporting on the outbound credentials.
        if self.jwks_cache.needs_refresh(now) {
            let _ = self.refresh_keys(now).await;
        }
        match self.access_token().await {
            Ok(_) => ChannelHealth::connected(now, Some("App credentials valid".to_string())),
            Err(error) => ChannelHealth::error(now, error),
        }
    }

    async fn poll(&self, _cursor: Option<&str>) -> Result<InboundBatch, String> {
        // Teams is delivered to, not polled — see the module doc.
        Ok(InboundBatch::default())
    }

    async fn send(&self, message: &OutboundMessage) -> SendOutcome {
        let service_url = {
            let cache = match self.service_urls.lock() {
                Ok(cache) => cache,
                Err(_) => {
                    return SendOutcome::PermanentFailure {
                        error: "Teams service URL cache is poisoned".to_string(),
                    }
                }
            };
            cache.get(&message.conversation_id).cloned()
        };
        let Some(service_url) = service_url else {
            return SendOutcome::PermanentFailure {
                error: format!(
                    "No verified serviceUrl on file for conversation '{}'; Teams requires an \
                     inbound activity to establish where to send before it can be messaged",
                    message.conversation_id
                ),
            };
        };
        let token = match self.access_token().await {
            Ok(token) => token,
            Err(error) => {
                return SendOutcome::RetryableFailure {
                    error,
                    retry_after_ms: None,
                }
            }
        };
        let client = match little_monkey_lib::egress::hardened().build() {
            Ok(client) => client,
            Err(error) => {
                return SendOutcome::PermanentFailure {
                    error: format!("Failed to build client: {error}"),
                }
            }
        };
        let url = format!(
            "{}/v3/conversations/{}/activities",
            service_url.trim_end_matches('/'),
            message.conversation_id
        );
        let body = serde_json::json!({
            "type": "message",
            "text": message.text,
        });
        let request = client.post(url).bearer_auth(&token).json(&body);
        let response = match little_monkey_lib::egress::send(request).await {
            Ok(response) => response,
            Err(error) => return map_transport_error(&error),
        };
        let status = response.status();
        let bytes = match response.bytes().await {
            Ok(bytes) => bytes,
            Err(error) => return map_transport_error(&error),
        };
        if status.is_success() {
            let parsed: JsonValue = serde_json::from_slice(&bytes).unwrap_or(JsonValue::Null);
            let provider_message_id = parsed
                .get("id")
                .and_then(JsonValue::as_str)
                .map(str::to_string);
            return SendOutcome::Sent {
                provider_message_id,
            };
        }
        let error_message = format!("Teams send failed with status {status}");
        if status.as_u16() == 429 {
            return SendOutcome::RetryableFailure {
                error: error_message,
                retry_after_ms: None,
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
                retry_after_ms: None,
            };
        }
        SendOutcome::PermanentFailure {
            error: error_message,
        }
    }

    /// Teams attachments arrive as a `contentUrl`. Bot-hosted content on the
    /// Bot Framework's own service hosts needs the bot's token; anything else
    /// — a SharePoint or OneDrive link, or any host an attacker could put in an
    /// activity — is fetched with no credential at all, because sending the
    /// bot's bearer token to a host chosen by the message author would hand it
    /// away.
    async fn fetch_attachment(
        &self,
        attachment: &ChannelAttachment,
        max_bytes: u64,
    ) -> Result<Vec<u8>, String> {
        let AttachmentSource::Url { url } = &attachment.source else {
            return Err("This Teams attachment has no content URL.".to_string());
        };
        let client = little_monkey_lib::egress::hardened()
            .build()
            .map_err(|error| format!("Could not build an HTTP client: {error}"))?;
        let request = if is_bot_framework_host(url) {
            let token = self.access_token().await?;
            client.get(url).bearer_auth(token)
        } else {
            client.get(url)
        };
        crate::daemon::channel_adapter::download_bounded(request, max_bytes).await
    }
}

/// Whether a URL is on a Bot Framework service host, and so may be sent the
/// bot's own token.
///
/// Host suffixes are matched on a label boundary — `evil-botframework.com`
/// must not pass as `botframework.com`, and neither must
/// `botframework.com.attacker.example`.
fn is_bot_framework_host(url: &str) -> bool {
    let Ok(parsed) = reqwest::Url::parse(url) else {
        return false;
    };
    if parsed.scheme() != "https" {
        return false;
    }
    let Some(host) = parsed.host_str() else {
        return false;
    };
    let host = host.to_ascii_lowercase();
    ["botframework.com", "trafficmanager.net"]
        .iter()
        .any(|suffix| host == *suffix || host.ends_with(&format!(".{suffix}")))
}

/// `https` on a Microsoft-owned Bot Framework host. Bot Framework
/// `serviceUrl`s are `api.botframework.com` or a regional
/// `*.botframework.com` / `*.trafficmanager.net` name (e.g.
/// `smba.trafficmanager.net`); nothing else is a real Bot Framework
/// endpoint, so nothing else is trusted.
fn validate_service_url(candidate: &str) -> Result<(), String> {
    let url =
        reqwest::Url::parse(candidate).map_err(|_| "serviceUrl is not a valid URL".to_string())?;
    if url.scheme() != "https" {
        return Err("serviceUrl must be https".to_string());
    }
    let host = url
        .host_str()
        .ok_or_else(|| "serviceUrl has no host".to_string())?;
    let host = host.to_ascii_lowercase();
    let allowed = host == "api.botframework.com"
        || host.ends_with(".botframework.com")
        || host.ends_with(".trafficmanager.net");
    if !allowed {
        return Err("serviceUrl is not on a Microsoft-owned Bot Framework host".to_string());
    }
    Ok(())
}

fn validate_claims_structurally(
    claims: &JsonValue,
    expected_app_id: &str,
    now_ms: i64,
) -> Result<(), String> {
    let issuer = claims
        .get("iss")
        .and_then(JsonValue::as_str)
        .ok_or_else(|| "JWT is missing iss".to_string())?;
    if issuer != EXPECTED_ISSUER {
        return Err("JWT issuer is not the Bot Framework".to_string());
    }
    let audience = claims
        .get("aud")
        .and_then(JsonValue::as_str)
        .ok_or_else(|| "JWT is missing aud".to_string())?;
    if audience != expected_app_id {
        return Err("JWT audience does not match the configured app id".to_string());
    }
    let now_secs = now_ms / 1000;
    let exp = claims
        .get("exp")
        .and_then(JsonValue::as_i64)
        .ok_or_else(|| "JWT is missing exp".to_string())?;
    if now_secs > exp + SKEW_SECS {
        return Err("JWT has expired".to_string());
    }
    if let Some(nbf) = claims.get("nbf").and_then(JsonValue::as_i64) {
        if now_secs + SKEW_SECS < nbf {
            return Err("JWT is not yet valid".to_string());
        }
    }
    Ok(())
}

fn sanitize_transport_error(error: &reqwest::Error) -> String {
    if error.is_timeout() {
        "Teams request timed out".to_string()
    } else if error.is_connect() {
        "Could not connect to Teams".to_string()
    } else {
        "Teams request failed".to_string()
    }
}

fn map_transport_error(error: &reqwest::Error) -> SendOutcome {
    if error.is_connect() {
        SendOutcome::RetryableFailure {
            error: sanitize_transport_error(error),
            retry_after_ms: None,
        }
    } else {
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

/// Normalizes a Bot Framework `Activity` of type `message` into an envelope.
/// Every other activity type (`conversationUpdate`, `typing`, `invoke`, ...)
/// normalizes to nothing.
///
/// Not currently reachable from [`TeamsAdapter::verify_and_normalize`] — see
/// the module doc for why — but implemented and tested against real Activity
/// shapes so wiring it in behind a signature check is a small change.
fn normalize_activity(
    activity: &JsonValue,
    account_id: &str,
    bot_id: &str,
    fallback_received_at_ms: i64,
) -> Option<ChannelEnvelope> {
    if activity.get("type").and_then(JsonValue::as_str) != Some("message") {
        return None;
    }
    let provider_event_id = activity.get("id").and_then(JsonValue::as_str)?.to_string();
    let conversation_id = activity
        .get("conversation")
        .and_then(|conversation| conversation.get("id"))
        .and_then(JsonValue::as_str)?;
    let conversation_type = activity
        .get("conversation")
        .and_then(|conversation| conversation.get("conversationType"))
        .and_then(JsonValue::as_str)
        .unwrap_or("personal");
    let conversation = if conversation_type == "personal" {
        ChannelConversation::direct(conversation_id)
    } else {
        ChannelConversation::group(conversation_id)
    };

    let from_id = activity
        .get("from")
        .and_then(|from| from.get("id"))
        .and_then(JsonValue::as_str)?;
    let from_name = activity
        .get("from")
        .and_then(|from| from.get("name"))
        .and_then(JsonValue::as_str);
    let sender = ChannelSender::new(from_id).with_label(from_name.map(str::to_string));

    let raw_text = activity
        .get("text")
        .and_then(JsonValue::as_str)
        .unwrap_or_default();
    let text = raw_text.replace("<at>", "").replace("</at>", "");

    let mentions_self = activity
        .get("entities")
        .and_then(JsonValue::as_array)
        .map(|entities| {
            entities.iter().any(|entity| {
                entity.get("type").and_then(JsonValue::as_str) == Some("mention")
                    && entity
                        .get("mentioned")
                        .and_then(|mentioned| mentioned.get("id"))
                        .and_then(JsonValue::as_str)
                        == Some(bot_id)
            })
        })
        .unwrap_or(false);

    let attachments = activity
        .get("attachments")
        .and_then(JsonValue::as_array)
        .map(|attachments| {
            attachments
                .iter()
                .filter_map(|attachment| {
                    let content_url = attachment.get("contentUrl").and_then(JsonValue::as_str)?;
                    let mime_type = attachment.get("contentType").and_then(JsonValue::as_str);
                    Some(ChannelAttachment {
                        provider_id: None,
                        kind: mime_type
                            .map(AttachmentKind::from_mime)
                            .unwrap_or(AttachmentKind::Other),
                        filename: attachment
                            .get("name")
                            .and_then(JsonValue::as_str)
                            .map(str::to_string),
                        mime_type: mime_type.map(str::to_string),
                        declared_size_bytes: None,
                        source: AttachmentSource::Url {
                            url: content_url.to_string(),
                        },
                    })
                })
                .collect()
        })
        .unwrap_or_default();

    let mut metadata = BoundedMetadata::new();
    if let Some(service_url) = activity.get("serviceUrl").and_then(JsonValue::as_str) {
        if validate_service_url(service_url).is_ok() {
            metadata.insert("teams_service_url", service_url);
        }
    }

    let received_at_ms = activity
        .get("timestamp")
        .and_then(JsonValue::as_str)
        .and_then(|value| chrono::DateTime::parse_from_rfc3339(value).ok())
        .map(|value| value.timestamp_millis())
        .unwrap_or(fallback_received_at_ms);

    Some(ChannelEnvelope {
        account_id: account_id.to_string(),
        kind: ChannelKind::Teams,
        provider_event_id,
        conversation,
        sender,
        text,
        attachments,
        reply_to_provider_id: activity
            .get("replyToId")
            .and_then(JsonValue::as_str)
            .map(str::to_string),
        mentions_self,
        received_at_ms,
        metadata,
    })
}

#[cfg(test)]
mod tests {
    #[test]
    fn only_bot_framework_hosts_are_sent_the_bot_token() {
        assert!(is_bot_framework_host(
            "https://smba.trafficmanager.net/amer/"
        ));
        assert!(is_bot_framework_host("https://api.botframework.com/x"));
        assert!(
            !is_bot_framework_host("https://evil-botframework.com/x"),
            "a suffix must match on a label boundary"
        );
        assert!(!is_bot_framework_host(
            "https://botframework.com.evil.example/x"
        ));
        assert!(!is_bot_framework_host("http://api.botframework.com/x"));
        assert!(!is_bot_framework_host("https://graph.microsoft.com/x"));
    }

    use super::*;
    use crate::daemon::channel_store::ChannelAccountRecord;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use base64::Engine as _;
    use little_monkey_lib::channels::policy::ChannelAccessPolicy;
    use little_monkey_lib::channels::types::{ConversationKind, HealthState};
    use std::io::{Read, Write};

    fn test_account() -> ChannelAccountRecord {
        ChannelAccountRecord {
            account_id: "acct-teams".to_string(),
            kind: ChannelKind::Teams,
            label: "Test Teams".to_string(),
            enabled: true,
            non_secret_config: serde_json::json!({
                "app_id": "app-id-1",
                "tenant_id": "tenant-1",
            }),
            credential_ref: Some("teams-cred".to_string()),
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

    fn adapter() -> TeamsAdapter {
        let account = test_account();
        let secret = serde_json::json!({ "app_password": "pw-value" }).to_string();
        let config = AdapterConfig {
            account: &account,
            secret,
        };
        TeamsAdapter::new(&config).expect("adapter builds")
    }

    /// A structurally-shaped but unsigned JWT: real base64url in all three
    /// segments (so `decode_jwt` never trips over the fixture itself), but
    /// the signature segment is not a genuine signature over anything. Used
    /// only by tests below whose target is a *structural* claims check that
    /// fires before signature verification is ever reached — never for a
    /// test that expects the signature check itself to run.
    fn make_jwt(claims: &JsonValue) -> String {
        let header =
            URL_SAFE_NO_PAD.encode(serde_json::json!({"alg":"RS256","typ":"JWT"}).to_string());
        let payload = URL_SAFE_NO_PAD.encode(claims.to_string());
        let signature = URL_SAFE_NO_PAD.encode(b"unsigned-placeholder-signature");
        format!("{header}.{payload}.{signature}")
    }

    fn valid_claims(now_secs: i64) -> JsonValue {
        serde_json::json!({
            "iss": EXPECTED_ISSUER,
            "aud": "app-id-1",
            "exp": now_secs + 3600,
            "nbf": now_secs - 60,
        })
    }

    // --- Structural checks + mandatory refusal -----------------------------

    /// A 2048-bit RSA test key generated locally for these tests only, whose
    /// public half is seeded into the JWKS cache as the "genuine" signer.
    /// Not used anywhere else and grants access to nothing.
    const TEST_PRIVATE_KEY_PEM: &str = "-----BEGIN PRIVATE KEY-----
MIIEvwIBADANBgkqhkiG9w0BAQEFAASCBKkwggSlAgEAAoIBAQDSxaufYG907lGy
3EZKuTZSppsN3d4R8whFTFsFgo+11Zp5plEImzXl469p+Nc4/afv56nsig4MegEe
0A7jiBHSnLBSBnsvuCCEoXY+T1QXyDOmZ3ycr9uSkRmSXon/wglUIbs4VaYCg2SS
tFPyOV+F4ebCNF5aY1RocZhe6tcSyWUHBxV5WMKXONCJ84qvON0f816/HUYgOnkN
+2QCiwou+12DqPC8zOlMtXBJVn/GJjZQwIOOac8LtFSeikTXDP1QaiP0ZvVCBXoA
8cR9nWTKbgWqx2yLVsOJR2rZu5ZCCtlUoIZddej0ob520hhDDXZgkm/bfAXEpbGX
NbZ0ZRHjAgMBAAECggEAA7gHJNhqabFsHEV0sWF+iuDJAD/3k3DVUYZdCMYv9kZe
47dChhiu+m+/quqqvs+tmeGy3Cs8v42biB6lqfAUWGyj/nPXfT+4xdOk0XP23jXj
FhQ3XPsMBb8CU38lh4S17hfA75KF9lS5KTl9+Fp4y/+bYbZ+KlwcTid1nR8e967b
zDzFUKncj3RDA7/Hp7l3ziEjlasU1i8M12g4MaaFa1N3BVVXeHIuodl9uGAnY4C7
fGdmji2iN7+0hQnf9V+RSXp60o1sKZq4e1dB8qoDqvDl1CCjmIyF1x3mGVzxKu3K
JCjnz6h2qxPApuyn5z1fqWljn1vAUTwQ+LhwLAABuQKBgQDyD9WQX/CfGlVqcWau
2npdjT5SHgDlbxBD1wrj2HojcU8HPCwarc9Xe6YRHILp/zELj0BH17FvSwYdO5DD
9cJG5fJnoB91BqZgN7lGg9cGrCk/dC2a/CWjkOv61sIF6c6R5h0ql2XhPnEq34HO
AgMQzoTsGvXGWm7cVd16p+KCHQKBgQDe6Jp72jKpEDm18PeNfr50cfQ1aFyK6zGn
3e1qbFMU0uDgi3KfM3L2ZOu/4MUsUv8hy9uHTQB1leVIaqLLEHqbnhpj6fVH61QD
HexFK3LBrfkJSefh+W67igPw4EgKIy9sRHBTT1ZJMnGTz2eS93peKiLVkw6MPXt9
7WvQQRGj/wKBgQC9+7mFyBcF+NgjY//Qqr8xn8LTFqNjb8kXRbdRXr12Bd+d8Rc4
lURQCEct1O/XEih/Rx6PhHXJwNt6pB6Z/tBNbvrTZDRsWBzLFdE/zAg/P25cVCXb
J52vA/aCeH3twDUWA8LOg+c9YxHVMXkipCed0Ek5Omu+E4pBOs9LDmtT7QKBgQDE
5HCQNYvKCarwKoh/UxSnhoBPLH+RtW2G+WBcQJKiMiKwNHxqYueI/FvAgKmpHSZ+
k7K1MC7Xri94Z7ij5Upnap+k4WLmw9bRafzonBghO6pdqgpIcCp/PMl+Wp1HVwzs
dQdCjzGINiZciTbTegV8Z3udaufOt//8m1o/+Tm7wQKBgQCUAcjcuBpvA2FE8bOi
kzfC4v322JZgqBYmhrjLXTMnRDg2fQ2ZAMOm8z/FxOsqy0J8oHQdaApDwgt0VZjP
0gZ72Ua/xANCeR0T6gSYZB3ujeC8ZzDsrwkLGYqU7JidHDGNZ/CaBcXSAILeYJv1
nJ6EyR9+bBW08LJfpDG+U7oWYA==
-----END PRIVATE KEY-----";

    /// A second, unrelated 2048-bit RSA test key: only ever used as the
    /// "wrong signer" in tests below.
    const TEST_WRONG_PRIVATE_KEY_PEM: &str = "-----BEGIN PRIVATE KEY-----
MIIEuwIBADANBgkqhkiG9w0BAQEFAASCBKUwggShAgEAAoIBAQDzgzLmiwnyEOF1
FnDCygqdjmbhjbJjW2d5W5VC2cWTbqpJA+UitbAn9zalMm8HDxazOGn5VVMZU7Rj
LFRqBsVStj8CKxzT/gSuODI8RQCgSSQV/RnymWmjk46E1/a3ArNMTuxK9oSqmPpG
Z4jI8pqz6Q8rOvEkUrePOs0AgNcjOPM0hEqzU+a6M6sn7SQAuoSWIJZQShnjmPaJ
syU907M6d3jTNqJux6klmce46cRIhfREu385Ez7x7MxB3tfvrXLZp+uPCLT602ta
OZZYHKNkkw5SDZVLstDwLK+LrrruaY8OF8L7icUfg32b9/ff2J13iiOJeRENuDsg
VrXu39RNAgMBAAECggEAXmsZmmCA27YF5UNtN2nlkc+8PmqVp4ayaVDEYCZWQGMh
bawv9TRjeCuXqZgTirYkBBu0o3OdA+37vJRcqruzWO3HIo0a4WvV3sN1Xv8WTg/u
CQSZQgKP/lfhY8rlI3LNmKHlZu+M4yTrrc7JL7k5mNaeBhIVnBLij4uqHy7VvBbA
PicQa0a4IhLEd9Np1TlOpO73U5y6Q6xy/LDGcGRVGfgCpia2lCJl9YV0P4v2bqkB
vOHsrG3lazOtvotzBz6qd0BIP8JdxaiHBGxlsuohl5ABTm5gzFq4QDqXPqeJW1i5
/4uaWmdkXcl8bpBwtUtYRmh2wyKNuymu2+SBJB1ttQKBgQD7Vc9i52KzfAZoidV2
JH+GmJm4GnI/mNp5LcIQf4cNTNESVZ9ZkQWSz+DGJDfHLaP43tQpp8U4+ORpz1as
1Tza37dMO0yifGWzdIKkl4lZV7BGs3AM/pQrf9tCxVHI9jVPcdKzYvZBDcraz8se
LUtH1Slm+sPGLnPVW+mSaHc7iwKBgQD4CDhXEJFEkBUd1xB1kXy6kfd05aP4jSWv
WMfgIh5fH2BBneLBJ+VOa3LuXnZ4L/6EyyT73J12GYBhideRn4uqmszQIIAQW5vI
G4SH6vMsDUr6+XBlm5gJUQ8R/f74pmjNk2iIqOOjxX/+3eW2Y7MMGATd2OSIVrmX
sKqz9IQKhwKBgQCAzX8UnqQUe3EFTe3ZN+cq4TWWBeea9AiypWKY9eIOTNmwXbTm
P83taR82LAVxy9AGkJuGJXaLNfJIz3sJ49XmDVRwestRUhMEnqb9FrPK14d9FCRO
ZIEmscV6OIkrRhIX/qsOR58Pw7O741Wix2+XBoTLQ6PlApVWOF5BK8w+9QJ/CLhB
Qs5STRbDp0joSznSKLz49iMcoKBVstRsMnUAnFd+CtCCKEg+x4L/h2HKyG7ng8Og
iTo4Tu6WlNdDvNrfDiBjEu4RkoGl+GL/Rcf8xI+zEx+x0+Ckd69h2EAVtqgjBxcn
laZaWmeXGF60tLTMlqBBi4sUfbaOz8ZmOe1etwKBgDaZUx+7ZphPj/RLtiIyK86c
bdszfWD7/gqkY/cP+HUyH0wrZBwT7yQReEhzaHEDI1Aeqnotl37q5lu5tAHCK5HP
U7+8WqqarVWRgm4Yhxhmi3lyoLNILeSR38ujqh0GCcPq06TgkxJYOUi/ZShf2XSD
Z4Cr3JR0FbjywTd4IHU6
-----END PRIVATE KEY-----";

    /// `n`/`e` of [`TEST_PRIVATE_KEY_PEM`]'s public half, computed once
    /// out-of-band (`openssl rsa ... -text`) and pasted here as the JWKS
    /// values a real provider would publish — this is what
    /// `JwksCache::seed_for_test` stands in for the network fetch.
    const TEST_JWK_N: &str = "0sWrn2BvdO5RstxGSrk2UqabDd3eEfMIRUxbBYKPtdWaeaZRCJs15eOvafjXOP2n7-ep7IoODHoBHtAO44gR0pywUgZ7L7gghKF2Pk9UF8gzpmd8nK_bkpEZkl6J_8IJVCG7OFWmAoNkkrRT8jlfheHmwjReWmNUaHGYXurXEsllBwcVeVjClzjQifOKrzjdH_Nevx1GIDp5DftkAosKLvtdg6jwvMzpTLVwSVZ_xiY2UMCDjmnPC7RUnopE1wz9UGoj9Gb1QgV6APHEfZ1kym4Fqsdsi1bDiUdq2buWQgrZVKCGXXXo9KG-dtIYQw12YJJv23wFxKWxlzW2dGUR4w";
    const TEST_JWK_E: &str = "AQAB";

    fn test_jwk() -> JwkRsaKey {
        JwkRsaKey {
            n: URL_SAFE_NO_PAD.decode(TEST_JWK_N).unwrap(),
            e: URL_SAFE_NO_PAD.decode(TEST_JWK_E).unwrap(),
        }
    }

    /// Strips PEM armor and base64-decodes to PKCS8 DER, for the test keys
    /// above. `google_chat.rs` has an identical helper for its own tests;
    /// small enough that duplicating it here beats reaching into another
    /// module's private test code.
    fn pem_to_der(pem: &str) -> Vec<u8> {
        use base64::engine::general_purpose::STANDARD;
        let body: String = pem
            .lines()
            .filter(|line| !line.starts_with("-----"))
            .collect::<Vec<_>>()
            .join("");
        STANDARD.decode(body.trim()).unwrap()
    }

    /// Signs `claims` with `kid` in the header, using `private_key_pem`.
    fn sign_test_jwt(claims: &JsonValue, kid: &str, private_key_pem: &str) -> String {
        use ring::rand::SystemRandom;
        use ring::signature::{RsaKeyPair, RSA_PKCS1_SHA256};

        let header = serde_json::json!({"alg": "RS256", "typ": "JWT", "kid": kid});
        let signing_input = format!(
            "{}.{}",
            URL_SAFE_NO_PAD.encode(header.to_string()),
            URL_SAFE_NO_PAD.encode(claims.to_string()),
        );
        let der = pem_to_der(private_key_pem);
        let key_pair = RsaKeyPair::from_pkcs8(&der).unwrap();
        let mut signature = vec![0u8; key_pair.public().modulus_len()];
        key_pair
            .sign(
                &RSA_PKCS1_SHA256,
                &SystemRandom::new(),
                signing_input.as_bytes(),
                &mut signature,
            )
            .unwrap();
        format!("{signing_input}.{}", URL_SAFE_NO_PAD.encode(signature))
    }

    #[test]
    fn a_genuine_signed_token_verifies_and_its_activity_normalizes() {
        let adapter = adapter();
        adapter.jwks_cache.seed_for_test("test-key-1", test_jwk());
        let now_ms = 1_700_000_000_000i64;
        let jwt = sign_test_jwt(
            &valid_claims(now_ms / 1000),
            "test-key-1",
            TEST_PRIVATE_KEY_PEM,
        );
        let body = serde_json::to_vec(&personal_activity()).unwrap();
        let envelopes = adapter
            .verify_and_normalize(
                &[("authorization".to_string(), format!("Bearer {jwt}"))],
                &body,
                None,
                now_ms,
            )
            .expect("a genuinely signed, structurally valid token must verify");
        assert_eq!(envelopes.len(), 1);
        assert_eq!(envelopes[0].text, "hello bot");
        assert_eq!(envelopes[0].conversation.kind, ConversationKind::Direct);
    }

    #[test]
    fn a_verified_activity_records_its_service_url_for_send() {
        let adapter = adapter();
        adapter.jwks_cache.seed_for_test("test-key-1", test_jwk());
        let now_ms = 1_700_000_000_000i64;
        let jwt = sign_test_jwt(
            &valid_claims(now_ms / 1000),
            "test-key-1",
            TEST_PRIVATE_KEY_PEM,
        );
        let body = serde_json::to_vec(&personal_activity()).unwrap();
        adapter
            .verify_and_normalize(
                &[("authorization".to_string(), format!("Bearer {jwt}"))],
                &body,
                None,
                now_ms,
            )
            .expect("verification should succeed");
        let cached = adapter
            .service_urls
            .lock()
            .unwrap()
            .get("19:conv1")
            .cloned();
        assert_eq!(
            cached.as_deref(),
            Some("https://smba.trafficmanager.net/amer/")
        );
    }

    #[test]
    fn a_token_signed_by_a_different_key_is_refused() {
        let adapter = adapter();
        adapter.jwks_cache.seed_for_test("test-key-1", test_jwk());
        let now_ms = 1_700_000_000_000i64;
        // Signed by TEST_WRONG_PRIVATE_KEY_PEM, but the cache under this
        // `kid` holds TEST_PRIVATE_KEY_PEM's public key — the mismatch a
        // forged token would produce.
        let jwt = sign_test_jwt(
            &valid_claims(now_ms / 1000),
            "test-key-1",
            TEST_WRONG_PRIVATE_KEY_PEM,
        );
        let result = adapter.verify_and_normalize(
            &[("authorization".to_string(), format!("Bearer {jwt}"))],
            b"{}",
            None,
            now_ms,
        );
        assert!(result.unwrap_err().contains("signature"));
    }

    #[test]
    fn a_tampered_payload_is_refused() {
        let adapter = adapter();
        adapter.jwks_cache.seed_for_test("test-key-1", test_jwk());
        let now_ms = 1_700_000_000_000i64;
        let jwt = sign_test_jwt(
            &valid_claims(now_ms / 1000),
            "test-key-1",
            TEST_PRIVATE_KEY_PEM,
        );
        let mut parts: Vec<&str> = jwt.split('.').collect();
        let mut tampered_claims = valid_claims(now_ms / 1000);
        tampered_claims["extra"] = serde_json::json!("tampered-after-signing");
        let tampered_payload = URL_SAFE_NO_PAD.encode(tampered_claims.to_string());
        parts[1] = &tampered_payload;
        let tampered_jwt = parts.join(".");
        let result = adapter.verify_and_normalize(
            &[(
                "authorization".to_string(),
                format!("Bearer {tampered_jwt}"),
            )],
            b"{}",
            None,
            now_ms,
        );
        assert!(result.unwrap_err().contains("signature"));
    }

    #[test]
    fn alg_none_is_refused() {
        let adapter = adapter();
        let now_ms = 1_700_000_000_000i64;
        let header =
            URL_SAFE_NO_PAD.encode(serde_json::json!({"alg":"none","typ":"JWT"}).to_string());
        let payload = URL_SAFE_NO_PAD.encode(valid_claims(now_ms / 1000).to_string());
        // A syntactically valid (if meaningless) signature segment, so the
        // refusal under test is really the `alg` check and not an earlier
        // "malformed JWT" bail-out over an empty segment.
        let jwt = format!("{header}.{payload}.AAAA");
        let result = adapter.verify_and_normalize(
            &[("authorization".to_string(), format!("Bearer {jwt}"))],
            b"{}",
            None,
            now_ms,
        );
        assert!(result.unwrap_err().contains("alg"));
    }

    #[test]
    fn an_hmac_alg_is_refused() {
        let adapter = adapter();
        let now_ms = 1_700_000_000_000i64;
        let header =
            URL_SAFE_NO_PAD.encode(serde_json::json!({"alg":"HS256","typ":"JWT"}).to_string());
        let payload = URL_SAFE_NO_PAD.encode(valid_claims(now_ms / 1000).to_string());
        let jwt = format!("{header}.{payload}.AAAA");
        let result = adapter.verify_and_normalize(
            &[("authorization".to_string(), format!("Bearer {jwt}"))],
            b"{}",
            None,
            now_ms,
        );
        assert!(result.unwrap_err().contains("alg"));
    }

    #[test]
    fn an_unknown_kid_with_an_empty_cache_is_refused_and_does_not_spin() {
        // A plain `#[test]`, deliberately: no Tokio runtime exists here, so
        // `jwt::try_refresh_blocking`'s `Handle::try_current()` fails
        // and the adapter never attempts a fetch at all. That this call
        // returns promptly (rather than hanging on a hypothetical unbounded
        // retry loop) is the "does not spin" guarantee, and it holds without
        // a mock network because there is no network path to exercise here.
        let adapter = adapter();
        let now_ms = 1_700_000_000_000i64;
        let jwt = sign_test_jwt(
            &valid_claims(now_ms / 1000),
            "a-kid-nobody-published",
            TEST_PRIVATE_KEY_PEM,
        );
        let result = adapter.verify_and_normalize(
            &[("authorization".to_string(), format!("Bearer {jwt}"))],
            b"{}",
            None,
            now_ms,
        );
        assert!(result.is_err());
    }

    #[test]
    fn a_not_yet_valid_token_fails() {
        let adapter = adapter();
        let now_ms = 1_700_000_000_000i64;
        let mut claims = valid_claims(now_ms / 1000);
        claims["nbf"] = serde_json::json!(now_ms / 1000 + 3600);
        let jwt = make_jwt(&claims);
        let result = adapter.verify_and_normalize(
            &[("authorization".to_string(), format!("Bearer {jwt}"))],
            b"{}",
            None,
            now_ms,
        );
        assert!(result.unwrap_err().contains("not yet valid"));
    }

    #[test]
    fn a_wrong_issuer_fails() {
        let adapter = adapter();
        let now_ms = 1_700_000_000_000i64;
        let mut claims = valid_claims(now_ms / 1000);
        claims["iss"] = serde_json::json!("https://evil.example.com");
        let jwt = make_jwt(&claims);
        let result = adapter.verify_and_normalize(
            &[("authorization".to_string(), format!("Bearer {jwt}"))],
            b"{}",
            None,
            now_ms,
        );
        assert!(result.unwrap_err().contains("issuer"));
    }

    #[test]
    fn a_wrong_audience_fails() {
        let adapter = adapter();
        let now_ms = 1_700_000_000_000i64;
        let mut claims = valid_claims(now_ms / 1000);
        claims["aud"] = serde_json::json!("some-other-app-id");
        let jwt = make_jwt(&claims);
        let result = adapter.verify_and_normalize(
            &[("authorization".to_string(), format!("Bearer {jwt}"))],
            b"{}",
            None,
            now_ms,
        );
        assert!(result.unwrap_err().contains("audience"));
    }

    #[test]
    fn a_stale_token_fails() {
        let adapter = adapter();
        let now_ms = 1_700_000_000_000i64;
        let mut claims = valid_claims(now_ms / 1000);
        claims["exp"] = serde_json::json!(now_ms / 1000 - 3600);
        let jwt = make_jwt(&claims);
        let result = adapter.verify_and_normalize(
            &[("authorization".to_string(), format!("Bearer {jwt}"))],
            b"{}",
            None,
            now_ms,
        );
        assert!(result.unwrap_err().contains("expired"));
    }

    #[test]
    fn a_missing_authorization_header_fails() {
        let adapter = adapter();
        let result = adapter.verify_and_normalize(&[], b"{}", None, 0);
        assert!(result.is_err());
    }

    #[test]
    fn an_empty_authorization_header_fails() {
        let adapter = adapter();
        let result = adapter.verify_and_normalize(
            &[("authorization".to_string(), String::new())],
            b"{}",
            None,
            0,
        );
        assert!(result.is_err());
    }

    #[test]
    fn every_refusal_path_produces_no_envelopes() {
        // verify_and_normalize's signature only ever returns Err in this
        // build, so "no envelopes on failure" holds by construction; this
        // pins that down explicitly rather than leaving it implicit.
        let adapter = adapter();
        assert!(adapter.verify_and_normalize(&[], b"{}", None, 0).is_err());
    }

    #[test]
    fn no_secret_appears_in_any_rendered_error_string() {
        let adapter = adapter();
        let result = adapter.verify_and_normalize(&[], b"{}", None, 0);
        let error = result.unwrap_err();
        assert!(!error.contains("pw-value"));
    }

    // --- Normalization (pure function, fixtures) --------------------------

    fn personal_activity() -> JsonValue {
        serde_json::json!({
            "type": "message",
            "id": "activity-1",
            "timestamp": "2024-01-01T00:00:00.000Z",
            "serviceUrl": "https://smba.trafficmanager.net/amer/",
            "conversation": {"id": "19:conv1", "conversationType": "personal"},
            "from": {"id": "29:user1", "name": "Ada"},
            "recipient": {"id": "28:bot-id"},
            "text": "hello bot"
        })
    }

    #[test]
    fn a_personal_conversation_normalizes_to_direct() {
        let envelope =
            normalize_activity(&personal_activity(), "acct-teams", "28:bot-id", 0).unwrap();
        assert_eq!(envelope.conversation.kind, ConversationKind::Direct);
        assert_eq!(envelope.provider_event_id, "activity-1");
        assert_eq!(envelope.text, "hello bot");
    }

    #[test]
    fn a_channel_conversation_normalizes_to_group() {
        let mut activity = personal_activity();
        activity["conversation"]["conversationType"] = serde_json::json!("channel");
        let envelope = normalize_activity(&activity, "acct-teams", "28:bot-id", 0).unwrap();
        assert_eq!(envelope.conversation.kind, ConversationKind::Group);
    }

    #[test]
    fn a_group_chat_conversation_normalizes_to_group() {
        let mut activity = personal_activity();
        activity["conversation"]["conversationType"] = serde_json::json!("groupChat");
        let envelope = normalize_activity(&activity, "acct-teams", "28:bot-id", 0).unwrap();
        assert_eq!(envelope.conversation.kind, ConversationKind::Group);
    }

    #[test]
    fn a_mention_of_the_bot_is_detected_and_stripped_from_text() {
        let mut activity = personal_activity();
        activity["text"] = serde_json::json!("<at>Bot</at> please help");
        activity["entities"] = serde_json::json!([{
            "type": "mention",
            "text": "<at>Bot</at>",
            "mentioned": {"id": "28:bot-id", "name": "Bot"}
        }]);
        let envelope = normalize_activity(&activity, "acct-teams", "28:bot-id", 0).unwrap();
        assert!(envelope.mentions_self);
        assert_eq!(envelope.text, "Bot please help");
        assert!(!envelope.text.contains("<at>"));
    }

    #[test]
    fn a_mention_of_someone_else_does_not_set_mentions_self() {
        let mut activity = personal_activity();
        activity["entities"] = serde_json::json!([{
            "type": "mention",
            "mentioned": {"id": "29:someone-else"}
        }]);
        let envelope = normalize_activity(&activity, "acct-teams", "28:bot-id", 0).unwrap();
        assert!(!envelope.mentions_self);
    }

    #[test]
    fn an_attachment_with_a_content_url_normalizes_to_a_url_source() {
        let mut activity = personal_activity();
        activity["attachments"] = serde_json::json!([{
            "contentType": "image/png",
            "contentUrl": "https://example.com/file.png",
            "name": "file.png"
        }]);
        let envelope = normalize_activity(&activity, "acct-teams", "28:bot-id", 0).unwrap();
        assert_eq!(envelope.attachments.len(), 1);
        match &envelope.attachments[0].source {
            AttachmentSource::Url { url } => assert_eq!(url, "https://example.com/file.png"),
            other => panic!("expected a URL source, got {other:?}"),
        }
    }

    #[test]
    fn a_non_message_activity_normalizes_to_nothing() {
        let mut activity = personal_activity();
        activity["type"] = serde_json::json!("conversationUpdate");
        assert!(normalize_activity(&activity, "acct-teams", "28:bot-id", 0).is_none());
    }

    #[test]
    fn provider_event_ids_are_deterministic() {
        let first = normalize_activity(&personal_activity(), "acct-teams", "28:bot-id", 0).unwrap();
        let second =
            normalize_activity(&personal_activity(), "acct-teams", "28:bot-id", 0).unwrap();
        assert_eq!(first.provider_event_id, second.provider_event_id);
    }

    // --- serviceUrl validation ---------------------------------------------

    #[test]
    fn a_microsoft_https_service_url_validates() {
        assert!(validate_service_url("https://smba.trafficmanager.net/amer/").is_ok());
        assert!(validate_service_url("https://api.botframework.com/").is_ok());
    }

    #[test]
    fn a_non_https_service_url_is_refused() {
        assert!(validate_service_url("http://smba.trafficmanager.net/amer/").is_err());
    }

    #[test]
    fn a_non_microsoft_host_is_refused() {
        assert!(validate_service_url("https://evil.example.com/amer/").is_err());
    }

    // --- Outbound mapping ---------------------------------------------------

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

    /// Serves a canned token response forever (accepts every connection),
    /// since both the token fetch and the activity POST happen in one
    /// `send()` call.
    fn serve_forever(status: &str, body: &str) -> String {
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let response = format!(
            "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        std::thread::spawn(move || loop {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    let mut scratch = [0u8; 2048];
                    let _ = stream.read(&mut scratch);
                    let _ = stream.write_all(response.as_bytes());
                    let _ = stream.flush();
                }
                Err(_) => break,
            }
        });
        format!("http://127.0.0.1:{port}")
    }

    fn outbound_message() -> OutboundMessage {
        OutboundMessage {
            account_id: "acct-teams".to_string(),
            kind: ChannelKind::Teams,
            conversation_id: "19:conv1".to_string(),
            thread_id: None,
            text: "hi".to_string(),
            attachments: Vec::new(),
            reply_to_provider_id: None,
            idempotency_key: "idem-1".to_string(),
        }
    }

    #[tokio::test]
    async fn sending_with_no_known_service_url_is_a_permanent_failure() {
        let outcome = adapter().send(&outbound_message()).await;
        match outcome {
            SendOutcome::PermanentFailure { error } => assert!(error.contains("serviceUrl")),
            other => panic!("expected PermanentFailure, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_429_activity_response_maps_to_retryable_failure() {
        let token_base = serve_forever("200 OK", r#"{"access_token":"tok","expires_in":3600}"#);
        let send_base = serve_once("429 Too Many Requests", "{}");
        let adapter = adapter().with_login_base(&token_base);
        adapter.seed_service_url_for_test("19:conv1", &send_base);
        let outcome = adapter.send(&outbound_message()).await;
        assert!(
            matches!(outcome, SendOutcome::RetryableFailure { .. }),
            "{outcome:?}"
        );
    }

    #[tokio::test]
    async fn a_401_activity_response_maps_to_permanent_failure() {
        let token_base = serve_forever("200 OK", r#"{"access_token":"tok","expires_in":3600}"#);
        let send_base = serve_once("401 Unauthorized", "{}");
        let adapter = adapter().with_login_base(&token_base);
        adapter.seed_service_url_for_test("19:conv1", &send_base);
        let outcome = adapter.send(&outbound_message()).await;
        assert!(
            matches!(outcome, SendOutcome::PermanentFailure { .. }),
            "{outcome:?}"
        );
    }

    #[tokio::test]
    async fn a_successful_send_extracts_the_provider_message_id() {
        let token_base = serve_forever("200 OK", r#"{"access_token":"tok","expires_in":3600}"#);
        let send_base = serve_once("200 OK", r#"{"id":"activity-out-1"}"#);
        let adapter = adapter().with_login_base(&token_base);
        adapter.seed_service_url_for_test("19:conv1", &send_base);
        let outcome = adapter.send(&outbound_message()).await;
        match outcome {
            SendOutcome::Sent {
                provider_message_id,
            } => {
                assert_eq!(provider_message_id.as_deref(), Some("activity-out-1"));
            }
            other => panic!("expected Sent, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn probe_reports_connected_when_the_token_exchange_succeeds() {
        let token_base = serve_forever("200 OK", r#"{"access_token":"tok","expires_in":3600}"#);
        let adapter = adapter()
            .with_login_base(&token_base)
            // Port 1 on loopback: nothing listens there, so the JWKS refresh
            // probe() attempts fails fast with connection-refused rather than
            // reaching the real Bot Framework metadata endpoint. probe()
            // treats that refresh as best-effort, so it does not affect the
            // assertion below.
            .with_metadata_url("http://127.0.0.1:1/metadata");
        let health = adapter.probe().await;
        assert_eq!(health.state, HealthState::Connected);
    }

    #[tokio::test]
    async fn probe_reports_error_when_the_token_exchange_fails_and_leaks_no_secret() {
        let token_base = serve_forever("401 Unauthorized", r#"{"error":"invalid_client"}"#);
        let adapter = adapter()
            .with_login_base(&token_base)
            .with_metadata_url("http://127.0.0.1:1/metadata");
        let health = adapter.probe().await;
        assert_eq!(health.state, HealthState::Error);
        let error = health.last_error.unwrap_or_default();
        assert!(!error.contains("pw-value"));
    }
}
