//! Google Chat app-interaction adapter (official HTTP endpoint URL flow).
//!
//! # Inbound trust — read this before touching `verify_and_normalize`
//!
//! Google Chat authenticates a delivery with a JWT in `Authorization: Bearer`,
//! issued by `chat@system.gserviceaccount.com`. What it puts in `aud` is the
//! app's own **Authentication Audience** setting, and only the *Project Number*
//! value of that setting is supported here: it is the one whose token is this
//! self-signed Chat service-account JWT, which is what the verifier below
//! actually checks. Google's other value, *App URL*, is a different token
//! issued by a different Google identity, and verifying it means a second,
//! genuinely separate OIDC path — swapping the expected `aud` on this verifier
//! would accept the wrong signer, so the option is absent rather than faked.
//! The setup panel names the same setting and the same single value.
//!
//! Full validation is: verify issuer, audience and expiry (structural — cheap,
//! no network), *and* verify the RS256 signature against
//! one of Google's rotating public keys, published as a JWKS document at
//! `https://www.googleapis.com/service_accounts/v1/jwk/chat@system.gserviceaccount.com`
//! and cached.
//!
//! This adapter implements both halves: the structural checks, and the
//! signature check against a JWKS key set cached in [`JwksCache`]. The cache
//! is refreshed from the async side — opportunistically in [`probe`](
//! GoogleChatAdapter::probe), and, when a delivery names a `kid` the cache
//! does not have, by exactly one bounded synchronous fetch bridged out of the
//! trait's required-synchronous `verify_and_normalize` (see
//! [`try_refresh_blocking`] for why that bridge is safe). A delivery whose
//! `kid` is still unknown after that one attempt is refused, not retried —
//! see [`GoogleChatAdapter::ensure_key_for_kid`].
//!
//! The JWKS verification core — JWT header/claims decoding without
//! re-serialization, `alg` pinned to `RS256` by name (never trusted from the
//! token), the JWKS fetch and cache, and the bounded sync-refresh bridge — is
//! written once here and reused by the Teams adapter, since both providers
//! publish RSA keys the same way (a JWKS document with `n`/`e` members). Only
//! the two things that differ per provider — where the JWKS document lives,
//! and the account-specific claims — stay adapter-local.
//!
//! [`normalize_event`] is the pure mapping from a verified event to an
//! envelope, unit-tested against fixtures independently of verification.
//!
//! Outbound authenticates as the configured service account: a JWT assertion
//! signed with its RSA private key is exchanged for an OAuth access token
//! (the standard Google server-to-server flow), and that token is cached and
//! refreshed on expiry.

use async_trait::async_trait;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use base64::Engine as _;
use little_monkey_lib::channels::types::{
    AttachmentKind, AttachmentSource, ChannelAttachment, ChannelConversation, ChannelEnvelope,
    ChannelHealth, ChannelKind, ChannelSender, InboundTransport, OutboundMessage,
    ProviderCapabilities, SendOutcome,
};
use ring::rand::SystemRandom;
use ring::signature::{RsaKeyPair, RSA_PKCS1_SHA256};
use serde::Deserialize;
use serde_json::Value as JsonValue;
use std::sync::Mutex;

use super::jwt::{
    decode_jwt, fetch_jwks_via_egress, try_refresh_blocking, validate_alg_is_rs256,
    verify_rs256_signature, JwkRsaKey, JwksCache,
};
use crate::daemon::channel_adapter::{
    AdapterConfig, ChannelAdapter, InboundBatch, WebhookChannelAdapter,
};

const CHAT_API_BASE: &str = "https://chat.googleapis.com";
const OAUTH_TOKEN_BASE: &str = "https://oauth2.googleapis.com";
/// Google Chat's fixed JWKS endpoint for app-interaction deliveries. No
/// account config overrides this one — unlike Teams, Google Chat does not
/// vary it per tenant.
const CHAT_JWKS_URL: &str =
    "https://www.googleapis.com/service_accounts/v1/jwk/chat@system.gserviceaccount.com";
/// Google Chat's own fixed JWT issuer for app-interaction deliveries.
const EXPECTED_ISSUER: &str = "chat@system.gserviceaccount.com";
const SKEW_SECS: i64 = 300;
const TOKEN_REFRESH_SKEW_SECS: i64 = 60;
const CHAT_BOT_SCOPE: &str = "https://www.googleapis.com/auth/chat.bot";

