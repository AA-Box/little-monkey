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
//! This adapter implements the structural checks and deliberately does not
//! implement the signature check — `TODO(teams-jwks)`. Fetching, caching and
//! rotating a remote JWKS correctly (freshness, key rollover, the fetch
//! itself being an authenticated-looking but attacker-reachable network call
//! this adapter would make on every delivery) is a meaningfully sized,
//! security-critical feature in its own right, not a detail to bolt on at the
//! end of four adapters. Rather than ship a half-verified signature check —
//! which is worse than none, because it *looks* like verification —
//! [`TeamsAdapter::verify_and_normalize`] refuses every inbound delivery
//! outright once the structural checks pass, with an error that says exactly
//! why. [`normalize_activity`] — the pure mapping this adapter would use once
//! JWKS verification lands — is implemented and unit-tested against fixtures
//! so wiring it in later is a small, reviewable change rather than a new
//! feature.
//!
//! Outbound uses the client-credentials flow against the configured tenant,
//! with the resulting token cached and refreshed on expiry.

use async_trait::async_trait;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use little_monkey_lib::channels::types::{
    AttachmentKind, AttachmentSource, BoundedMetadata, ChannelAttachment, ChannelConversation,
    ChannelEnvelope, ChannelHealth, ChannelKind, ChannelSender, InboundTransport, OutboundMessage,
    ProviderCapabilities, SendOutcome,
};
use serde::Deserialize;
use serde_json::Value as JsonValue;
use std::collections::BTreeMap;
use std::sync::Mutex;

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

#[derive(Debug, Deserialize)]
struct TeamsNonSecretConfig {
    app_id: String,
    tenant_id: String,
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
    app_id: String,
    tenant_id: String,
    app_password: String,
    token_cache: Mutex<Option<CachedToken>>,
    /// Validated `serviceUrl` per conversation. Only ever written by
    /// [`TeamsAdapter::record_conversation_service_url`], which validates
    /// before inserting — `send` refuses to guess or derive one. Currently
    /// never populated in production because `verify_and_normalize` refuses
    /// every delivery (see the module doc); the mechanism exists so wiring in
    /// JWKS verification later only has to call it.
    service_urls: Mutex<BTreeMap<String, String>>,
    /// Identity provider origin. Always [`LOGIN_BASE`] in production;
    /// swappable in tests.
    login_base: String,
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
        Ok(Self {
            app_id: non_secret.app_id,
            tenant_id: non_secret.tenant_id,
            app_password: secrets.app_password,
            token_cache: Mutex::new(None),
            service_urls: Mutex::new(BTreeMap::new()),
            login_base: LOGIN_BASE.to_string(),
        })
    }

    #[cfg(test)]
    fn with_login_base(mut self, base: &str) -> Self {
        self.login_base = base.to_string();
        self
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
        _body: &[u8],
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
        let claims = decode_jwt_claims(token)?;
        validate_claims_structurally(&claims, &self.app_id, now_ms)?;

        // Structural checks passed, but that is not verification — see the
        // module doc. The body is deliberately never parsed past this point:
        // an unverified delivery earns no normalization, exactly as a
        // structurally-invalid one would.
        Err(
            "Teams inbound signature verification is not implemented (TODO(teams-jwks)); \
             refusing unverified delivery"
                .to_string(),
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
            supports_attachments: true,
            supports_mention_metadata: true,
            supports_idempotency_key: false,
            supports_delivery_receipts: false,
            ..ProviderCapabilities::minimal(ChannelKind::Teams, InboundTransport::Webhook)
        }
    }

    async fn probe(&self) -> ChannelHealth {
        let now = now_ms();
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

/// Splits, base64url-decodes and JSON-parses a JWT's claims (the middle
/// segment). Does not touch the signature — that is the part this adapter
/// does not verify.
fn decode_jwt_claims(token: &str) -> Result<JsonValue, String> {
    let mut parts = token.split('.');
    let (Some(_header), Some(payload), Some(_signature)) =
        (parts.next(), parts.next(), parts.next())
    else {
        return Err("Malformed JWT".to_string());
    };
    if parts.next().is_some() {
        return Err("Malformed JWT".to_string());
    }
    let decoded = URL_SAFE_NO_PAD
        .decode(payload)
        .map_err(|_| "JWT payload is not valid base64url".to_string())?;
    serde_json::from_slice(&decoded).map_err(|_| "JWT payload is not valid JSON".to_string())
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
    use super::*;
    use crate::daemon::channel_store::ChannelAccountRecord;
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

    fn make_jwt(claims: &JsonValue) -> String {
        let header =
            URL_SAFE_NO_PAD.encode(serde_json::json!({"alg":"RS256","typ":"JWT"}).to_string());
        let payload = URL_SAFE_NO_PAD.encode(claims.to_string());
        format!("{header}.{payload}.unsigned-placeholder-signature")
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

    #[test]
    fn a_structurally_valid_token_is_still_refused_because_the_signature_is_unverified() {
        let adapter = adapter();
        let now_ms = 1_700_000_000_000i64;
        let jwt = make_jwt(&valid_claims(now_ms / 1000));
        let result = adapter.verify_and_normalize(
            &[("authorization".to_string(), format!("Bearer {jwt}"))],
            b"{}",
            None,
            now_ms,
        );
        let error = result.unwrap_err();
        assert!(error.contains("teams-jwks"));
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
        let adapter = adapter().with_login_base(&token_base);
        let health = adapter.probe().await;
        assert_eq!(health.state, HealthState::Connected);
    }

    #[tokio::test]
    async fn probe_reports_error_when_the_token_exchange_fails_and_leaks_no_secret() {
        let token_base = serve_forever("401 Unauthorized", r#"{"error":"invalid_client"}"#);
        let adapter = adapter().with_login_base(&token_base);
        let health = adapter.probe().await;
        assert_eq!(health.state, HealthState::Error);
        let error = health.last_error.unwrap_or_default();
        assert!(!error.contains("pw-value"));
    }
}
