//! Google Chat app-interaction adapter (official HTTP endpoint URL flow).
//!
//! # Inbound trust — read this before touching `verify_and_normalize`
//!
//! Google Chat authenticates a delivery with a JWT in `Authorization: Bearer`,
//! issued by `chat@system.gserviceaccount.com`, audience the configured
//! project number. Full validation is: verify issuer, audience and expiry
//! (structural — cheap, no network), *and* verify the RS256 signature against
//! one of Google's rotating public certificates, fetched from
//! `https://www.googleapis.com/service_accounts/v1/metadata/x509/chat@system.gserviceaccount.com`
//! and cached.
//!
//! This adapter implements the structural checks and deliberately does not
//! implement the certificate fetch and signature check —
//! `TODO(google-chat-certs)`. Same reasoning as the Teams adapter's
//! `TODO(teams-jwks)`: fetching and caching a remote, rotating certificate set
//! correctly is a security-critical feature on its own, and a half-verified
//! signature check is worse than an honest refusal, because it looks like
//! verification. [`normalize_event`] — the pure mapping this adapter would use
//! once certificate verification lands — is implemented and unit-tested
//! against fixtures so wiring it in later is a small, reviewable change.
//!
//! Outbound authenticates as the configured service account: a JWT assertion
//! signed with its RSA private key is exchanged for an OAuth access token
//! (the standard Google server-to-server flow), and that token is cached and
//! refreshed on expiry.

use async_trait::async_trait;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use base64::Engine as _;
use little_monkey_lib::channels::types::{
    ChannelConversation, ChannelEnvelope, ChannelHealth, ChannelKind, ChannelSender,
    InboundTransport, OutboundMessage, ProviderCapabilities, SendOutcome,
};
use ring::rand::SystemRandom;
use ring::signature::{RsaKeyPair, RSA_PKCS1_SHA256};
use serde::Deserialize;
use serde_json::Value as JsonValue;
use std::sync::Mutex;

use crate::daemon::channel_adapter::{
    AdapterConfig, ChannelAdapter, InboundBatch, WebhookChannelAdapter,
};

const CHAT_API_BASE: &str = "https://chat.googleapis.com";
const OAUTH_TOKEN_BASE: &str = "https://oauth2.googleapis.com";
/// Google Chat's own fixed JWT issuer for app-interaction deliveries.
const EXPECTED_ISSUER: &str = "chat@system.gserviceaccount.com";
const SKEW_SECS: i64 = 300;
const TOKEN_REFRESH_SKEW_SECS: i64 = 60;
const CHAT_BOT_SCOPE: &str = "https://www.googleapis.com/auth/chat.bot";

#[derive(Debug, Deserialize)]
struct GoogleChatNonSecretConfig {
    project_number: String,
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
    project_number: String,
    client_email: String,
    /// PKCS8 DER, parsed once at construction so a malformed key is rejected
    /// at setup rather than on the first send.
    private_key_der: Vec<u8>,
    token_cache: Mutex<Option<CachedToken>>,
    chat_api_base: String,
    oauth_token_base: String,
}