#[derive(Debug, Deserialize)]
struct GoogleChatNonSecretConfig {
    /// The Cloud project number this app's deliveries are minted for, which is
    /// the `aud` every delivery must carry. Required — there is nothing to
    /// verify a token against without it.
    project_number: String,
    /// This app's own Chat resource name (`users/<id>`), used only to detect
    /// whether an inbound message `@mentions` this app. Optional and
    /// non-secret: absent, `mentions_self` is conservatively always `false`
    /// rather than guessed.
    #[serde(default)]
    bot_user_name: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GoogleChatSecrets {
    client_email: String,
    private_key: String,
}

struct CachedToken {
    access_token: String,
    expires_at_ms: i64,
}

pub struct GoogleChatAdapter {
    account_id: String,
    /// What an inbound token's `aud` must be: the operator's Cloud project
    /// number, exactly as their Authentication Audience setting mints it.
    project_number: String,
    client_email: String,
    /// PKCS8 DER, parsed once at construction so a malformed key is rejected
    /// at setup rather than on the first send.
    private_key_der: Vec<u8>,
    /// This app's own Chat resource name, for `mentions_self` detection.
    /// Empty when not configured, which makes `mentions_self` always `false`
    /// rather than a guess.
    bot_user_name: String,
    token_cache: Mutex<Option<CachedToken>>,
    /// Cached RS256 JWKS keys for verifying inbound deliveries. See the
    /// module doc for how this is kept warm.
    jwks_cache: JwksCache,
    chat_api_base: String,
    oauth_token_base: String,
    /// Where the JWKS document is fetched from. Always [`CHAT_JWKS_URL`] in
    /// production; swappable in tests.
    jwks_url: String,
}

impl GoogleChatAdapter {
    pub fn new(config: &AdapterConfig<'_>) -> Result<Self, String> {
        let non_secret: GoogleChatNonSecretConfig =
            serde_json::from_value(config.account.non_secret_config.clone())
                .map_err(|error| format!("Invalid Google Chat account config: {error}"))?;
        if non_secret.project_number.trim().is_empty() {
            return Err(
                "Google Chat account is missing project_number, which is what its \
                 Authentication Audience setting of Project Number verifies against"
                    .to_string(),
            );
        }
        let secrets: GoogleChatSecrets = serde_json::from_str(&config.secret)
            .map_err(|_| "Google Chat account credential is missing or malformed".to_string())?;
        if secrets.client_email.trim().is_empty() || secrets.private_key.trim().is_empty() {
            return Err(
                "Google Chat account credential is missing client_email or private_key".to_string(),
            );
        }
        let private_key_der = pkcs8_der_from_pem(&secrets.private_key)
            .map_err(|_| "Google Chat account private_key is not a valid PEM key".to_string())?;
        // Fails fast on a malformed key rather than on the first send.
        RsaKeyPair::from_pkcs8(&private_key_der).map_err(|_| {
            "Google Chat account private_key could not be parsed as RSA PKCS8".to_string()
        })?;
        Ok(Self {
            account_id: config.account.account_id.clone(),
            project_number: non_secret.project_number.trim().to_string(),
            client_email: secrets.client_email,
            private_key_der,
            bot_user_name: non_secret.bot_user_name.unwrap_or_default(),
            token_cache: Mutex::new(None),
            jwks_cache: JwksCache::new(),
            chat_api_base: CHAT_API_BASE.to_string(),
            oauth_token_base: OAUTH_TOKEN_BASE.to_string(),
            jwks_url: CHAT_JWKS_URL.to_string(),
        })
    }

    #[cfg(test)]
    pub(crate) fn with_bases(mut self, chat_api_base: &str, oauth_token_base: &str) -> Self {
        self.chat_api_base = chat_api_base.to_string();
        self.oauth_token_base = oauth_token_base.to_string();
        self
    }

    /// Stand in for the JWKS fetch with this file's own test key, so a test can
    /// verify a genuinely signed token without reaching Google.
    #[cfg(test)]
    pub(crate) fn seed_jwks_for_test(&self) {
        self.jwks_cache
            .seed_for_test("test-key-1", tests::test_jwk());
    }

    #[cfg(test)]
    pub(crate) fn with_jwks_url(mut self, jwks_url: &str) -> Self {
        self.jwks_url = jwks_url.to_string();
        self
    }

    /// Fetches the JWKS document and replaces the cache on success. Used by
    /// [`probe`](ChannelAdapter::probe) to keep the cache warm, and by
    /// [`ensure_key_for_kid`](Self::ensure_key_for_kid) as the one bounded
    /// synchronous attempt on an unknown `kid`.
    async fn refresh_keys(&self, now_ms: i64) -> bool {
        match fetch_jwks_via_egress(&self.jwks_url).await {
            Ok(keys) => {
                self.jwks_cache.replace(keys, now_ms);
                true
            }
            Err(_) => false,
        }
    }

    /// The key for `kid`, fetching once more if the cache does not have it.
    ///
    /// A delivery whose `kid` is still unknown after that one attempt is
    /// refused by the caller — this never loops or retries beyond the single
    /// bridge, which is what keeps an attacker-supplied `kid` from driving
    /// unbounded fetches.
    fn ensure_key_for_kid(&self, kid: &str, now_ms: i64) -> Option<JwkRsaKey> {
        if let Some(key) = self.jwks_cache.find(kid) {
            return Some(key);
        }
        if try_refresh_blocking(|| self.refresh_keys(now_ms)) {
            return self.jwks_cache.find(kid);
        }
        None
    }

    /// A cached OAuth access token, minting and exchanging a fresh
    /// service-account JWT assertion when absent or near expiry.
    async fn access_token(&self) -> Result<String, String> {
        let now = now_ms();
        if let Ok(cache) = self.token_cache.lock() {
            if let Some(token) = cache.as_ref() {
                if now < token.expires_at_ms - TOKEN_REFRESH_SKEW_SECS * 1000 {
                    return Ok(token.access_token.clone());
                }
            }
        }
        let assertion =
            mint_service_account_jwt(&self.client_email, &self.private_key_der, now / 1000)?;
        let client = little_monkey_lib::egress::hardened()
            .build()
            .map_err(|error| format!("Failed to build client: {error}"))?;
        let url = format!("{}/token", self.oauth_token_base);
        let params = [
            ("grant_type", "urn:ietf:params:oauth:grant-type:jwt-bearer"),
            ("assertion", assertion.as_str()),
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
            // Never includes the response body: a failed exchange can echo
            // parts of the request back.
            return Err(format!(
                "Google Chat token request failed with status {status}"
            ));
        }
        let parsed: JsonValue = serde_json::from_slice(&bytes)
            .map_err(|_| "Google Chat token response was not valid JSON".to_string())?;
        let access_token = parsed
            .get("access_token")
            .and_then(JsonValue::as_str)
            .ok_or_else(|| "Google Chat token response is missing access_token".to_string())?
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

impl WebhookChannelAdapter for GoogleChatAdapter {
    fn kind(&self) -> ChannelKind {
        ChannelKind::GoogleChat
    }

    /// Google Chat reads a `200`'s body as an optional immediate reply and
    /// treats any other status as a failed delivery. An empty JSON object is
    /// how an app that will answer through the API later says there is nothing
    /// to post right now.
    fn ack(&self) -> crate::daemon::channel_adapter::WebhookAck {
        crate::daemon::channel_adapter::WebhookAck::json_ok()
    }

    fn verify_and_normalize(
        &self,
        headers: &[(String, String)],
        body: &[u8],
        _public_base_url: Option<&str>,
        now_ms: i64,
    ) -> Result<Vec<ChannelEnvelope>, String> {
        // `public_base_url` is unused: the only supported Authentication
        // Audience is the project number, so nothing here is ever compared
        // against a URL — least of all one a request could choose.
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
        validate_claims_structurally(&decoded.claims, &self.project_number, now_ms)?;
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
        // Google's own JWKS — the body is never parsed before that.
        let event: JsonValue = serde_json::from_slice(body)
            .map_err(|_| "Google Chat event body is not valid JSON".to_string())?;
        Ok(
            normalize_event(&event, &self.account_id, &self.bot_user_name, now_ms)
                .into_iter()
                .collect(),
        )
    }
}

#[async_trait]
impl ChannelAdapter for GoogleChatAdapter {
    fn kind(&self) -> ChannelKind {
        ChannelKind::GoogleChat
    }

    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            max_text_chars: 4096,
            supports_threads: true,
            supports_attachments: false,
            supports_mention_metadata: true,
            // `requestId` on messages.create, which the send always carries.
            supports_idempotency_key: true,
            supports_delivery_receipts: false,
            ..ProviderCapabilities::minimal(ChannelKind::GoogleChat, InboundTransport::Webhook)
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
            Ok(_) => {
                ChannelHealth::connected(now, Some("Service account credentials valid".to_string()))
            }
            Err(error) => ChannelHealth::error(now, error),
        }
    }

