//! Microsoft Teams (Bot Framework Activity protocol) adapter.
//!
//! # Inbound trust — read this before touching `verify_and_normalize`
//!
//! The Bot Framework authenticates a webhook delivery with a JWT in
//! `Authorization: Bearer`. Full validation is five things, and this adapter
//! does all five:
//!
//! 1. `alg` pinned to RS256 by name, never read from the token;
//! 2. the RS256 signature, against a key from Microsoft's own OpenID metadata
//!    document (fetched and cached as a JWKS);
//! 3. issuer, audience, `exp` and `nbf`;
//! 4. the activity's channel identity and the signing key's **endorsements** —
//!    the activity must name the Teams channel, and the Bot Framework
//!    publishes, per key, which channels that key may sign for, so the key must
//!    be endorsed for Teams ([`validate_channel_is_endorsed`]);
//! 5. the token's `serviceurl` claim against the activity's own `serviceUrl`
//!    ([`validate_service_url_claim`]) — the JWT signs its claims, not the
//!    body, and this is the claim that binds the two. Without it a valid token
//!    could be replayed with a body naming a different endpoint, and the reply
//!    address stored below would be that endpoint.
//!
//! The signature machinery itself is the provider-agnostic JWKS core in
//! `jwt.rs`, shared with Google Chat, since both publish RSA keys the same way.
//! What is Teams-specific is *where* the document lives: rather than a fixed
//! JWKS URL, Bot Framework publishes an OpenID Connect discovery document whose
//! `jwks_uri` names the actual JWKS. [`TeamsAdapter::refresh_keys`] is that
//! two-step fetch; [`TeamsAdapter::ensure_key_for_kid`] is the sync-side lookup
//! with the one-shot catch-up on an unknown `kid` — see
//! `jwt::try_refresh_blocking`'s doc for why that bridge is safe.
//!
//! # Which cloud
//!
//! The issuer, the metadata document, the OAuth authority, the token scope and
//! the hosts a reply may be sent to are one thing, not five: they all name the
//! same Bot Framework cloud, and any one of them set independently describes a
//! cloud the other four do not. [`TeamsEnvironment`] holds them together and
//! is the only source for all five, inbound and outbound alike.
//!
//! [`normalize_activity`] is the pure mapping from a verified activity to an
//! envelope, unit-tested against fixtures independently of verification.
//!
//! Outbound uses the client-credentials flow against the configured tenant,
//! with the resulting token cached and refreshed on expiry.
//!
//! # Where a reply is addressed — read this before touching `send`
//!
//! The Bot Framework does not accept a conversation id alone. A reply is POSTed
//! to the `serviceUrl` the inbound activity carried, which is per conversation
//! and per region, so without it there is no endpoint at all. That value is
//! therefore *durable state*, not a cache: a turn accepted before a restart
//! still owes an answer afterwards, and a process-local map cannot give it one.
//! [`TeamsAdapter::conversation_reference_for`] produces it — only from an
//! activity whose JWT has already verified, and only after
//! [`validate_service_url`] — and it leaves this adapter as
//! [`DurableAddressing`], which the webhook acceptance path commits *before*
//! the provider is answered. `send` loads it back through
//! [`ConversationReferences`]. Nothing derives, reconstructs or defaults it.
//!
//! That the address is committed by the acceptance path rather than written
//! here is the correctness property, not a refactor: a message this build has
//! acknowledged but cannot address is a message nobody will ever get an answer
//! to, and no amount of retrying fixes it because the provider considers it
//! delivered. So the two are one acceptance — event and address together, or
//! neither and a redelivery.
//!
//! The bearer token is the opposite and stays that way: acquired from the
//! operator's own app credentials, cached in memory with its expiry, never
//! written anywhere.

use async_trait::async_trait;
use little_monkey_lib::channels::types::{
    AttachmentKind, AttachmentSource, BoundedMetadata, ChannelAttachment, ChannelConversation,
    ChannelEnvelope, ChannelHealth, ChannelKind, ChannelSender, InboundTransport, OutboundMessage,
    ProviderCapabilities, SendOutcome,
};
use serde::Deserialize;
use serde_json::Value as JsonValue;
use std::sync::Mutex;

use super::jwt::{
    decode_jwt, fetch_bytes_via_egress, fetch_jwks_via_egress, parse_jwks_uri_from_metadata,
    try_refresh_blocking, validate_alg_is_rs256, verify_rs256_signature, JwkRsaKey, JwksCache,
};
use crate::daemon::channel_adapter::{
    fetch_url, AdapterConfig, ChannelAdapter, ConversationReferences, DaemonConversationReferences,
    DurableAddressing, InboundBatch, WebhookChannelAdapter,
};

/// Applies to `exp`/`nbf` alike, per the shared skew rule this file follows.
const SKEW_SECS: i64 = 300;
/// Refresh this many seconds before the token's own `expires_in` — never cut
/// it exactly at the edge, or a request built just before expiry could be
/// sent with a token that dies in flight.
const TOKEN_REFRESH_SKEW_SECS: i64 = 60;

/// The five values a Bot Framework cloud is defined by, held together so they
/// cannot drift apart.
///
/// Every one of them is security-sensitive and every one of them is only
/// meaningful in combination: the issuer a token must claim, the metadata
/// document its signing keys are published in, the authority the bot's own
/// token is bought from, the scope it is bought for, and the hosts a reply may
/// be POSTed to. An account that could set any one of them on its own — which
/// is what a free-form "OpenID metadata URL" box was — could point key
/// discovery at one cloud while the rest of the checks still described
/// another.
///
/// Only the public cloud is offered, because it is the only one this build can
/// be shown working end to end. A sovereign-cloud variant is five constants,
/// but five constants nobody here can verify are five ways to accept a token
/// that should have been refused, so the option is absent rather than
/// advertised — see the PR notes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TeamsEnvironment {
    openid_metadata_url: &'static str,
    oauth_authority: &'static str,
    token_scope: &'static str,
    expected_issuer: &'static str,
    /// A `serviceUrl` host must equal one of these or end with a dot and one
    /// of these — never a bare suffix match, which `evil-botframework.com`
    /// passes.
    allowed_service_url_hosts: &'static [&'static str],
}