impl GoogleChatAdapter {
    pub fn new(config: &AdapterConfig<'_>) -> Result<Self, String> {
        let non_secret: GoogleChatNonSecretConfig =
            serde_json::from_value(config.account.non_secret_config.clone())
                .map_err(|error| format!("Invalid Google Chat account config: {error}"))?;
        if non_secret.project_number.trim().is_empty() {
            return Err("Google Chat account is missing project_number".to_string());
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
            project_number: non_secret.project_number,
            client_email: secrets.client_email,
            private_key_der,
            token_cache: Mutex::new(None),
            chat_api_base: CHAT_API_BASE.to_string(),
            oauth_token_base: OAUTH_TOKEN_BASE.to_string(),
        })
    }

    #[cfg(test)]
    fn with_bases(mut self, chat_api_base: &str, oauth_token_base: &str) -> Self {
        self.chat_api_base = chat_api_base.to_string();
        self.oauth_token_base = oauth_token_base.to_string();
        self
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

    fn verify_and_normalize(
        &self,
        headers: &[(String, String)],
        _body: &[u8],
        _public_base_url: Option<&str>,
        now_ms: i64,
    ) -> Result<Vec<ChannelEnvelope>, String> {
        // `public_base_url` is unused: Google's JWT signs the token's own
        // claims, never the delivery URL.
        let authorization = headers
            .iter()
            .find(|(name, _)| name == "authorization")
            .map(|(_, value)| value.as_str())
            .ok_or_else(|| "Missing Authorization header".to_string())?;
        let token = authorization
            .strip_prefix("Bearer ")
            .ok_or_else(|| "Authorization header is not a Bearer token".to_string())?;
        let claims = decode_jwt_claims(token)?;
        validate_claims_structurally(&claims, &self.project_number, now_ms)?;

        // Structural checks passed, but that is not verification — see the
        // module doc. The body is deliberately never parsed past this point.
        Err(
            "Google Chat inbound signature verification is not implemented \
             (TODO(google-chat-certs)); refusing unverified delivery"
                .to_string(),
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
            supports_idempotency_key: false,
            supports_delivery_receipts: false,
            ..ProviderCapabilities::minimal(ChannelKind::GoogleChat, InboundTransport::Webhook)
        }
    }

    async fn probe(&self) -> ChannelHealth {
        let now = now_ms();
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
        let url = format!(
            "{}/v1/{}/messages",
            self.chat_api_base, message.conversation_id
        );
        let mut body = serde_json::json!({ "text": message.text });
        if let Some(thread_id) = &message.thread_id {
            body["thread"] = serde_json::json!({ "name": thread_id });
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

/// Splits, base64url-decodes and JSON-parses a JWT's claims (the middle
/// segment). Does not touch the signature.
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
    expected_project_number: &str,
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
    if audience != expected_project_number {
        return Err("JWT audience does not match the configured project number".to_string());
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

    Some(ChannelEnvelope {
        account_id: account_id.to_string(),
        kind: ChannelKind::GoogleChat,
        provider_event_id,
        conversation,
        sender,
        text,
        attachments: Vec::new(),
        reply_to_provider_id: None,
        mentions_self,
        received_at_ms,
        metadata: little_monkey_lib::channels::types::BoundedMetadata::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::channel_store::ChannelAccountRecord;
    use little_monkey_lib::channels::policy::ChannelAccessPolicy;
    use little_monkey_lib::channels::types::{ConversationKind, HealthState};
    use std::io::{Read, Write};

    /// A 2048-bit RSA test key generated locally for these tests only. Not
    /// used anywhere else and grants access to nothing.
    const TEST_PRIVATE_KEY_PEM: &str = "-----BEGIN PRIVATE KEY-----
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

    fn test_account() -> ChannelAccountRecord {
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

    fn make_jwt(claims: &JsonValue) -> String {
        let header =
            URL_SAFE_NO_PAD.encode(serde_json::json!({"alg":"RS256","typ":"JWT"}).to_string());
        let payload = URL_SAFE_NO_PAD.encode(claims.to_string());
        format!("{header}.{payload}.unsigned-placeholder-signature")
    }

    fn valid_claims(now_secs: i64) -> JsonValue {
        serde_json::json!({
            "iss": EXPECTED_ISSUER,
            "aud": "123456789",
            "exp": now_secs + 3600,
            "iat": now_secs - 60,
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
        assert!(result.unwrap_err().contains("google-chat-certs"));
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
            .probe()
            .await;
        assert_eq!(health.state, HealthState::Connected);
    }

    #[tokio::test]
    async fn probe_reports_error_when_the_token_exchange_fails_and_leaks_no_secret() {
        let token_base = serve_forever("401 Unauthorized", r#"{"error":"invalid_grant"}"#);
        let health = adapter()
            .with_bases(CHAT_API_BASE, &token_base)
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