    async fn poll(&self, _cursor: Option<&str>) -> Result<InboundBatch, String> {
        // Google Chat is delivered to, not polled — see the module doc.
        Ok(InboundBatch::default())
    }

    async fn send(&self, message: &OutboundMessage) -> SendOutcome {
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
        // `requestId` is Chat's own idempotency: creating a message with a
        // request id it has already seen returns that message rather than
        // posting a second one. The outbox's key is stable across retries of
        // the same row, which is the only thing that makes it useful.
        let mut url = format!(
            "{}/v1/{}/messages?requestId={}",
            self.chat_api_base,
            message.conversation_id,
            request_id(&message.idempotency_key)
        );
        let mut body = serde_json::json!({ "text": message.text });
        if let Some(thread_id) = &message.thread_id {
            body["thread"] = serde_json::json!({ "name": thread_id });
            // Naming a thread is not by itself a reply: without this the API
            // starts a new thread and the answer is posted away from the
            // question. The fallback is deliberate — a thread that has since
            // been deleted should still get the answer somewhere visible
            // rather than nowhere.
            url.push_str("&messageReplyOption=REPLY_MESSAGE_FALLBACK_TO_NEW_THREAD");
        }
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
                .get("name")
                .and_then(JsonValue::as_str)
                .map(str::to_string);
            return SendOutcome::Sent {
                provider_message_id,
            };
        }
        let error_message = format!("Google Chat send failed with status {status}");
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