impl TeamsEnvironment {
    /// The public Bot Framework cloud, which is what `teams.microsoft.com`
    /// tenants and the Azure Bot resource default to.
    const PUBLIC: Self = Self {
        openid_metadata_url: "https://login.botframework.com/v1/.well-known/openidconfiguration",
        oauth_authority: "https://login.microsoftonline.com",
        token_scope: "https://api.botframework.com/.default",
        expected_issuer: "https://api.botframework.com",
        allowed_service_url_hosts: &[
            "api.botframework.com",
            "botframework.com",
            "trafficmanager.net",
        ],
    };
}

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

/// The addressing a reply needs, read back out of the durable reference.
struct TeamsConversation {
    /// Already trailing-slash trimmed and re-validated.
    service_url: String,
    bot_id: Option<String>,
    bot_name: Option<String>,
}

pub struct TeamsAdapter {
    account_id: String,
    app_id: String,
    tenant_id: String,
    app_password: String,
    token_cache: Mutex<Option<CachedToken>>,
    /// Where a reply to each conversation is addressed. Durable, because a
    /// turn accepted before a restart still owes an answer after one — see the
    /// module doc. Read here and written by the acceptance path; `send`
    /// refuses to guess or derive an endpoint.
    references: std::sync::Arc<dyn ConversationReferences>,
    /// What the last verified delivery established about where to answer,
    /// waiting for the acceptance path to drain and commit it. Empty at every
    /// other moment — see [`WebhookChannelAdapter::take_durable_addressing`].
    pending_addressing: Mutex<Vec<DurableAddressing>>,
    /// Which Bot Framework cloud this account belongs to. Decides the issuer
    /// that is accepted, where signing keys come from, where the bot's own
    /// token is bought and what a reply may be addressed to — all five
    /// together, never one at a time.
    environment: TeamsEnvironment,
    /// Identity provider origin. Always the environment's own authority in
    /// production; swappable in tests.
    login_base: String,
    /// Cached RS256 JWKS keys for verifying inbound deliveries. See the
    /// module doc for how this is kept warm.
    jwks_cache: JwksCache,
    /// Where the `jwks_uri` is discovered. Always the environment's own
    /// metadata document in production; swappable in tests.
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
        let environment = TeamsEnvironment::PUBLIC;
        Ok(Self {
            account_id: config.account.account_id.clone(),
            app_id: non_secret.app_id,
            tenant_id: non_secret.tenant_id,
            app_password: secrets.app_password,
            token_cache: Mutex::new(None),
            references: std::sync::Arc::new(DaemonConversationReferences::new()),
            pending_addressing: Mutex::new(Vec::new()),
            environment,
            login_base: environment.oauth_authority.to_string(),
            jwks_cache: JwksCache::new(),
            metadata_url: environment.openid_metadata_url.to_string(),
        })
    }

    /// Point the durable reply-address store at a specific daemon's state.
    ///
    /// `None` leaves it resolving the running daemon's own paths, which is what
    /// a caller with no state of its own (the `channels probe` command) wants.
    pub(crate) fn with_state(mut self, state: Option<&crate::daemon::store::DaemonPaths>) -> Self {
        if let Some(paths) = state {
            self.references = std::sync::Arc::new(DaemonConversationReferences::at(paths.clone()));
        }
        self
    }

    #[cfg(test)]
    pub(crate) fn with_login_base(mut self, base: &str) -> Self {
        self.login_base = base.to_string();
        self
    }

    #[cfg(test)]
    pub(crate) fn with_metadata_url(mut self, url: &str) -> Self {
        self.metadata_url = url.to_string();
        self
    }

    /// Stand in for the JWKS fetch with this file's own test key, so a test
    /// can verify a genuinely signed token without reaching Microsoft.
    #[cfg(test)]
    pub(crate) fn seed_jwks_for_test(&self) {
        self.jwks_cache
            .seed_for_test("test-key-1", tests::test_jwk());
    }

    /// The same, with the key published as endorsing exactly `channels`, so a
    /// test can drive the endorsement check from both sides.
    #[cfg(test)]
    pub(crate) fn seed_jwks_endorsing_for_test(&self, channels: &[&str]) {
        self.jwks_cache
            .seed_for_test("test-key-1", tests::test_jwk_endorsing(channels));
    }

    /// Swap the durable reference store. Used by the restart tests, which build
    /// two adapters over one store to prove the second can address a
    /// conversation the first was told about.
    #[cfg(test)]
    pub(crate) fn with_references(
        mut self,
        references: std::sync::Arc<dyn ConversationReferences>,
    ) -> Self {
        self.references = references;
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

    /// Where replies to one conversation go, refusing any `serviceUrl` that is
    /// not `https` on a Microsoft-owned Bot Framework host.
    ///
    /// This is the only way `send` learns where to POST — it never
    /// reconstructs a URL or trusts one handed to it another way — and it is
    /// only ever called from [`WebhookChannelAdapter::verify_and_normalize`]
    /// *after* the activity's JWT has verified. That ordering is the security
    /// property: an unauthenticated request cannot make this process POST an
    /// operator's bot token to a host of its choosing.
    ///
    /// No token is produced. What is produced is addressing: the endpoint, the
    /// tenant and conversation shape the Bot Framework needs to route a reply,
    /// and when it was last confirmed.
    fn conversation_reference_for(
        &self,
        conversation_id: &str,
        activity: &JsonValue,
        now_ms: i64,
    ) -> Result<DurableAddressing, String> {
        let service_url = activity
            .get("serviceUrl")
            .and_then(JsonValue::as_str)
            .ok_or_else(|| "Activity carries no serviceUrl".to_string())?;
        validate_service_url(service_url, self.environment)?;

        let conversation = activity.get("conversation");
        let mut reference = serde_json::json!({
            "service_url": service_url,
            "last_updated_at_ms": now_ms,
        });
        // Everything below is optional because Teams omits each of them in
        // some shapes: a personal chat has no tenant on the conversation, and
        // `conversationType` is absent outside channels. What is present is
        // kept, because a proactive reply is addressed with it.
        let mut put = |key: &str, value: Option<&str>| {
            if let Some(value) = value.filter(|value| !value.is_empty()) {
                reference[key] = JsonValue::from(value);
            }
        };
        put(
            "tenant_id",
            activity
                .get("channelData")
                .and_then(|data| data.get("tenant"))
                .and_then(|tenant| tenant.get("id"))
                .and_then(JsonValue::as_str)
                .or_else(|| {
                    conversation
                        .and_then(|conversation| conversation.get("tenantId"))
                        .and_then(JsonValue::as_str)
                }),
        );
        put(
            "conversation_type",
            conversation
                .and_then(|conversation| conversation.get("conversationType"))
                .and_then(JsonValue::as_str),
        );
        put(
            "channel_id",
            activity.get("channelId").and_then(JsonValue::as_str),
        );
        // The bot's own identity on this conversation, which a proactive
        // activity has to name as its `from`.
        put(
            "bot_id",
            activity
                .get("recipient")
                .and_then(|recipient| recipient.get("id"))
                .and_then(JsonValue::as_str),
        );
        put(
            "bot_name",
            activity
                .get("recipient")
                .and_then(|recipient| recipient.get("name"))
                .and_then(JsonValue::as_str),
        );
        Ok(DurableAddressing {
            account_id: self.account_id.clone(),
            conversation_id: conversation_id.to_string(),
            reference,
        })
    }

    /// The stored reference for a conversation, or the reason there is none.
    fn conversation_reference(&self, conversation_id: &str) -> Result<TeamsConversation, String> {
        let stored = self
            .references
            .get(&self.account_id, conversation_id)
            .ok_or_else(|| {
                format!(
                    "No verified serviceUrl on file for conversation '{conversation_id}'; Teams \
                     requires an inbound activity to establish where to send before it can be \
                     messaged"
                )
            })?;
        let service_url = stored
            .get("service_url")
            .and_then(JsonValue::as_str)
            .ok_or_else(|| {
                format!("The stored Teams reference for '{conversation_id}' names no serviceUrl")
            })?;
        // Re-validated on the way out as well as in. The row is only ever
        // written by the path above, but a database is a file on disk and this
        // is the moment a bot token would be sent somewhere.
        validate_service_url(service_url, self.environment)?;
        Ok(TeamsConversation {
            service_url: service_url.trim_end_matches('/').to_string(),
            bot_id: stored
                .get("bot_id")
                .and_then(JsonValue::as_str)
                .map(str::to_string),
            bot_name: stored
                .get("bot_name")
                .and_then(JsonValue::as_str)
                .map(str::to_string),
        })
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
            // The same environment that decided which issuer is accepted
            // inbound decides what this token is bought for.
            ("scope", self.environment.token_scope),
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

    /// The Bot Framework connector treats `200` as delivered and reads the
    /// body as an optional response activity; an empty JSON object is how a
    /// bot that will answer later says there is nothing to send back now.
    fn ack(&self) -> crate::daemon::channel_adapter::WebhookAck {
        crate::daemon::channel_adapter::WebhookAck::json_ok()
    }

    fn take_durable_addressing(&self) -> Vec<DurableAddressing> {
        self.pending_addressing
            .lock()
            .map(|mut pending| std::mem::take(&mut *pending))
            .unwrap_or_default()
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
        validate_claims_structurally(
            &decoded.claims,
            &self.app_id,
            self.environment.expected_issuer,
            now_ms,
        )?;
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

        // The token proves the Bot Framework issued it for this bot. It does
        // not, on its own, say anything about the body it arrived with — the
        // JWT signs its own claims, not the activity. Two claims are what tie
        // the two together, and both are checked before a single field of the
        // body is believed.
        validate_channel_is_endorsed(&activity, &key)?;
        validate_service_url_claim(&decoded.claims, &activity)?;

        // The `serviceUrl` this specific activity carries is the only way
        // `send` learns where to POST for this conversation — produce it now
        // that the activity is verified *and* bound to the token, per
        // `conversation_reference_for`'s own doc, and hand it to the acceptance
        // path, which commits it before this provider is told anything. It has
        // to be durable rather than cached: the turn this delivery becomes may
        // well outlive the process that received it.
        //
        // An activity whose `serviceUrl` is on a host this cloud does not own
        // yields no address, and still normalizes and runs — it just cannot be
        // replied to until one that carries a usable address arrives. Throwing
        // away a real message over that would be worse, and the send says
        // plainly what is missing. What must *not* happen is the other order:
        // an address that exists and is lost. That case is the acceptance
        // path's, and it withholds the acknowledgement.
        if let Some(conversation_id) = activity
            .get("conversation")
            .and_then(|conversation| conversation.get("id"))
            .and_then(JsonValue::as_str)
        {
            if let Ok(addressing) =
                self.conversation_reference_for(conversation_id, &activity, now_ms)
            {
                if let Ok(mut pending) = self.pending_addressing.lock() {
                    pending.push(addressing);
                }
            }
        }

        let bot_id = activity
            .get("recipient")
            .and_then(|recipient| recipient.get("id"))
            .and_then(JsonValue::as_str)
            .unwrap_or_default();
        Ok(normalize_activity(
            &activity,
            &self.account_id,
            bot_id,
            self.environment,
            now_ms,
        )
        .into_iter()
        .collect())
    }
}

/// Refuse an activity whose `serviceUrl` the token does not vouch for.
///
/// A Bot Framework channel token carries a `serviceurl` claim naming the
/// endpoint the activity it accompanies came from. Without this check the JWT
/// only ever proves "somebody holds a token minted for this bot" — a token
/// obtained from one legitimate context could then be replayed with a body
/// naming any Bot Framework host, and this process would durably store that as
/// where to POST the bot's own bearer token. Trailing slashes differ between
/// Microsoft's own payloads, so they are not part of the comparison.
///
/// A token carrying no `serviceurl` claim at all is refused rather than waved
/// through: the claim is what binds token to body, and a delivery this build
/// cannot bind is a delivery it cannot vouch for.
fn validate_service_url_claim(claims: &JsonValue, activity: &JsonValue) -> Result<(), String> {
    let claimed = claims
        .get("serviceurl")
        .and_then(JsonValue::as_str)
        .ok_or_else(|| "JWT carries no serviceurl claim to bind the activity to".to_string())?;
    let activity_url = activity
        .get("serviceUrl")
        .and_then(JsonValue::as_str)
        .ok_or_else(|| "Activity carries no serviceUrl to check against the token".to_string())?;
    if claimed.trim_end_matches('/') != activity_url.trim_end_matches('/') {
        return Err(
            "The activity's serviceUrl is not the one this token was issued for".to_string(),
        );
    }
    Ok(())
}

/// The only channel identity this adapter accepts.
///
/// This is the Teams adapter: the account it belongs to was configured against
/// an Azure Bot resource's Teams channel, its reply endpoints are Teams', and
/// the operator's policy was written about Teams. An activity naming any other
/// Bot Framework channel — `directline`, `webchat`, `emulator`, anything a bot
/// may also be connected to — is a different product arriving at this door, and
/// is refused rather than normalized into a Teams conversation.
const TEAMS_CHANNEL_ID: &str = "msteams";

/// Refuse an activity that is not Teams', or whose signing key is not endorsed
/// for Teams.
///
/// Two separate facts, both required. The `channelId` is the activity's own
/// claim about where it came from, and it must be Teams. The **endorsements**
/// are what the Bot Framework publishes alongside each key in its OpenID key
/// document: the list of channels that key is permitted to sign for. A key not
/// endorsed for Teams signing an activity that claims to be Teams is exactly
/// the substitution this closes, so the endorsement must be *present* and must
/// name Teams.
///
/// A key that publishes no endorsements at all therefore cannot sign a Teams
/// activity here. That is deliberate: an empty list is an absent claim, and
/// waving one through would let any Bot Framework key sign for this channel —
/// which is the whole of what the check is for. Microsoft's Teams signing keys
/// carry the endorsement; a fixture that omits it is a wrong fixture, not a
/// reason to relax this.
fn validate_channel_is_endorsed(activity: &JsonValue, key: &JwkRsaKey) -> Result<(), String> {
    let channel_id = activity
        .get("channelId")
        .and_then(JsonValue::as_str)
        .ok_or_else(|| "Activity names no channelId".to_string())?;
    if channel_id != TEAMS_CHANNEL_ID {
        return Err("This activity is not from the Microsoft Teams channel".to_string());
    }
    if !key
        .endorsements
        .iter()
        .any(|endorsed| endorsed == TEAMS_CHANNEL_ID)
    {
        return Err("The signing key is not endorsed for this activity's channel".to_string());
    }
    Ok(())
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
            supports_attachments: false, // inbound only: this adapter does not upload files yet
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
        // Loaded, never remembered: this is the same lookup whether the
        // activity arrived a second ago or before the last restart.
        let conversation = match self.conversation_reference(&message.conversation_id) {
            Ok(conversation) => conversation,
            Err(error) => return SendOutcome::PermanentFailure { error },
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
        // Replying to a specific activity keeps the answer under the message
        // that prompted it, which in a Teams channel is the difference between
        // a threaded reply and a new post nobody connects to the question.
        // Falling back to the conversation endpoint is what a proactive
        // message — one with no activity to hang from — needs.
        let reply_to = message
            .reply_to_provider_id
            .as_deref()
            .filter(|value| !value.is_empty());
        let url = match activity_url(
            &conversation.service_url,
            &message.conversation_id,
            reply_to,
        ) {
            Ok(url) => url,
            Err(error) => return SendOutcome::PermanentFailure { error },
        };
        let mut body = serde_json::json!({
            "type": "message",
            "text": message.text,
        });
        if let Some(activity_id) = reply_to {
            body["replyToId"] = JsonValue::from(activity_id);
        }
        if let Some(bot_id) = conversation.bot_id.as_deref() {
            let mut from = serde_json::json!({ "id": bot_id });
            if let Some(name) = conversation.bot_name.as_deref() {
                from["name"] = JsonValue::from(name);
            }
            body["from"] = from;
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

    /// Teams serves an attachment from the `contentUrl` its activity named.
    ///
    /// The bot's own token is sent only to a Bot Framework host: a `contentUrl`
    /// is chosen by whoever posted the message, and a URL somewhere else is
    /// fetched anonymously rather than handed the credential.
    async fn fetch_attachment(
        &self,
        attachment: &ChannelAttachment,
        limits: crate::daemon::channel_adapter::AttachmentLimits,
    ) -> Result<Vec<u8>, String> {
        let AttachmentSource::Url { url } = &attachment.source else {
            return Err("This Teams attachment has no content URL.".to_string());
        };
        if is_bot_framework_host(url) {
            let token = self.access_token().await?;
            fetch_url(url, Some(&token), limits.max_bytes).await
        } else {
            fetch_url(url, None, limits.max_bytes).await
        }
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

/// The Bot Framework endpoint one reply is POSTed to.
///
/// Built with the URL parser rather than by formatting, so a conversation or
/// activity id is percent-encoded into exactly one path segment. Those ids come
/// from a provider payload: a `/` inside one, formatted straight into a path,
/// would move the request to an endpoint of the sender's choosing — with the
/// bot's own bearer token attached.
fn activity_url(
    service_url: &str,
    conversation_id: &str,
    reply_to_activity_id: Option<&str>,
) -> Result<String, String> {
    let mut url = reqwest::Url::parse(service_url)
        .map_err(|_| "The stored Teams serviceUrl is not a valid URL".to_string())?;
    {
        let mut segments = url
            .path_segments_mut()
            .map_err(|_| "The stored Teams serviceUrl cannot carry a path".to_string())?;
        // The stored value is a base like `https://smba.trafficmanager.net/amer/`,
        // whose trailing slash would otherwise leave an empty segment.
        segments.pop_if_empty();
        segments.extend(["v3", "conversations"]);
        segments.push(conversation_id);
        segments.push("activities");
        if let Some(activity_id) = reply_to_activity_id {
            segments.push(activity_id);
        }
    }
    Ok(url.to_string())
}

/// `https` on a host the configured cloud owns.
///
/// The host list comes from [`TeamsEnvironment`] rather than from constants
/// here, so it can never describe a different cloud than the issuer and token
/// scope do. Matching is on a label boundary — `evil-botframework.com` must not
/// pass as `botframework.com`, and neither must `botframework.com.example`.
fn validate_service_url(candidate: &str, environment: TeamsEnvironment) -> Result<(), String> {
    let url =
        reqwest::Url::parse(candidate).map_err(|_| "serviceUrl is not a valid URL".to_string())?;
    // A loopback fixture stands in for the Bot Framework in this file's own
    // tests, which is the only way to drive the real send path without the
    // network. Compiled out of every shipped build, so the rule below is the
    // only one production has.
    #[cfg(test)]
    if url
        .host_str()
        .is_some_and(|host| host == "127.0.0.1" || host == "localhost")
    {
        return Ok(());
    }
    if url.scheme() != "https" {
        return Err("serviceUrl must be https".to_string());
    }
    let host = url
        .host_str()
        .ok_or_else(|| "serviceUrl has no host".to_string())?
        .to_ascii_lowercase();
    let allowed = environment
        .allowed_service_url_hosts
        .iter()
        .any(|suffix| host == *suffix || host.ends_with(&format!(".{suffix}")));
    if !allowed {
        return Err("serviceUrl is not on a host this Bot Framework cloud owns".to_string());
    }
    Ok(())
}

fn validate_claims_structurally(
    claims: &JsonValue,
    expected_app_id: &str,
    expected_issuer: &str,
    now_ms: i64,
) -> Result<(), String> {
    let issuer = claims
        .get("iss")
        .and_then(JsonValue::as_str)
        .ok_or_else(|| "JWT is missing iss".to_string())?;
    if issuer != expected_issuer {
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
    environment: TeamsEnvironment,
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
                        stored_artifact_id: None,
                        text_excerpt: None,
                        fetch_error: None,
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
        if validate_service_url(service_url, environment).is_ok() {
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
pub(crate) mod tests {
    use super::*;
    use crate::daemon::channel_adapter::MemoryConversationReferences;
    use crate::daemon::channel_store::ChannelAccountRecord;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use base64::Engine as _;
    use little_monkey_lib::channels::policy::ChannelAccessPolicy;
    use little_monkey_lib::channels::types::{ConversationKind, HealthState};
    use std::io::{Read, Write};

    pub(crate) fn test_account() -> ChannelAccountRecord {
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
        // Never the daemon-backed reference store: a unit test must not reach
        // for the operator's real state database to find a reply address.
        TeamsAdapter::new(&config)
            .expect("adapter builds")
            .with_references(std::sync::Arc::new(MemoryConversationReferences::default()))
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

    /// The `serviceUrl` every activity fixture in these tests carries, and so
    /// the one a genuine token for them names.
    pub(crate) const TEST_SERVICE_URL: &str = "https://smba.trafficmanager.net/amer/";

    pub(crate) fn valid_claims(now_secs: i64) -> JsonValue {
        claims_for_service_url(now_secs, TEST_SERVICE_URL)
    }

    /// A token that is valid on any clock a test in this tree runs on.
    ///
    /// The production route reads the real system clock, while the fixtures
    /// around it are pinned to a fixed instant, so a token minted relative to
    /// either one is refused by the other. Expiry and not-yet-valid have their
    /// own tests above; this fixture is for the paths where the clock is not
    /// what is under test.
    pub(crate) fn long_lived_claims(service_url: &str) -> JsonValue {
        serde_json::json!({
            "iss": TeamsEnvironment::PUBLIC.expected_issuer,
            "aud": "app-id-1",
            "serviceurl": service_url,
            "exp": 4_000_000_000i64,
            "nbf": 1_500_000_000i64,
        })
    }

    /// The same claims with a chosen `serviceurl`, so a test can drive the
    /// binding between the token and the activity from both sides.
    pub(crate) fn claims_for_service_url(now_secs: i64, service_url: &str) -> JsonValue {
        serde_json::json!({
            "iss": TeamsEnvironment::PUBLIC.expected_issuer,
            "aud": "app-id-1",
            "serviceurl": service_url,
            "exp": now_secs + 3600,
            "nbf": now_secs - 60,
        })
    }

    // --- Structural checks + mandatory refusal -----------------------------

    /// A 2048-bit RSA test key generated locally for these tests only, whose
    /// public half is seeded into the JWKS cache as the "genuine" signer.
    /// Not used anywhere else and grants access to nothing.
    pub(crate) const TEST_PRIVATE_KEY_PEM: &str = "-----BEGIN PRIVATE KEY-----
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
    pub(crate) const TEST_WRONG_PRIVATE_KEY_PEM: &str = "-----BEGIN PRIVATE KEY-----
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

    pub(crate) fn test_jwk() -> JwkRsaKey {
        test_jwk_endorsing(&["msteams"])
    }

    /// The `kid` this file's test key is published under for the tests that
    /// drive the production HTTP route, which builds its own adapter and so
    /// cannot be handed a seeded cache.
    pub(crate) const ROUTE_KID: &str = "teams-route-key";

    /// Publish the test key as the Bot Framework's own JWKS would, so every
    /// cache built from here on can verify a token signed with it.
    pub(crate) fn publish_route_jwk() {
        super::super::jwt::test_keys::publish(ROUTE_KID, test_jwk());
    }

    /// The same key published with a chosen endorsement list, so a test can
    /// drive the channel check both ways.
    pub(crate) fn test_jwk_endorsing(channels: &[&str]) -> JwkRsaKey {
        JwkRsaKey {
            n: URL_SAFE_NO_PAD.decode(TEST_JWK_N).unwrap(),
            e: URL_SAFE_NO_PAD.decode(TEST_JWK_E).unwrap(),
            endorsements: channels.iter().map(|value| value.to_string()).collect(),
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
    pub(crate) fn sign_test_jwt(claims: &JsonValue, kid: &str, private_key_pem: &str) -> String {
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

    /// What the webhook acceptance path does with the addressing a verified
    /// delivery established: drain it and commit it. Here the store is the
    /// in-memory one, because what these tests are about is the *value* the
    /// verifier produced — `channel_webhook_tests` proves the real path
    /// commits it to the real database before the provider is answered.
    fn commit_addressing(adapter: &TeamsAdapter, references: &dyn ConversationReferences) {
        for entry in
            crate::daemon::channel_adapter::WebhookChannelAdapter::take_durable_addressing(adapter)
        {
            references
                .put(&entry.account_id, &entry.conversation_id, &entry.reference)
                .expect("store the address");
        }
    }

    #[test]
    fn a_verified_activity_records_its_service_url_for_send() {
        let references = std::sync::Arc::new(MemoryConversationReferences::default());
        let adapter = adapter().with_references(references.clone());
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
        commit_addressing(&adapter, references.as_ref());
        let stored = references
            .get("acct-teams", "19:conv1")
            .expect("the verified activity's address is durable");
        assert_eq!(
            stored.get("service_url").and_then(JsonValue::as_str),
            Some("https://smba.trafficmanager.net/amer/")
        );
        // Addressing, and nothing that authorizes anything.
        assert_eq!(
            stored.get("bot_id").and_then(JsonValue::as_str),
            Some("28:bot-id")
        );
        assert_eq!(
            stored.get("conversation_type").and_then(JsonValue::as_str),
            Some("personal")
        );
        assert!(
            stored.get("last_updated_at_ms").is_some(),
            "the operator can see how fresh an address is"
        );
        let serialized = stored.to_string();
        assert!(
            !serialized.contains("Bearer") && !serialized.contains("access_token"),
            "no token may be written to the reference table: {serialized}"
        );
    }

    /// A second adapter over the same store, as a restart produces: the
    /// process that received the activity is gone and this one has to answer.
    #[tokio::test]
    async fn a_reply_is_addressed_from_the_store_after_the_receiving_adapter_is_gone() {
        let references = std::sync::Arc::new(MemoryConversationReferences::default());
        let now_ms = 1_700_000_000_000i64;
        {
            let receiver = adapter().with_references(references.clone());
            receiver.jwks_cache.seed_for_test("test-key-1", test_jwk());
            let jwt = sign_test_jwt(
                &valid_claims(now_ms / 1000),
                "test-key-1",
                TEST_PRIVATE_KEY_PEM,
            );
            receiver
                .verify_and_normalize(
                    &[("authorization".to_string(), format!("Bearer {jwt}"))],
                    &serde_json::to_vec(&personal_activity()).unwrap(),
                    None,
                    now_ms,
                )
                .expect("verification should succeed");
            commit_addressing(&receiver, references.as_ref());
        }

        let token_base = serve_forever("200 OK", r#"{"access_token":"tok","expires_in":3600}"#);
        let send_base = serve_once("200 OK", r#"{"id":"activity-out-1"}"#);
        // The sender is told nothing about the conversation beyond its id, and
        // its own reference store starts empty of anything it learned itself.
        let sender = adapter()
            .with_login_base(&token_base)
            .with_references(references.clone());
        // Rewrite only the endpoint the fixture listens on, keeping every
        // other field the verified activity produced.
        let mut stored = references.get("acct-teams", "19:conv1").expect("stored");
        stored["service_url"] = JsonValue::from(send_base);
        references.put("acct-teams", "19:conv1", &stored).unwrap();

        let outcome = sender.send(&outbound_message()).await;
        assert!(
            matches!(outcome, SendOutcome::Sent { .. }),
            "a restart must not strand a durable reply: {outcome:?}"
        );
    }

    /// The claim that binds a token to the body it arrived with. Without it,
    /// any valid token for this bot could be replayed alongside a body naming
    /// a different endpoint — and that endpoint is where the bot's own bearer
    /// token would then be POSTed.
    #[test]
    fn a_token_issued_for_another_service_url_cannot_carry_this_activity() {
        let references = std::sync::Arc::new(MemoryConversationReferences::default());
        let adapter = adapter().with_references(references.clone());
        adapter.jwks_cache.seed_for_test("test-key-1", test_jwk());
        let now_ms = 1_700_000_000_000i64;
        let jwt = sign_test_jwt(
            &claims_for_service_url(now_ms / 1000, "https://smba.trafficmanager.net/emea/"),
            "test-key-1",
            TEST_PRIVATE_KEY_PEM,
        );

        let result = adapter.verify_and_normalize(
            &[("authorization".to_string(), format!("Bearer {jwt}"))],
            &serde_json::to_vec(&personal_activity()).unwrap(),
            None,
            now_ms,
        );

        assert!(
            result.unwrap_err().contains("serviceUrl"),
            "a token for one endpoint must not vouch for another"
        );
        commit_addressing(&adapter, references.as_ref());
        assert!(
            references.get("acct-teams", "19:conv1").is_none(),
            "a mismatched token planted a reply address"
        );
    }

    /// A trailing slash is not a difference: Microsoft's own payloads are not
    /// consistent about one, and refusing over it would refuse real traffic.
    #[test]
    fn a_trailing_slash_is_not_a_service_url_mismatch() {
        let adapter = adapter();
        adapter.jwks_cache.seed_for_test("test-key-1", test_jwk());
        let now_ms = 1_700_000_000_000i64;
        let jwt = sign_test_jwt(
            &claims_for_service_url(now_ms / 1000, "https://smba.trafficmanager.net/amer"),
            "test-key-1",
            TEST_PRIVATE_KEY_PEM,
        );

        adapter
            .verify_and_normalize(
                &[("authorization".to_string(), format!("Bearer {jwt}"))],
                &serde_json::to_vec(&personal_activity()).unwrap(),
                None,
                now_ms,
            )
            .expect("the same endpoint, spelled with and without its slash");
    }

    /// A token with no `serviceurl` claim binds to nothing, so there is
    /// nothing to check the body against and the delivery is refused.
    #[test]
    fn a_token_that_binds_to_no_service_url_is_refused() {
        let adapter = adapter();
        adapter.jwks_cache.seed_for_test("test-key-1", test_jwk());
        let now_ms = 1_700_000_000_000i64;
        let mut claims = valid_claims(now_ms / 1000);
        claims.as_object_mut().unwrap().remove("serviceurl");
        let jwt = sign_test_jwt(&claims, "test-key-1", TEST_PRIVATE_KEY_PEM);

        let result = adapter.verify_and_normalize(
            &[("authorization".to_string(), format!("Bearer {jwt}"))],
            &serde_json::to_vec(&personal_activity()).unwrap(),
            None,
            now_ms,
        );
        assert!(result.unwrap_err().contains("serviceurl"));
    }

    /// The Bot Framework says, per signing key, which channels that key may
    /// sign for. A key endorsed for one channel signing an activity claiming
    /// another is the case this refuses.
    #[test]
    fn an_activity_from_a_channel_the_signing_key_is_not_endorsed_for_is_refused() {
        let adapter = adapter();
        adapter.seed_jwks_endorsing_for_test(&["skype"]);
        let now_ms = 1_700_000_000_000i64;
        let jwt = sign_test_jwt(
            &valid_claims(now_ms / 1000),
            "test-key-1",
            TEST_PRIVATE_KEY_PEM,
        );

        let result = adapter.verify_and_normalize(
            &[("authorization".to_string(), format!("Bearer {jwt}"))],
            &serde_json::to_vec(&personal_activity()).unwrap(),
            None,
            now_ms,
        );
        assert!(
            result.unwrap_err().contains("endorsed"),
            "an unendorsed channel must not be accepted"
        );
    }

    /// An empty endorsement list is an absent claim, not a wildcard. Accepting
    /// one would let any Bot Framework signing key sign for Teams, which is
    /// exactly what the endorsement exists to stop.
    #[test]
    fn a_key_publishing_no_endorsements_cannot_sign_for_teams() {
        let adapter = adapter();
        adapter.seed_jwks_endorsing_for_test(&[]);
        let now_ms = 1_700_000_000_000i64;
        let jwt = sign_test_jwt(
            &valid_claims(now_ms / 1000),
            "test-key-1",
            TEST_PRIVATE_KEY_PEM,
        );

        let result = adapter.verify_and_normalize(
            &[("authorization".to_string(), format!("Bearer {jwt}"))],
            &serde_json::to_vec(&personal_activity()).unwrap(),
            None,
            now_ms,
        );
        assert!(
            result.unwrap_err().contains("endorsed"),
            "a key claiming no channel cannot claim this one"
        );
    }

    /// This is the Teams adapter. An activity from another Bot Framework
    /// channel the same bot is connected to — Direct Line, Web Chat, the
    /// emulator — is a different product arriving at this door.
    #[test]
    fn an_activity_from_another_bot_framework_channel_is_refused() {
        for channel_id in ["directline", "webchat", "emulator", "skype"] {
            let adapter = adapter();
            // Endorsed for both, so what refuses the activity is its own
            // channel identity rather than the key's endorsements.
            adapter.seed_jwks_endorsing_for_test(&["msteams", channel_id]);
            let now_ms = 1_700_000_000_000i64;
            let jwt = sign_test_jwt(
                &valid_claims(now_ms / 1000),
                "test-key-1",
                TEST_PRIVATE_KEY_PEM,
            );
            let mut activity = personal_activity();
            activity["channelId"] = JsonValue::from(channel_id);

            let result = adapter.verify_and_normalize(
                &[("authorization".to_string(), format!("Bearer {jwt}"))],
                &serde_json::to_vec(&activity).unwrap(),
                None,
                now_ms,
            );
            assert!(
                result
                    .unwrap_err()
                    .contains("not from the Microsoft Teams channel"),
                "{channel_id} is not Teams"
            );
        }
    }

    /// An activity naming no channel at all cannot be checked, and is refused
    /// rather than assumed to be Teams.
    #[test]
    fn an_activity_naming_no_channel_is_refused() {
        let adapter = adapter();
        adapter.seed_jwks_for_test();
        let now_ms = 1_700_000_000_000i64;
        let jwt = sign_test_jwt(
            &valid_claims(now_ms / 1000),
            "test-key-1",
            TEST_PRIVATE_KEY_PEM,
        );
        let mut activity = personal_activity();
        activity.as_object_mut().unwrap().remove("channelId");

        let result = adapter.verify_and_normalize(
            &[("authorization".to_string(), format!("Bearer {jwt}"))],
            &serde_json::to_vec(&activity).unwrap(),
            None,
            now_ms,
        );
        assert!(result.unwrap_err().contains("channelId"));
    }

    /// The endorsement list is what the JWKS document publishes, so it has to
    /// survive parsing — a key representation that dropped it would make the
    /// check above unimplementable rather than merely unused.
    #[test]
    fn endorsements_survive_the_jwks_parser() {
        let document = serde_json::json!({
            "keys": [{
                "kty": "RSA",
                "kid": "test-key-1",
                "n": TEST_JWK_N,
                "e": TEST_JWK_E,
                "endorsements": ["msteams", "skype"],
            }]
        })
        .to_string();
        let keys = super::super::jwt::parse_jwks_body(document.as_bytes()).expect("parses");
        assert_eq!(keys[0].1.endorsements, vec!["msteams", "skype"]);
    }

    /// The five values that define a cloud move together or not at all.
    #[test]
    fn one_environment_decides_both_directions() {
        let environment = TeamsEnvironment::PUBLIC;
        assert!(environment.expected_issuer.contains("botframework.com"));
        assert!(environment
            .token_scope
            .starts_with(environment.expected_issuer));
        assert!(environment
            .openid_metadata_url
            .starts_with("https://login.botframework.com/"));
        assert!(environment.oauth_authority.starts_with("https://login."));
        assert!(environment
            .allowed_service_url_hosts
            .contains(&"trafficmanager.net"));
        // And a host from no cloud at all is refused by the same list.
        assert!(validate_service_url("https://smba.example.com/amer/", environment).is_err());
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
            "serviceUrl": TEST_SERVICE_URL,
            "channelId": "msteams",
            "conversation": {"id": "19:conv1", "conversationType": "personal"},
            "from": {"id": "29:user1", "name": "Ada"},
            "recipient": {"id": "28:bot-id"},
            "text": "hello bot"
        })
    }

    #[test]
    fn a_personal_conversation_normalizes_to_direct() {
        let envelope = normalize_activity(
            &personal_activity(),
            "acct-teams",
            "28:bot-id",
            TeamsEnvironment::PUBLIC,
            0,
        )
        .unwrap();
        assert_eq!(envelope.conversation.kind, ConversationKind::Direct);
        assert_eq!(envelope.provider_event_id, "activity-1");
        assert_eq!(envelope.text, "hello bot");
    }

    #[test]
    fn a_channel_conversation_normalizes_to_group() {
        let mut activity = personal_activity();
        activity["conversation"]["conversationType"] = serde_json::json!("channel");
        let envelope = normalize_activity(
            &activity,
            "acct-teams",
            "28:bot-id",
            TeamsEnvironment::PUBLIC,
            0,
        )
        .unwrap();
        assert_eq!(envelope.conversation.kind, ConversationKind::Group);
    }

    #[test]
    fn a_group_chat_conversation_normalizes_to_group() {
        let mut activity = personal_activity();
        activity["conversation"]["conversationType"] = serde_json::json!("groupChat");
        let envelope = normalize_activity(
            &activity,
            "acct-teams",
            "28:bot-id",
            TeamsEnvironment::PUBLIC,
            0,
        )
        .unwrap();
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
        let envelope = normalize_activity(
            &activity,
            "acct-teams",
            "28:bot-id",
            TeamsEnvironment::PUBLIC,
            0,
        )
        .unwrap();
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
        let envelope = normalize_activity(
            &activity,
            "acct-teams",
            "28:bot-id",
            TeamsEnvironment::PUBLIC,
            0,
        )
        .unwrap();
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
        let envelope = normalize_activity(
            &activity,
            "acct-teams",
            "28:bot-id",
            TeamsEnvironment::PUBLIC,
            0,
        )
        .unwrap();
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
        assert!(normalize_activity(
            &activity,
            "acct-teams",
            "28:bot-id",
            TeamsEnvironment::PUBLIC,
            0
        )
        .is_none());
    }

    #[test]
    fn provider_event_ids_are_deterministic() {
        let first = normalize_activity(
            &personal_activity(),
            "acct-teams",
            "28:bot-id",
            TeamsEnvironment::PUBLIC,
            0,
        )
        .unwrap();
        let second = normalize_activity(
            &personal_activity(),
            "acct-teams",
            "28:bot-id",
            TeamsEnvironment::PUBLIC,
            0,
        )
        .unwrap();
        assert_eq!(first.provider_event_id, second.provider_event_id);
    }

    // --- serviceUrl validation ---------------------------------------------

    #[test]
    fn a_microsoft_https_service_url_validates() {
        assert!(validate_service_url(
            "https://smba.trafficmanager.net/amer/",
            TeamsEnvironment::PUBLIC
        )
        .is_ok());
        assert!(
            validate_service_url("https://api.botframework.com/", TeamsEnvironment::PUBLIC).is_ok()
        );
    }

    #[test]
    fn a_conversation_id_cannot_walk_the_reply_out_of_its_own_endpoint() {
        // Ids arrive in a provider payload. One containing a path separator,
        // formatted straight into a URL, would move the POST — and the bot's
        // bearer token with it — somewhere the sender chose.
        let url = activity_url(
            "https://smba.trafficmanager.net/amer",
            "19:conv1/../../../v3/conversations/19:victim",
            None,
        )
        .expect("a hostile id still produces a URL, just not that one");
        assert!(
            url.starts_with("https://smba.trafficmanager.net/amer/v3/conversations/"),
            "{url}"
        );
        assert!(
            !url.contains("/../"),
            "the id escaped its own path segment: {url}"
        );
        // The ordinary shape, with the base's trailing slash: no empty segment.
        let url = activity_url("https://smba.trafficmanager.net/amer/", "19:conv1", None).unwrap();
        assert_eq!(
            url,
            "https://smba.trafficmanager.net/amer/v3/conversations/19:conv1/activities"
        );
    }

    #[test]
    fn a_non_https_service_url_is_refused() {
        assert!(validate_service_url(
            "http://smba.trafficmanager.net/amer/",
            TeamsEnvironment::PUBLIC
        )
        .is_err());
    }

    #[test]
    fn a_non_microsoft_host_is_refused() {
        assert!(
            validate_service_url("https://evil.example.com/amer/", TeamsEnvironment::PUBLIC)
                .is_err()
        );
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

    /// A reference store already holding one conversation addressed at a
    /// loopback fixture, which is what an inbound activity would have written.
    fn references_at(service_url: &str) -> std::sync::Arc<MemoryConversationReferences> {
        let references = std::sync::Arc::new(MemoryConversationReferences::default());
        references
            .put(
                "acct-teams",
                "19:conv1",
                &serde_json::json!({
                    "service_url": service_url,
                    "bot_id": "28:bot",
                    "last_updated_at_ms": 1_700_000_000_000i64,
                }),
            )
            .expect("seed reference");
        references
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
        let adapter = adapter()
            .with_login_base(&token_base)
            .with_references(references_at(&send_base));
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
        let adapter = adapter()
            .with_login_base(&token_base)
            .with_references(references_at(&send_base));
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
        let adapter = adapter()
            .with_login_base(&token_base)
            .with_references(references_at(&send_base));
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