    /// Google Chat serves an uploaded file through its media endpoint, named by
    /// the `resourceName` the message carried, with the same service-account
    /// token the send path uses.
    async fn fetch_attachment(
        &self,
        attachment: &ChannelAttachment,
        limits: crate::daemon::channel_adapter::AttachmentLimits,
    ) -> Result<Vec<u8>, String> {
        let AttachmentSource::ProviderHandle { handle } = &attachment.source else {
            return Err("This Google Chat attachment has no resource name.".to_string());
        };
        // The resource name is path-concatenated and arrives inside a delivery,
        // so anything that could climb out of `/v1/media/` is refused.
        if handle.is_empty() || handle.contains("..") || handle.starts_with('/') {
            return Err("That Google Chat resource name is not usable".to_string());
        }
        let token = self.access_token().await?;
        crate::daemon::channel_adapter::fetch_url(
            &format!("{}/v1/media/{handle}?alt=media", self.chat_api_base),
            Some(&token),
            limits.max_bytes,
        )
        .await
    }
}

/// The outbox's idempotency key as a Chat `requestId`.
///
/// Hex of a digest rather than the key itself: the key is an internal id of no
/// fixed alphabet, and this ends up in a query string. Deterministic, because a
/// request id that changed per attempt would collapse nothing.
fn request_id(idempotency_key: &str) -> String {
    let digest = ring::digest::digest(&ring::digest::SHA256, idempotency_key.as_bytes());
    digest.as_ref()[..16]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// Strips PEM armor and base64-decodes to PKCS8 DER.
fn pkcs8_der_from_pem(pem: &str) -> Result<Vec<u8>, ()> {
    let body: String = pem
        .lines()
        .filter(|line| !line.starts_with("-----"))
        .collect::<Vec<_>>()
        .join("");
    if body.is_empty() {
        return Err(());
    }
    STANDARD.decode(body.trim()).map_err(|_| ())
}

/// Signs a short-lived JWT assertion as the service account, for exchange at
/// Google's token endpoint. `now_secs` is injected rather than read directly
/// so a test can pin a deterministic `iat`/`exp`.
fn mint_service_account_jwt(
    client_email: &str,
    private_key_der: &[u8],
    now_secs: i64,
) -> Result<String, String> {
    let header = serde_json::json!({ "alg": "RS256", "typ": "JWT" });
    let claims = serde_json::json!({
        "iss": client_email,
        "scope": CHAT_BOT_SCOPE,
        "aud": format!("{OAUTH_TOKEN_BASE}/token"),
        "iat": now_secs,
        "exp": now_secs + 3600,
    });
    let signing_input = format!(
        "{}.{}",
        URL_SAFE_NO_PAD.encode(header.to_string()),
        URL_SAFE_NO_PAD.encode(claims.to_string()),
    );
    let key_pair = RsaKeyPair::from_pkcs8(private_key_der)
        .map_err(|_| "Google Chat private key could not be parsed".to_string())?;
    let mut signature = vec![0u8; key_pair.public().modulus_len()];
    key_pair
        .sign(
            &RSA_PKCS1_SHA256,
            &SystemRandom::new(),
            signing_input.as_bytes(),
            &mut signature,
        )
        .map_err(|_| "Failed to sign the Google Chat service-account assertion".to_string())?;
    Ok(format!(
        "{signing_input}.{}",
        URL_SAFE_NO_PAD.encode(signature)
    ))
}

fn validate_claims_structurally(
    claims: &JsonValue,
    expected_audience: &str,
    now_ms: i64,
) -> Result<(), String> {
    let issuer = claims
        .get("iss")
        .and_then(JsonValue::as_str)
        .ok_or_else(|| "JWT is missing iss".to_string())?;
    if issuer != EXPECTED_ISSUER {
        return Err("JWT issuer is not Google Chat's system account".to_string());
    }
    let audience = claims
        .get("aud")
        .and_then(JsonValue::as_str)
        .ok_or_else(|| "JWT is missing aud".to_string())?;
    if audience != expected_audience {
        return Err(
            "JWT audience does not match this account's configured authentication audience"
                .to_string(),
        );
    }
    let now_secs = now_ms / 1000;
    let exp = claims
        .get("exp")
        .and_then(JsonValue::as_i64)
        .ok_or_else(|| "JWT is missing exp".to_string())?;
    if now_secs > exp + SKEW_SECS {
        return Err("JWT has expired".to_string());
    }
    if let Some(iat) = claims.get("iat").and_then(JsonValue::as_i64) {
        if now_secs + SKEW_SECS < iat {
            return Err("JWT is not yet valid".to_string());
        }
    }
    Ok(())
}

fn sanitize_transport_error(error: &reqwest::Error) -> String {
    if error.is_timeout() {
        "Google Chat request timed out".to_string()
    } else if error.is_connect() {
        "Could not connect to Google Chat".to_string()
    } else {
        "Google Chat request failed".to_string()
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

/// Normalizes a `MESSAGE` event into an envelope. Every other event type
/// (`ADDED_TO_SPACE`, `REMOVED_FROM_SPACE`, `CARD_CLICKED`, ...) normalizes to
/// nothing.
///
/// Not currently reachable from [`GoogleChatAdapter::verify_and_normalize`] —
/// see the module doc for why — but implemented and tested against real event
/// shapes so wiring it in behind a certificate check is a small change.
fn normalize_event(
    event: &JsonValue,
    account_id: &str,
    bot_user_name: &str,
    fallback_received_at_ms: i64,
) -> Option<ChannelEnvelope> {
    if event.get("type").and_then(JsonValue::as_str) != Some("MESSAGE") {
        return None;
    }
    let message = event.get("message")?;
    let provider_event_id = message.get("name").and_then(JsonValue::as_str)?.to_string();

    let space = event
        .get("space")
        .or_else(|| message.get("space"))
        .unwrap_or(&JsonValue::Null);
    let space_name = space.get("name").and_then(JsonValue::as_str)?;
    let space_type = space
        .get("type")
        .and_then(JsonValue::as_str)
        .unwrap_or("ROOM");
    let conversation = if space_type == "DM" {
        ChannelConversation::direct(space_name)
    } else {
        ChannelConversation::group(space_name)
    };
    let thread_id = message
        .get("thread")
        .and_then(|thread| thread.get("name"))
        .and_then(JsonValue::as_str)
        .map(str::to_string);
    let conversation = conversation.with_thread(thread_id);

    let sender_id = message
        .get("sender")
        .and_then(|sender| sender.get("name"))
        .and_then(JsonValue::as_str)?;
    let sender_label = message
        .get("sender")
        .and_then(|sender| sender.get("displayName"))
        .and_then(JsonValue::as_str);
    let sender = ChannelSender::new(sender_id).with_label(sender_label.map(str::to_string));

    let text = message
        .get("argumentText")
        .and_then(JsonValue::as_str)
        .or_else(|| message.get("text").and_then(JsonValue::as_str))
        .unwrap_or_default()
        .to_string();

    let mentions_self = message
        .get("annotations")
        .and_then(JsonValue::as_array)
        .map(|annotations| {
            annotations.iter().any(|annotation| {
                annotation.get("type").and_then(JsonValue::as_str) == Some("USER_MENTION")
                    && annotation
                        .get("userMention")
                        .and_then(|mention| mention.get("user"))
                        .and_then(|user| user.get("name"))
                        .and_then(JsonValue::as_str)
                        == Some(bot_user_name)
            })
        })
        .unwrap_or(false);

    let received_at_ms = event
        .get("eventTime")
        .and_then(JsonValue::as_str)
        .and_then(|value| chrono::DateTime::parse_from_rfc3339(value).ok())
        .map(|value| value.timestamp_millis())
        .unwrap_or(fallback_received_at_ms);

    // Google Chat puts uploaded files in `message.attachment[]`, each naming a
    // media resource the Chat API serves separately. Without this a message
    // that was only a file normalized to empty text and the gate dropped it.
    let attachments: Vec<ChannelAttachment> = message
        .get("attachment")
        .and_then(JsonValue::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| {
                    let resource = item
                        .get("attachmentDataRef")
                        .and_then(|reference| reference.get("resourceName"))
                        .and_then(JsonValue::as_str)?;
                    let mime_type = item.get("contentType").and_then(JsonValue::as_str);
                    Some(ChannelAttachment {
                        provider_id: Some(resource.to_string()),
                        kind: mime_type
                            .map(AttachmentKind::from_mime)
                            .unwrap_or(AttachmentKind::Other),
                        filename: item
                            .get("contentName")
                            .and_then(JsonValue::as_str)
                            .map(str::to_string),
                        mime_type: mime_type.map(str::to_string),
                        declared_size_bytes: None,
                        source: AttachmentSource::ProviderHandle {
                            handle: resource.to_string(),
                        },
                        stored_artifact_id: None,
                        fetch_error: None,
                        text_excerpt: None,
                    })
                })
                .collect()
        })
        .unwrap_or_default();

    Some(ChannelEnvelope {
        account_id: account_id.to_string(),
        kind: ChannelKind::GoogleChat,
        provider_event_id,
        conversation,
        sender,
        text,
        attachments,
        reply_to_provider_id: None,
        mentions_self,
        received_at_ms,
        metadata: little_monkey_lib::channels::types::BoundedMetadata::new(),
    })
}

#[cfg(test)]
pub(crate) mod tests {

    #[test]
    fn an_uploaded_file_becomes_an_attachment_the_adapter_can_fetch() {
        let event = serde_json::json!({
            "type": "MESSAGE",
            "space": {"name": "spaces/AAAA", "type": "ROOM"},
            "message": {
                "name": "spaces/AAAA/messages/BBBB",
                "sender": {"name": "users/111", "displayName": "Ada"},
                "text": "",
                "attachment": [{
                    "contentName": "report.pdf",
                    "contentType": "application/pdf",
                    "attachmentDataRef": {"resourceName": "uploads/xyz"}
                }]
            }
        });

        let envelope = normalize_event(&event, "acct-1", "users/bot", 0).expect("an envelope");

        assert_eq!(envelope.attachments.len(), 1);
        assert_eq!(
            envelope.attachments[0].filename.as_deref(),
            Some("report.pdf")
        );
        match &envelope.attachments[0].source {
            AttachmentSource::ProviderHandle { handle } => assert_eq!(handle, "uploads/xyz"),
            other => panic!("expected a provider handle, got {other:?}"),
        }
    }
    use super::*;
    use crate::daemon::channel_store::ChannelAccountRecord;
    use little_monkey_lib::channels::policy::ChannelAccessPolicy;
    use little_monkey_lib::channels::types::{ConversationKind, HealthState};
    use std::io::{Read, Write};

    /// A 2048-bit RSA test key generated locally for these tests only. Not
    /// used anywhere else and grants access to nothing.
    pub(crate) const TEST_PRIVATE_KEY_PEM: &str = "-----BEGIN PRIVATE KEY-----
MIIEvQIBADANBgkqhkiG9w0BAQEFAASCBKcwggSjAgEAAoIBAQDazRpB7LGR9vNE
et1N+4LHeMOtHD6zHXy7bx4t/YlI0EVVPbekoxkqG5TXLIzrOoKcDoEgOvs5ZpZV
zdaFW55J1YnUdS84Wiu3rfcBNCNOZXC/ovOpLvTOIU/gqjFM05QqsrXQVK6/iRHO
coOWLu86s7GrwkjJ3Q0rkajLKNlXjZ4zZk8cyo3FwTAom0Jz8Z1k/AHvCnuTW7om
cVRNnKn4vNKzdbnSW7wGjXNuuvcIe+EeEIfCpdd+u2+Nj4gt6wNxG0N4ylnwd9if
qZaXX6CdF9WoyikjV5aFB5Kvnw2AqV8mAqQCwmsXHmKCmAnCRAr0z9jdp6yUZW6a
WxnJvAkHAgMBAAECggEAYYpNylXaU1fj2wNq5PjatFIB6YpN6Uub73L54UbFjNBM
GFQLKjoLXdot4D7cE4Fh+G/+4H9qv4wcOOQqXgkZ55BqgWw8QMLx+lBzbPXxR2Gx
gb0DIsjsGXiAQ4ebssQfqWlB5D3cKUbRyVcDklAaFfFFo2xZRqRk2qd0uBzdx0ln
vHTo6OZ2479HJFFcDOTegb1rqss9M1QSQSH24er5f1Sz/lAEoSTrnuoovLaiKRfX
1w/y7yvBghz6Evmy6dYMS6C9/oDJdCJpubaA/qiQjEl4L7+vgJU5EfOBj70UHGZr
6beYIJvSkPs630sdvC9+67OUpwFNLZ6e1nnGL++0AQKBgQD0gkrICdG56aeEyuDx
L4/MYo0i+pTWj5jv3GarpP83B6mBmkrDI1GuMIoCIy6sXALGJO5bIcGUyGQnVzvk
Wzw52PTRcmFOT9BwZqDFXamW7sOvPmumFLa8Q4gLJr7vBFlvUViZaAyofCFYaY+d
8eKOWCRHuWkt5BjXX6cGcCwE9wKBgQDlFYSg47skhhr52FNo1eYQClHfcZhX6zca
22OdIONOrjeZRf7qefYIwVqUAPf5FNURB12Hw6wJHCMpgmuzxkRS+Q1n7/1N5c6u
WvXL/i8I7/5PaZlmvCaoAJ3r4yyiDDPTFs55/TvmqgJDWKFDF8PyszhSYXVGeqP2
8HayzyLocQKBgDhpFvevzEMoNQ3eAKekYXi2A/nd9hjKaG5uWwGev4LisajFABql
O2MEr9Jua4Y4dCtiudssnozE7tZkudylb++orlLkIK8AmwZTpyPhyA6aZ2s2638V
qFMnAWwRNFvQlRGpotdBuink+Yx8TjYSaEKO80/Y5vs/dLu7xb0mhAFhAoGAc95a
YvZMpcCezFg6eAAYiWxu1NGj+HQkPxVQYR1NW3KK9J+OvBJL+0mxAsMqqXV7/0z2
ZcD2tvTgZBJvX1KdJEqMGVItkMT3sQCY6kD6kU6yFoCW8nugIcGOHs2cuanqXI5g
iRsovRaoZl/h0QmoNo2noyNgKfHGFRSzNOXIBkECgYEAph7pIWcLrtikgeO/KOQl
Y7hh4X0wa01O65MLQTRVVbG7L7eO14b20/+dWMclkowB7WE2/G+PMoxK9AEmTEN3
sYUI1xZFJBXRBoaBu6eF94YD5JKHxsm80gSsAHACPui5VVoMSoHPA8YXxlHh5nFC
rBTxwRqn0v9lv8H7GtnYwaw=
-----END PRIVATE KEY-----";

    pub(crate) fn test_account() -> ChannelAccountRecord {
        ChannelAccountRecord {
            account_id: "acct-gchat".to_string(),
            kind: ChannelKind::GoogleChat,
            label: "Test Google Chat".to_string(),
            enabled: true,
            non_secret_config: serde_json::json!({ "project_number": "123456789" }),
            credential_ref: Some("gchat-cred".to_string()),
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

    fn adapter() -> GoogleChatAdapter {
        let account = test_account();
        let secret = serde_json::json!({
            "client_email": "bot@test-project.iam.gserviceaccount.com",
            "private_key": TEST_PRIVATE_KEY_PEM,
        })
        .to_string();
        let config = AdapterConfig {
            account: &account,
            secret,
        };
        GoogleChatAdapter::new(&config).expect("adapter builds")
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

    pub(crate) fn valid_claims(now_secs: i64) -> JsonValue {
        serde_json::json!({
            "iss": EXPECTED_ISSUER,
            "aud": "123456789",
            "exp": now_secs + 3600,
            "iat": now_secs - 60,
        })
    }

    // --- Authentication audience ------------------------------------------

    fn signed(claims: &JsonValue) -> Vec<(String, String)> {
        vec![(
            "authorization".to_string(),
            format!(
                "Bearer {}",
                sign_test_jwt(claims, "test-key-1", TEST_PRIVATE_KEY_PEM)
            ),
        )]
    }

    /// The one supported Authentication Audience: `aud` is the project number,
    /// and a genuinely signed token carrying it verifies.
    #[test]
    fn the_project_number_is_what_a_delivery_is_verified_against() {
        let adapter = adapter();
        adapter.seed_jwks_for_test();
        let now_ms = 1_700_000_000_000i64;
        adapter
            .verify_and_normalize(
                &signed(&valid_claims(now_ms / 1000)),
                &serde_json::to_vec(&dm_message_event()).unwrap(),
                Some("https://monkey.example.test"),
                now_ms,
            )
            .expect("the project number is the audience");
    }

    /// The callback URL is Google's *other* Authentication Audience setting,
    /// which mints a different token from a different issuer. Nothing here
    /// accepts one, and the operator's own public base cannot make it so.
    #[test]
    fn a_token_whose_audience_is_the_callback_url_is_refused() {
        let adapter = adapter();
        adapter.seed_jwks_for_test();
        let now_ms = 1_700_000_000_000i64;
        let mut claims = valid_claims(now_ms / 1000);
        claims["aud"] = JsonValue::from("https://monkey.example.test/v1/channels/acct-gchat");

        let result = adapter.verify_and_normalize(
            &signed(&claims),
            &serde_json::to_vec(&dm_message_event()).unwrap(),
            Some("https://monkey.example.test"),
            now_ms,
        );
        assert!(result.unwrap_err().contains("audience"));
    }

    /// An account cannot select an audience mode at all: the key is not a
    /// Google Chat setting, so a config carrying it is refused before it is
    /// ever stored.
    #[test]
    fn an_audience_mode_is_not_a_setting_this_provider_has() {
        let error = crate::daemon::adapters::validate_non_secret_config(
            ChannelKind::GoogleChat,
            &serde_json::json!({ "project_number": "123456789", "auth_audience": "app_url" }),
        )
        .expect_err("there is no audience mode to choose");
        assert!(error.contains("auth_audience"), "{error}");
    }

    #[test]
    fn the_project_number_is_required() {
        let mut account = test_account();
        account.non_secret_config = serde_json::json!({});
        let error = GoogleChatAdapter::new(&AdapterConfig {
            account: &account,
            secret: serde_json::json!({
                "client_email": "bot@test-project.iam.gserviceaccount.com",
                "private_key": TEST_PRIVATE_KEY_PEM,
            })
            .to_string(),
        })
        .err()
        .expect("no audience to verify against");
        assert!(error.contains("project_number"));
    }

    /// A token that is valid on any clock a test in this tree runs on — see
    /// the Teams adapter's helper of the same name for why.
    pub(crate) fn long_lived_claims() -> JsonValue {
        serde_json::json!({
            "iss": EXPECTED_ISSUER,
            "aud": "123456789",
            "exp": 4_000_000_000i64,
            "iat": 1_500_000_000i64,
        })
    }

    // --- Key parsing ---------------------------------------------------

    #[test]
    fn the_adapter_builds_from_a_valid_pkcs8_pem_key() {
        let _ = adapter();
    }

    #[test]
    fn a_malformed_private_key_is_rejected_at_construction() {
        let account = test_account();
        let secret = serde_json::json!({
            "client_email": "bot@test-project.iam.gserviceaccount.com",
            "private_key": "not a real key",
        })
        .to_string();
        let config = AdapterConfig {
            account: &account,
            secret,
        };
        assert!(GoogleChatAdapter::new(&config).is_err());
    }

    // --- Structural checks + mandatory refusal -----------------------------

    /// A second, unrelated 2048-bit RSA test key: only ever used as the
    /// "wrong signer" in tests below. Not used anywhere else and grants
    /// access to nothing.
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
    /// [`JwksCache::seed_for_test`] stands in for the network fetch.
    const TEST_JWK_N: &str = "2s0aQeyxkfbzRHrdTfuCx3jDrRw-sx18u28eLf2JSNBFVT23pKMZKhuU1yyM6zqCnA6BIDr7OWaWVc3WhVueSdWJ1HUvOFort633ATQjTmVwv6LzqS70ziFP4KoxTNOUKrK10FSuv4kRznKDli7vOrOxq8JIyd0NK5GoyyjZV42eM2ZPHMqNxcEwKJtCc_GdZPwB7wp7k1u6JnFUTZyp-LzSs3W50lu8Bo1zbrr3CHvhHhCHwqXXfrtvjY-ILesDcRtDeMpZ8HfYn6mWl1-gnRfVqMopI1eWhQeSr58NgKlfJgKkAsJrFx5igpgJwkQK9M_Y3aeslGVumlsZybwJBw";
    const TEST_JWK_E: &str = "AQAB";

    /// The `kid` this file's test key is published under for the tests that
    /// drive the production HTTP route, which builds its own adapter and so
    /// cannot be handed a seeded cache.
    pub(crate) const ROUTE_KID: &str = "gchat-route-key";

    /// Publish the test key as Google's own JWKS would, so every cache built
    /// from here on can verify a token signed with it.
    pub(crate) fn publish_route_jwk() {
        super::super::jwt::test_keys::publish(ROUTE_KID, test_jwk());
    }

    pub(crate) fn test_jwk() -> JwkRsaKey {
        JwkRsaKey {
            n: URL_SAFE_NO_PAD.decode(TEST_JWK_N).unwrap(),
            e: URL_SAFE_NO_PAD.decode(TEST_JWK_E).unwrap(),
            // Google's JWKS declares no endorsements, and nothing in this
            // adapter reads them.
            endorsements: Vec::new(),
        }
    }

    /// Signs `claims` with `kid` in the header, using `private_key_pem`.
    /// Real signing (not a placeholder), exercised against `ring`'s own
    /// verifier the same way [`the_service_account_jwt_signs_and_verifies_with_its_own_public_key`]
    /// already does for the outbound assertion — this is the same technique
    /// aimed at an inbound delivery instead.
    pub(crate) fn sign_test_jwt(claims: &JsonValue, kid: &str, private_key_pem: &str) -> String {
        let header = serde_json::json!({"alg": "RS256", "typ": "JWT", "kid": kid});
        let signing_input = format!(
            "{}.{}",
            URL_SAFE_NO_PAD.encode(header.to_string()),
            URL_SAFE_NO_PAD.encode(claims.to_string()),
        );
        let der = pkcs8_der_from_pem(private_key_pem).unwrap();
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
    fn a_genuine_signed_token_verifies_and_its_event_normalizes() {
        let mut account = test_account();
        account.non_secret_config =
            serde_json::json!({ "project_number": "123456789", "bot_user_name": "users/bot" });
        let secret = serde_json::json!({
            "client_email": "bot@test-project.iam.gserviceaccount.com",
            "private_key": TEST_PRIVATE_KEY_PEM,
        })
        .to_string();
        let config = AdapterConfig {
            account: &account,
            secret,
        };
        let adapter = GoogleChatAdapter::new(&config).unwrap();
        adapter.jwks_cache.seed_for_test("test-key-1", test_jwk());

        let now_ms = 1_700_000_000_000i64;
        let jwt = sign_test_jwt(
            &valid_claims(now_ms / 1000),
            "test-key-1",
            TEST_PRIVATE_KEY_PEM,
        );
        let body = serde_json::to_vec(&dm_message_event()).unwrap();
        let envelopes = adapter
            .verify_and_normalize(
                &[("authorization".to_string(), format!("Bearer {jwt}"))],
                &body,
                None,
                now_ms,
            )
            .expect("a genuinely signed, structurally valid token must verify");
        assert_eq!(envelopes.len(), 1);
        assert_eq!(envelopes[0].text, "hello there");
        assert_eq!(envelopes[0].conversation.kind, ConversationKind::Direct);
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
        // `try_refresh_blocking`'s `Handle::try_current()` fails and the
        // adapter never attempts a fetch at all. That this call returns
        // promptly (rather than hanging on a hypothetical unbounded retry
        // loop) is the "does not spin" guarantee, and it holds without a
        // mock network because there is no network path to exercise here.
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
        claims["iat"] = serde_json::json!(now_ms / 1000 + 3600);
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
        claims["iss"] = serde_json::json!("someone-else@example.com");
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
        claims["aud"] = serde_json::json!("999999999");
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
        let adapter = adapter();
        assert!(adapter.verify_and_normalize(&[], b"{}", None, 0).is_err());
    }

    #[test]
    fn no_secret_appears_in_any_rendered_error_string() {
        let adapter = adapter();
        let result = adapter.verify_and_normalize(&[], b"{}", None, 0);
        let error = result.unwrap_err();
        assert!(!error.contains("BEGIN PRIVATE KEY") && !error.contains(TEST_PRIVATE_KEY_PEM));
    }

    // --- Normalization (pure function, fixtures) --------------------------

    fn dm_message_event() -> JsonValue {
        serde_json::json!({
            "type": "MESSAGE",
            "eventTime": "2024-01-01T00:00:00.000Z",
            "message": {
                "name": "spaces/AAAA/messages/BBBB",
                "sender": {"name": "users/111", "displayName": "Ada", "type": "HUMAN"},
                "text": "hello there",
                "argumentText": "hello there",
                "space": {"name": "spaces/AAAA", "type": "DM"}
            },
            "space": {"name": "spaces/AAAA", "type": "DM"}
        })
    }

    #[test]
    fn a_dm_space_normalizes_to_direct() {
        let envelope = normalize_event(&dm_message_event(), "acct-gchat", "users/bot", 0).unwrap();
        assert_eq!(envelope.conversation.kind, ConversationKind::Direct);
        assert_eq!(envelope.provider_event_id, "spaces/AAAA/messages/BBBB");
        assert_eq!(envelope.text, "hello there");
    }

    #[test]
    fn a_room_space_normalizes_to_group() {
        let mut event = dm_message_event();
        event["space"]["type"] = serde_json::json!("ROOM");
        event["message"]["space"]["type"] = serde_json::json!("ROOM");
        let envelope = normalize_event(&event, "acct-gchat", "users/bot", 0).unwrap();
        assert_eq!(envelope.conversation.kind, ConversationKind::Group);
    }

    #[test]
    fn a_thread_name_separates_threads() {
        let mut event = dm_message_event();
        event["message"]["thread"] = serde_json::json!({"name": "spaces/AAAA/threads/CCCC"});
        let envelope = normalize_event(&event, "acct-gchat", "users/bot", 0).unwrap();
        assert_eq!(
            envelope.conversation.thread_id.as_deref(),
            Some("spaces/AAAA/threads/CCCC")
        );
    }

    #[test]
    fn argument_text_is_preferred_over_raw_text() {
        let mut event = dm_message_event();
        event["message"]["text"] = serde_json::json!("@Bot hello there");
        event["message"]["argumentText"] = serde_json::json!(" hello there");
        let envelope = normalize_event(&event, "acct-gchat", "users/bot", 0).unwrap();
        assert_eq!(envelope.text, " hello there");
    }

    #[test]
    fn raw_text_is_used_when_argument_text_is_absent() {
        let mut event = dm_message_event();
        event["message"]
            .as_object_mut()
            .unwrap()
            .remove("argumentText");
        event["message"]["text"] = serde_json::json!("plain text");
        let envelope = normalize_event(&event, "acct-gchat", "users/bot", 0).unwrap();
        assert_eq!(envelope.text, "plain text");
    }

    #[test]
    fn a_user_mention_of_the_app_is_detected() {
        let mut event = dm_message_event();
        event["message"]["annotations"] = serde_json::json!([{
            "type": "USER_MENTION",
            "userMention": {"user": {"name": "users/bot", "type": "BOT"}}
        }]);
        let envelope = normalize_event(&event, "acct-gchat", "users/bot", 0).unwrap();
        assert!(envelope.mentions_self);
    }

    #[test]
    fn a_user_mention_of_someone_else_does_not_set_mentions_self() {
        let mut event = dm_message_event();
        event["message"]["annotations"] = serde_json::json!([{
            "type": "USER_MENTION",
            "userMention": {"user": {"name": "users/222", "type": "HUMAN"}}
        }]);
        let envelope = normalize_event(&event, "acct-gchat", "users/bot", 0).unwrap();
        assert!(!envelope.mentions_self);
    }

    #[test]
    fn a_non_message_event_normalizes_to_nothing() {
        let mut event = dm_message_event();
        event["type"] = serde_json::json!("ADDED_TO_SPACE");
        assert!(normalize_event(&event, "acct-gchat", "users/bot", 0).is_none());
    }

    #[test]
    fn provider_event_ids_are_deterministic() {
        let first = normalize_event(&dm_message_event(), "acct-gchat", "users/bot", 0).unwrap();
        let second = normalize_event(&dm_message_event(), "acct-gchat", "users/bot", 0).unwrap();
        assert_eq!(first.provider_event_id, second.provider_event_id);
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
            account_id: "acct-gchat".to_string(),
            kind: ChannelKind::GoogleChat,
            conversation_id: "spaces/AAAA".to_string(),
            thread_id: Some("spaces/AAAA/threads/CCCC".to_string()),
            text: "hi".to_string(),
            attachments: Vec::new(),
            reply_to_provider_id: None,
            idempotency_key: "idem-1".to_string(),
        }
    }

    #[tokio::test]
    async fn a_429_response_maps_to_retryable_failure() {
        let token_base = serve_forever("200 OK", r#"{"access_token":"tok","expires_in":3600}"#);
        let send_base = serve_once("429 Too Many Requests", "{}");
        let outcome = adapter()
            .with_bases(&send_base, &token_base)
            .send(&outbound_message())
            .await;
        assert!(
            matches!(outcome, SendOutcome::RetryableFailure { .. }),
            "{outcome:?}"
        );
    }

    #[tokio::test]
    async fn a_401_response_maps_to_permanent_failure() {
        let token_base = serve_forever("200 OK", r#"{"access_token":"tok","expires_in":3600}"#);
        let send_base = serve_once("401 Unauthorized", "{}");
        let outcome = adapter()
            .with_bases(&send_base, &token_base)
            .send(&outbound_message())
            .await;
        assert!(
            matches!(outcome, SendOutcome::PermanentFailure { .. }),
            "{outcome:?}"
        );
    }

    #[tokio::test]
    async fn a_successful_send_extracts_the_provider_message_id() {
        let token_base = serve_forever("200 OK", r#"{"access_token":"tok","expires_in":3600}"#);
        let send_base = serve_once("200 OK", r#"{"name":"spaces/AAAA/messages/OUT1"}"#);
        let outcome = adapter()
            .with_bases(&send_base, &token_base)
            .send(&outbound_message())
            .await;
        match outcome {
            SendOutcome::Sent {
                provider_message_id,
            } => {
                assert_eq!(
                    provider_message_id.as_deref(),
                    Some("spaces/AAAA/messages/OUT1")
                );
            }
            other => panic!("expected Sent, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn probe_reports_connected_when_the_token_exchange_succeeds() {
        let token_base = serve_forever("200 OK", r#"{"access_token":"tok","expires_in":3600}"#);
        let health = adapter()
            .with_bases(CHAT_API_BASE, &token_base)
            // Port 1 on loopback: nothing listens there, so the JWKS refresh
            // probe() attempts fails fast with connection-refused rather than
            // reaching the real Google endpoint. probe() treats that refresh
            // as best-effort, so it does not affect the assertion below.
            .with_jwks_url("http://127.0.0.1:1/jwks")
            .probe()
            .await;
        assert_eq!(health.state, HealthState::Connected);
    }

    #[tokio::test]
    async fn probe_reports_error_when_the_token_exchange_fails_and_leaks_no_secret() {
        let token_base = serve_forever("401 Unauthorized", r#"{"error":"invalid_grant"}"#);
        let health = adapter()
            .with_bases(CHAT_API_BASE, &token_base)
            .with_jwks_url("http://127.0.0.1:1/jwks")
            .probe()
            .await;
        assert_eq!(health.state, HealthState::Error);
        let error = health.last_error.unwrap_or_default();
        assert!(!error.contains("BEGIN PRIVATE KEY"));
    }

    #[tokio::test]
    async fn the_service_account_jwt_signs_and_verifies_with_its_own_public_key() {
        // Exercises the real RSA signing path end to end without a network:
        // mint an assertion, then verify it with ring's own verifier against
        // the public key recoverable from the same PKCS8 key, proving the
        // signature is well-formed and over the exact signing input.
        let der = pkcs8_der_from_pem(TEST_PRIVATE_KEY_PEM).unwrap();
        let jwt = mint_service_account_jwt(
            "bot@test-project.iam.gserviceaccount.com",
            &der,
            1_700_000_000,
        )
        .unwrap();
        let mut parts = jwt.split('.');
        let header = parts.next().unwrap();
        let payload = parts.next().unwrap();
        let signature = URL_SAFE_NO_PAD.decode(parts.next().unwrap()).unwrap();
        let signing_input = format!("{header}.{payload}");

        let key_pair = RsaKeyPair::from_pkcs8(&der).unwrap();
        let public_key = ring::signature::UnparsedPublicKey::new(
            &ring::signature::RSA_PKCS1_2048_8192_SHA256,
            ring::signature::KeyPair::public_key(&key_pair).as_ref(),
        );
        assert!(public_key
            .verify(signing_input.as_bytes(), &signature)
            .is_ok());
    }
}
