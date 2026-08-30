//! Generic MCP-spec OAuth 2.0 support for `mcp.rs`'s `McpTransport::Http`
//! servers — RFC 8414 authorization server metadata discovery, RFC 7591
//! dynamic client registration (falling back to a caller-supplied client id
//! when a server doesn't support DCR), and a PKCE authorization-code flow,
//! all built on `rmcp`'s `transport::auth` module (the `auth` Cargo feature
//! enabled in `Cargo.toml`).
//!
//! This is an ADDITIONAL, alternative way to obtain a bearer token for an
//! HTTP MCP server — it does not replace `mcp.rs`'s manual
//! `mcp_set_http_token` path (for servers that hand out a static token
//! outside any OAuth dance). See `mcp.rs`'s module doc and `connect_impl`'s
//! `Http` branch for how the two combine at connect time.
//!
//! Flow, driven by [`mcp_oauth_connect`]:
//! 1. `AuthorizationManager::new(base_url)` — the MCP server's own HTTP(S)
//!    URL doubles as the OAuth resource/issuer base for discovery.
//! 2. `discover_metadata()` (RFC 8414, with an RFC 9728 protected-resource
//!    metadata attempt first, and a same-origin-derived legacy fallback if
//!    the server advertises neither — all handled inside `rmcp` itself).
//! 3. [`LoopbackListener::bind_for`] — reserves the server's stable loopback
//!    port for the redirect URI before starting the flow, so it can be
//!    registered (DCR) or repeated back with the client-id fallback.
//! 4. [`prepare_authorization`] — dynamic client registration at that
//!    redirect URI, or (if the server doesn't support DCR)
//!    `client_id_override` configured against it instead; then builds the
//!    PKCE + CSRF authorization URL.
//! 5. The system browser is opened on that URL; the loopback listener
//!    awaits the single resulting redirect (bounded by
//!    [`LOOPBACK_TIMEOUT_SECS`]).
//! 6. `AuthorizationManager::exchange_code_for_token_with_issuer` — verifies
//!    the CSRF token and optional RFC 9207 issuer against the (in-memory,
//!    single-attempt-scoped) `StateStore`, exchanges the code, and persists the result via
//!    [`KeychainCredentialStore`] (OS keychain, never a JSON file).
//!
//! Progress streams to the frontend via `mcp-oauth://status` events (mirrors
//! `mcp.rs`'s `mcp://status`), since steps 4-6 can take minutes — the user
//! has to actually complete browser consent.
//!
//! Because this app ships as a public open-source binary, it holds no OAuth
//! client credentials of its own. How it identifies itself instead follows the
//! MCP authorization spec's priority order (2025-11-25, "Client Registration
//! Approaches"), which [`prepare_authorization`] implements:
//!
//! 1. **Pre-registered** — the user's own OAuth app, pasted once into Settings
//!    and kept in their keychain ([`ManualClientRegistration`]). Covers both
//!    halves such a registration can have: a public (PKCE-only) client id, and
//!    — for providers like Google, whose installed-app clients require it at the
//!    token endpoint — an accompanying `client_secret`.
//! 2. **CIMD** — a Client ID Metadata Document ([`CIMD_CLIENT_ID`]): the client
//!    id is an HTTPS URL the authorization server fetches. No registration, no
//!    secret, nothing for the user to do.
//! 3. **DCR** — RFC 7591 dynamic client registration.
//! 4. **Ask the user** — [`CLIENT_ID_REQUIRED_MESSAGE`], for a server that
//!    supports none of the above (Google and Slack, today).
//!
//! See `docs/byo-oauth-clients.md` for the user-facing version of all this.

use std::time::Duration;

use async_trait::async_trait;
use rmcp::transport::auth::{
    AuthError, AuthorizationManager, CredentialStore, OAuthClientConfig, StoredCredentials,
};
use tauri::Emitter;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

use crate::AppState;

/// Same keychain service every credential in this app lives under (see
/// `mcp.rs`/`providers.rs`) — entries are disambiguated by *account*, not
/// service, hence this module's own `mcp-oauth:<id>` account prefix.
/// Profile-scoped (K23). The default profile keeps this exact service name, so
/// every credential stored before profiles existed still resolves; any other
/// profile's secrets live under `<service>.profile.<id>`, which is a different
/// keychain item that this profile's code never names.
static KEYCHAIN_SERVICE: std::sync::LazyLock<String> =
    std::sync::LazyLock::new(|| crate::profiles::keychain_service("com.littlemonkey.app"));

/// Bound on how long [`mcp_oauth_connect`] waits for the browser to redirect
/// back to the loopback listener after being opened — the user has to
/// actually see the consent screen and click through it, unlike
/// `mcp::CONNECT_TIMEOUT_SECS`'s much shorter handshake bound.
const LOOPBACK_TIMEOUT_SECS: u64 = 300;

/// Distinctive error message returned (and streamed as the `"needs_client_id"`
/// progress phase) when a server's authorization server doesn't support
/// dynamic client registration and the caller didn't supply a `client_id` —
/// the frontend matches on the phase (not this string) to show a "bring your
/// own OAuth app" input, mirroring the same fallback every provider's OAuth
/// setup already requires when DCR isn't available.
pub const CLIENT_ID_REQUIRED_MESSAGE: &str = "This MCP server does not support automatic OAuth client registration. Enter the client id from your own OAuth app registration and try again.";
const OAUTH_CANCELLED_MESSAGE: &str = "OAuth connection cancelled";

/// The keychain *account* name under which server `id`'s OAuth-derived
/// credentials (refresh token, last access token, granted scopes, client id)
/// are stored — `mcp-oauth:<id>`, distinct from `mcp.rs`'s own `mcp:<id>`
/// manual-bearer-token accounts in the same keychain service.
fn keychain_account(server_id: &str) -> String {
    format!("mcp-oauth:{}", server_id)
}

/// The keychain *account* holding server `id`'s user-supplied OAuth *client*
/// registration — `mcp-oauth-client:<id>`, a third namespace alongside
/// [`keychain_account`]'s `mcp-oauth:<id>` credentials and `mcp.rs`'s
/// `mcp:<id>` manual bearer tokens.
fn manual_client_keychain_account(server_id: &str) -> String {
    format!("mcp-oauth-client:{}", server_id)
}

fn oauth_credentials_entry(server_id: &str) -> Result<keyring::Entry, String> {
    keyring::Entry::new(&KEYCHAIN_SERVICE, &keychain_account(server_id))
        .map_err(|e| format!("Failed to access keychain: {e}"))
}

fn load_oauth_credentials_record(server_id: &str) -> Result<Option<String>, String> {
    match oauth_credentials_entry(server_id)?.get_password() {
        Ok(json) => Ok(Some(json)),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(e) => Err(format!(
            "Failed to read OAuth credentials from keychain: {e}"
        )),
    }
}

fn save_oauth_credentials_record(server_id: &str, record: &str) -> Result<(), String> {
    oauth_credentials_entry(server_id)?
        .set_password(record)
        .map_err(|e| format!("Failed to save OAuth credentials to keychain: {e}"))
}

fn remove_oauth_credentials_record(server_id: &str) -> Result<(), String> {
    match oauth_credentials_entry(server_id)?.delete_credential() {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(e) => Err(format!("Failed to remove saved OAuth credentials: {e}")),
    }
}

/// A "bring your own OAuth app" client registration for one server id: what
/// the user pasted into Settings after registering an OAuth client with the
/// provider themselves, for the (common) case where the authorization server
/// doesn't support RFC 7591 dynamic client registration. This app can't ship
/// its own client credentials — it's a public open-source binary, so anything
/// baked in would be extractable by anyone who downloads it.
///
/// Persisted rather than kept in memory for the duration of one connect,
/// because it is needed again on every later token *refresh*: `rmcp`'s
/// `StoredCredentials` carries only a `client_id`, and its
/// `initialize_from_store` reconfigures the OAuth client from that alone with
/// no secret — so a provider that requires `client_secret` at the token
/// endpoint (Google's installed-app clients do, even though that secret is
/// explicitly not treated as confidential there) would reject every refresh
/// once the first access token expired. Also what makes a later reconnect
/// one click instead of another paste.
#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct ManualClientRegistration {
    pub client_id: String,
    /// `None` for a public client (pure PKCE, no secret) — the shape RFC 8252
    /// recommends for native apps, and what most MCP-native providers issue.
    pub client_secret: Option<String>,
}

fn manual_client_entry(server_id: &str) -> Result<keyring::Entry, String> {
    keyring::Entry::new(
        &KEYCHAIN_SERVICE,
        &manual_client_keychain_account(server_id),
    )
    .map_err(|e| format!("Failed to access keychain: {e}"))
}

/// Reads server `id`'s saved OAuth client registration, if any. Absence is
/// normal (a server whose authorization server supports DCR never needs one).
fn load_manual_client_record(server_id: &str) -> Result<Option<String>, String> {
    match manual_client_entry(server_id)?.get_password() {
        Ok(json) => Ok(Some(json)),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(e) => Err(format!(
            "Failed to read OAuth client registration from keychain: {e}"
        )),
    }
}

fn load_manual_client(server_id: &str) -> Result<Option<ManualClientRegistration>, String> {
    load_manual_client_record(server_id)?
        .map(|json| {
            serde_json::from_str(&json)
                .map_err(|e| format!("Corrupt stored OAuth client registration: {e}"))
        })
        .transpose()
}

fn save_manual_client_record(server_id: &str, json: &str) -> Result<(), String> {
    manual_client_entry(server_id)?
        .set_password(json)
        .map_err(|e| format!("Failed to save OAuth client registration: {e}"))
}

#[cfg(test)]
fn save_manual_client(
    server_id: &str,
    registration: &ManualClientRegistration,
) -> Result<(), String> {
    let json = serde_json::to_string(registration)
        .map_err(|e| format!("Failed to serialize OAuth client registration: {e}"))?;
    save_manual_client_record(server_id, &json)
}

/// Clears server `id`'s saved OAuth client registration. A no-op success when
/// none was saved.
fn remove_manual_client(server_id: &str) -> Result<(), String> {
    match manual_client_entry(server_id)?.delete_credential() {
        Ok(()) => Ok(()),
        Err(keyring::Error::NoEntry) => Ok(()),
        Err(e) => Err(format!(
            "Failed to remove saved OAuth client registration: {e}"
        )),
    }
}

/// The two durable records that one OAuth exchange stages as a transaction.
///
/// The production implementation remains the OS keychain. Keeping this small
/// boundary private lets the transaction itself be proven without depending on
/// a platform credential manager's cross-thread ordering guarantees.
trait OAuthRecordStore: Send + Sync {
    fn load_credentials(&self, server_id: &str) -> Result<Option<String>, String>;
    fn save_credentials(&self, server_id: &str, record: &str) -> Result<(), String>;
    fn remove_credentials(&self, server_id: &str) -> Result<(), String>;
    fn load_manual_client(&self, server_id: &str) -> Result<Option<String>, String>;
    fn save_manual_client(&self, server_id: &str, record: &str) -> Result<(), String>;
    fn remove_manual_client(&self, server_id: &str) -> Result<(), String>;
}

struct KeychainOAuthRecordStore;

impl OAuthRecordStore for KeychainOAuthRecordStore {
    fn load_credentials(&self, server_id: &str) -> Result<Option<String>, String> {
        load_oauth_credentials_record(server_id)
    }

    fn save_credentials(&self, server_id: &str, record: &str) -> Result<(), String> {
        save_oauth_credentials_record(server_id, record)
    }

    fn remove_credentials(&self, server_id: &str) -> Result<(), String> {
        remove_oauth_credentials_record(server_id)
    }

    fn load_manual_client(&self, server_id: &str) -> Result<Option<String>, String> {
        load_manual_client_record(server_id)
    }

    fn save_manual_client(&self, server_id: &str, record: &str) -> Result<(), String> {
        save_manual_client_record(server_id, record)
    }

    fn remove_manual_client(&self, server_id: &str) -> Result<(), String> {
        remove_manual_client(server_id)
    }
}

/// Removes two related records as one recoverable operation. The first record
/// is snapshotted before either deletion; if deleting the second record fails,
/// the first is restored before the error is returned. This keeps a failed
/// disconnect from stranding a still-present token without the client
/// registration it needs for refresh.
///
/// The operations are injected so the rollback contract can be tested without
/// teaching production keychain code about a test-only backend.
fn remove_pair_with_rollback<T, LoadFirst, RemoveFirst, RemoveSecond, RestoreFirst>(
    load_first: LoadFirst,
    remove_first: RemoveFirst,
    remove_second: RemoveSecond,
    restore_first: RestoreFirst,
) -> Result<(), String>
where
    LoadFirst: FnOnce() -> Result<Option<T>, String>,
    RemoveFirst: FnOnce() -> Result<(), String>,
    RemoveSecond: FnOnce() -> Result<(), String>,
    RestoreFirst: FnOnce(&T) -> Result<(), String>,
{
    let previous_first = load_first()?;
    remove_first()?;

    if let Err(second_error) = remove_second() {
        if let Some(previous_first) = previous_first.as_ref() {
            if let Err(restore_error) = restore_first(previous_first) {
                return Err(format!(
                    "{second_error}; additionally failed to restore the first OAuth record: {restore_error}"
                ));
            }
        }
        return Err(second_error);
    }

    Ok(())
}

/// `rmcp::transport::auth::CredentialStore` backed by the OS keychain —
/// never a JSON file on disk, unlike `mcp_servers.json`/`connectors.json`,
/// since these credentials (a refresh token, in particular) are exactly the
/// kind of secret this app already keeps out of plain files. Serializes
/// `StoredCredentials` (which is itself just `#[derive(Serialize,
/// Deserialize)]`) to a JSON string for the single keychain "password"
/// value, mirroring how `m4_runtime.rs`/`portability_commands.rs` already
/// store structured secrets as a serialized blob in one keychain entry.
#[derive(Clone)]
pub struct KeychainCredentialStore {
    server_id: String,
}

impl KeychainCredentialStore {
    pub fn new(server_id: impl Into<String>) -> Self {
        Self {
            server_id: server_id.into(),
        }
    }

    fn entry(&self) -> Result<keyring::Entry, AuthError> {
        keyring::Entry::new(&KEYCHAIN_SERVICE, &keychain_account(&self.server_id))
            .map_err(|e| AuthError::InternalError(format!("Failed to access keychain: {e}")))
    }
}

#[async_trait]
impl CredentialStore for KeychainCredentialStore {
    async fn load(&self) -> Result<Option<StoredCredentials>, AuthError> {
        let entry = self.entry()?;
        match entry.get_password() {
            Ok(json) => serde_json::from_str(&json).map(Some).map_err(|e| {
                AuthError::InternalError(format!("Corrupt stored OAuth credentials: {e}"))
            }),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(AuthError::InternalError(format!(
                "Failed to read OAuth credentials from keychain: {e}"
            ))),
        }
    }

    async fn save(&self, credentials: StoredCredentials) -> Result<(), AuthError> {
        let entry = self.entry()?;
        let json = serde_json::to_string(&credentials).map_err(|e| {
            AuthError::InternalError(format!("Failed to serialize OAuth credentials: {e}"))
        })?;
        entry.set_password(&json).map_err(|e| {
            AuthError::InternalError(format!("Failed to save OAuth credentials to keychain: {e}"))
        })
    }

    async fn clear(&self) -> Result<(), AuthError> {
        let entry = self.entry()?;
        match entry.delete_credential() {
            Ok(()) => Ok(()),
            Err(keyring::Error::NoEntry) => Ok(()),
            Err(e) => Err(AuthError::InternalError(format!(
                "Failed to remove OAuth credentials from keychain: {e}"
            ))),
        }
    }
}

/// Whether server `id` currently has OAuth-derived credentials saved —
/// never the credentials themselves. Mirrors `mcp.rs::read_http_token`'s
/// "absence is normal, not an error" stance, collapsed to a bool for
/// `McpServerInfo::has_oauth`.
pub fn has_oauth_credentials(server_id: &str) -> bool {
    keyring::Entry::new(&KEYCHAIN_SERVICE, &keychain_account(server_id))
        .ok()
        .and_then(|e| e.get_password().ok())
        .is_some()
}

/// Core remove-credentials logic behind [`mcp_oauth_disconnect`], also
/// called (best-effort) by `mcp::mcp_remove_server` so deleting a server
/// never leaves an orphaned OAuth credential behind — same reasoning as
/// `mcp::remove_http_token_impl`. A missing entry is a no-op success.
pub(crate) fn remove_oauth_credentials_impl(server_id: &str) -> Result<(), String> {
    // The user's own client registration goes with them: disconnecting is the
    // one place where "forget everything about this server's OAuth" is the
    // intent (an expiring/failing *token* never reaches here — that path
    // refreshes or reconnects, reusing the saved registration).
    remove_pair_with_rollback(
        // Snapshot the opaque keychain value rather than deserializing it.
        // Even an older or corrupt record can therefore still be cleared and,
        // if the token deletion fails, restored byte-for-byte.
        || load_manual_client_record(server_id),
        || remove_manual_client(server_id),
        || {
            let entry = keyring::Entry::new(&KEYCHAIN_SERVICE, &keychain_account(server_id))
                .map_err(|e| format!("Failed to access keychain: {e}"))?;
            match entry.delete_credential() {
                Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
                Err(e) => Err(format!("Failed to remove saved OAuth credentials: {e}")),
            }
        },
        |record| save_manual_client_record(server_id, record),
    )
}

fn auth_err(e: AuthError) -> String {
    format!("OAuth error: {e}")
}

/// Returns (creating on first use) the per-`server_id` async lock that
/// serializes [`get_access_token_if_connected`]'s refresh-token exchange —
/// see `AppState::mcp_oauth_refresh_locks`'s doc comment for why this exists.
/// Only the map itself is guarded by the (synchronous, briefly-held)
/// `std::sync::Mutex`; the returned `Arc<tokio::sync::Mutex<()>>` is what
/// callers actually hold across the `.await`ing refresh call.
///
/// `pub(crate)` so `hosted_oauth.rs` can serialize its own refresh calls
/// through the same map — a server id only ever belongs to one OAuth flow
/// (this module's generic rmcp one, or the hosted-broker one), so sharing
/// the map is just reusing the "one lock per server id" bookkeeping rather
/// than standing up a second, identical map for the same concern.
pub(crate) fn refresh_lock_for(
    state: &AppState,
    server_id: &str,
) -> std::sync::Arc<tokio::sync::Mutex<()>> {
    let mut guard = state
        .mcp_oauth_refresh_locks
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    guard
        .entry(server_id.to_string())
        .or_insert_with(|| std::sync::Arc::new(tokio::sync::Mutex::new(())))
        .clone()
}

/// If server `id` has OAuth-derived credentials saved, returns a currently
/// valid access token for it (refreshing via the stored refresh token if the
/// cached one has expired) — `Ok(None)` if no OAuth credentials exist at all
/// for this id (the normal case for a server never connected via OAuth; the
/// caller should fall back to a manual bearer token, if any). An `Err` means
/// OAuth credentials DO exist but no usable token could be produced (e.g.
/// the refresh token was revoked) — re-authorization via
/// [`mcp_oauth_connect`] is required.
///
/// Rediscovers metadata on every call rather than caching an
/// `AuthorizationManager` across connects — simpler and consistent with
/// `rmcp`'s own `initialize_from_store` helper, at the cost of one extra
/// discovery round-trip per connect. Acceptable here since MCP connects are
/// not a hot path (`mcp::CONNECT_TIMEOUT_SECS` already budgets 30s for the
/// whole handshake).
///
/// Serializes concurrent calls for the same `server_id` behind
/// `AppState::mcp_oauth_refresh_locks` — two overlapping `mcp_connect` calls
/// for the same OAuth-connected server (e.g. a double-click on Reconnect, or
/// an auto-reconnect racing a manual one) would otherwise both read the same
/// still-current refresh token and race to redeem it, and an authorization
/// server that rotates refresh tokens on use would reject whichever request
/// arrives second — surfacing a false "authorization expired/revoked" error
/// for a connect attempt that was actually fine.
pub(crate) async fn get_access_token_if_connected(
    state: &AppState,
    server_id: &str,
    base_url: &str,
) -> Result<Option<String>, String> {
    if !has_oauth_credentials(server_id) {
        return Ok(None);
    }
    // Defensive, mirroring `resolve_http_base_url`'s check at the start of
    // the OAuth flow: a server's URL can be edited to plaintext HTTP *after*
    // OAuth credentials were saved against its original (secure) URL — never
    // send a refresh-token exchange (which, like the initial code exchange,
    // returns fresh credentials) over the network unencrypted just because
    // the config changed underneath already-saved credentials.
    ensure_oauth_base_url_is_secure(base_url)?;

    let lock = refresh_lock_for(state, server_id);
    let _guard = lock.lock().await;

    let mut manager = AuthorizationManager::new(base_url)
        .await
        .map_err(auth_err)?;
    manager.set_credential_store(KeychainCredentialStore::new(server_id.to_string()));

    let initialized = manager.initialize_from_store().await.map_err(auth_err)?;
    if !initialized {
        // Credentials were saved but carry no token response — treat like
        // "not OAuth-connected" rather than erroring the whole connect.
        return Ok(None);
    }

    // `initialize_from_store` rebuilt the OAuth client from the stored
    // `client_id` alone — `rmcp`'s `StoredCredentials` has nowhere to keep a
    // `client_secret`. Re-apply the user's own registration so a provider
    // that authenticates the client at the token endpoint (Google's
    // installed-app clients require `client_secret` even for the loopback
    // PKCE flow) can actually redeem the refresh token. `base_url` as the
    // redirect URI mirrors what `rmcp`'s own `configure_client_id` does here:
    // a refresh request sends no `redirect_uri`, so only its well-formedness
    // matters, and the real one belongs to a loopback listener that stopped
    // existing when the connect flow ended.
    if let Some(registration) = load_manual_client(server_id)? {
        if let Some(secret) = registration.client_secret {
            let config =
                OAuthClientConfig::new(registration.client_id, base_url).with_client_secret(secret);
            manager.configure_client(config).map_err(auth_err)?;
        }
    }

    let token = manager.get_access_token().await.map_err(|e| match e {
        AuthError::AuthorizationRequired => format!(
            "OAuth authorization for MCP server '{server_id}' has expired or was revoked — reconnect via OAuth in Settings."
        ),
        other => auth_err(other),
    })?;
    Ok(Some(token))
}

/// One retry, after a short backoff, on top of `AuthorizationManager::
/// register_client` — a single DCR attempt can fail for reasons that have
/// nothing to do with whether the server supports DCR at all (a request
/// timeout, a transient 5xx from the authorization server, a momentarily
/// malformed response, or any other HTTP-level hiccup), and
/// `rmcp::transport::auth::AuthError` collapses every one of those causes
/// into the same `RegistrationFailed` variant as "no `registration_endpoint`
/// advertised" — there's no reliable, error-shape-based way for
/// [`prepare_authorization`] to tell them apart (worse, when discovery finds
/// no real RFC 8414/9728 metadata at all, `rmcp` still *guesses* a
/// `registration_endpoint` for its legacy fallback, so even checking that
/// field doesn't disambiguate "genuinely no DCR" from "a real endpoint that
/// just blipped"). Retrying once filters out the transient case before
/// [`prepare_authorization`] falls back to asking the user for a manually
/// registered OAuth client id — a real registration outage/misconfiguration
/// still (correctly) ends up there after both attempts fail.
async fn register_client_with_retry(
    manager: &mut AuthorizationManager,
    redirect_uri: &str,
) -> Result<OAuthClientConfig, AuthError> {
    match manager
        .register_client("Little Monkey", redirect_uri, &[])
        .await
    {
        Ok(config) => Ok(config),
        Err(first_err) => {
            tokio::time::sleep(Duration::from_millis(300)).await;
            manager
                .register_client("Little Monkey", redirect_uri, &[])
                .await
                .map_err(|_second_err| first_err)
        }
    }
}

/// Authorize-endpoint query params that specific authorization servers require
/// before they will issue a *refresh* token at all. Standard OAuth has no such
/// knob — RFC 6749 leaves refresh-token issuance to the server, and the MCP
/// spec's own answer (SEP-2207's `offline_access` scope, which `rmcp` already
/// appends when advertised) doesn't apply to servers that don't advertise it.
/// Google is the one that matters here: without `access_type=offline` its token
/// response carries no refresh token, so an MCP server behind a Google account
/// would work for exactly one hour and then fail every tool call with
/// `Auth required` and no way to recover but a full re-consent.
/// `prompt=consent` goes with it, since Google only re-issues a refresh token
/// on a consent screen the user actually sees (a silent re-approval of an
/// already-granted client returns an access token alone).
///
/// Keyed by the *authorization endpoint's* host, so it can't be spoofed by an
/// MCP server's own URL: the endpoint comes from the discovered authorization
/// server metadata.
const EXTRA_AUTHORIZE_PARAMS_BY_HOST: &[(&str, &[(&str, &str)])] = &[(
    "accounts.google.com",
    &[("access_type", "offline"), ("prompt", "consent")],
)];

/// Applies [`EXTRA_AUTHORIZE_PARAMS_BY_HOST`] to a freshly built authorize URL,
/// never overwriting a param the authorization server (or `rmcp`) already put
/// there. An unparseable URL is returned untouched — the browser open that
/// follows will surface the problem far more clearly than a discarded param
/// would.
fn with_extra_authorize_params(authorize_url: String) -> String {
    let Ok(mut parsed) = url::Url::parse(&authorize_url) else {
        return authorize_url;
    };
    let Some(host) = parsed.host_str().map(str::to_ascii_lowercase) else {
        return authorize_url;
    };
    let Some((_, extra)) = EXTRA_AUTHORIZE_PARAMS_BY_HOST
        .iter()
        .find(|(known_host, _)| *known_host == host)
    else {
        return authorize_url;
    };

    let existing: Vec<String> = parsed
        .query_pairs()
        .map(|(key, _)| key.into_owned())
        .collect();
    let mut query = parsed.query_pairs_mut();
    for (key, value) in extra.iter() {
        if !existing.iter().any(|present| present == key) {
            query.append_pair(key, value);
        }
    }
    drop(query);
    parsed.to_string()
}

/// This app's OAuth Client ID Metadata Document URL, used verbatim as the
/// `client_id` for authorization servers that support CIMD
/// (draft-ietf-oauth-client-id-metadata-document-00, adopted by the MCP
/// authorization spec 2025-11-25).
///
/// The mechanism: instead of registering a client (DCR) or asking the user to
/// paste one, the client identifies itself with an HTTPS URL, and the
/// authorization server fetches that URL to learn the client's name and its
/// permitted redirect URIs. That is exactly the problem this app has — a public
/// binary with no client credentials of its own, talking to servers it has no
/// prior relationship with — and it's how Claude Code identifies itself too.
/// Nothing secret is involved: the document is public, static, and the client
/// authenticates as a public client (`token_endpoint_auth_method: "none"`).
///
/// The document lives in the website repo at `public/oauth/client-metadata.json`
/// and MUST keep declaring a loopback redirect URI compatible with what
/// [`LoopbackListener`] binds — an authorization server validates the redirect
/// URI in the authorize request against that list (waiving the port per RFC 8252
/// §7.3), so the two files are a contract. It also MUST keep its own `client_id`
/// field equal to this URL, or every CIMD server rejects the flow.
const CIMD_CLIENT_ID: &str = "https://getlittlemonkey.com/oauth/client-metadata.json";

/// Whether this authorization server accepts a CIMD `client_id`.
///
/// Both conditions come from the spec plus the practical constraint that this
/// app's document declares a *public* client: the server must advertise
/// `client_id_metadata_document_supported`, and it must accept `none` at the
/// token endpoint (RFC 8414's default is `client_secret_basic`, so an absent
/// list means "no public clients here" and a secret this app doesn't have would
/// be required). Same pair of checks Claude's own connector implementation
/// documents.
fn authorization_server_supports_cimd(
    metadata: &rmcp::transport::auth::AuthorizationMetadata,
) -> bool {
    let advertises_cimd = metadata
        .additional_fields
        .get("client_id_metadata_document_supported")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    let accepts_public_clients = metadata
        .additional_fields
        .get("token_endpoint_auth_methods_supported")
        .and_then(serde_json::Value::as_array)
        .is_some_and(|methods| methods.iter().any(|method| method.as_str() == Some("none")));
    advertises_cimd && accepts_public_clients
}

/// Scopes to request for specific MCP servers, instead of everything their
/// RFC 9728 protected-resource metadata advertises.
///
/// `rmcp`'s `select_scopes` asks for the advertised set in full, which is right
/// for a server that advertises one coherent scope but wrong for Google's MCP
/// endpoints. `gmailmcp.googleapis.com` advertises five nested Gmail scopes
/// (verified live): `mail.google.com` (full account access, Google's most
/// restricted), plus `gmail.modify`, `.compose`, `.readonly` and `.metadata`.
/// Requesting all five asks the user to grant far more than the server's tools
/// need, and `gmail.metadata` is documented as not combinable with the other
/// Gmail scopes at all — so the union isn't just over-broad, it risks being
/// rejected outright.
///
/// Each entry is the least-privileged scope that still covers every tool the
/// server offers, and it is *intersected* with what the resource actually
/// advertises (see [`scopes_to_request`]) — this table can therefore only ever
/// narrow a request, never smuggle in a scope the server didn't claim to
/// support. Keyed by the MCP server's own host, which is where the tool surface
/// being scoped lives.
const PREFERRED_SCOPES_BY_RESOURCE_HOST: &[(&str, &[&str])] = &[
    (
        "gmailmcp.googleapis.com",
        &["https://www.googleapis.com/auth/gmail.modify"],
    ),
    (
        "drivemcp.googleapis.com",
        &["https://www.googleapis.com/auth/drive"],
    ),
];

/// Picks the scopes to put in the authorize request: the narrowed set from
/// [`PREFERRED_SCOPES_BY_RESOURCE_HOST`] when this server has an entry and the
/// server advertises every scope in it, otherwise whatever `rmcp` selected from
/// the discovered metadata (resource metadata first, then authorization server
/// metadata, then its own default). Falling back rather than forcing a
/// non-advertised scope keeps a provider that reorganizes its scopes from
/// turning into a hard connect failure here.
fn scopes_to_request(base_url: &str, advertised: Vec<String>) -> Vec<String> {
    let Some(host) = url::Url::parse(base_url)
        .ok()
        .and_then(|parsed| parsed.host_str().map(str::to_ascii_lowercase))
    else {
        return advertised;
    };
    let Some((_, preferred)) = PREFERRED_SCOPES_BY_RESOURCE_HOST
        .iter()
        .find(|(known_host, _)| *known_host == host)
    else {
        return advertised;
    };
    if preferred
        .iter()
        .all(|scope| advertised.iter().any(|candidate| candidate == scope))
    {
        return preferred.iter().map(|scope| scope.to_string()).collect();
    }
    advertised
}

/// The outcome of [`prepare_authorization`]: the URL to send the user's browser
/// to, plus the client registration that URL was built with *if* it came from
/// the user rather than from dynamic registration — the caller persists that
/// (see [`ManualClientRegistration`]) so later refreshes and reconnects don't
/// need it pasted again.
pub(crate) struct PreparedAuthorization {
    pub authorize_url: String,
    pub manual_client: Option<ManualClientRegistration>,
}

/// Redacting `Debug` (the shape `rmcp::transport::auth::StoredCredentials` uses
/// for the same reason): a `PreparedAuthorization` is what error paths and
/// `unwrap()` panics format, and neither the client secret nor the authorize
/// URL's PKCE/CSRF params belong in a log line.
impl std::fmt::Debug for PreparedAuthorization {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PreparedAuthorization")
            .field("authorize_url", &"[REDACTED]")
            .field(
                "manual_client",
                &self
                    .manual_client
                    .as_ref()
                    .map(|client| client.client_id.as_str()),
            )
            .finish()
    }
}

/// Discovers authorization server metadata, then either dynamically
/// registers an OAuth client at `redirect_uri` (RFC 7591, retried once on
/// failure — see [`register_client_with_retry`]) or — if that still fails —
/// configures the user's own `manual_client` against it instead, and returns
/// the resulting PKCE + CSRF authorization URL to send the user to.
/// AppHandle-free and directly unit-testable (see the tests below); the
/// `#[tauri::command]` wrapper ([`mcp_oauth_connect`]) only adds progress
/// events, opening the system browser, and awaiting the loopback callback
/// around this.
pub(crate) async fn prepare_authorization(
    manager: &mut AuthorizationManager,
    base_url: &str,
    redirect_uri: &str,
    manual_client: Option<&ManualClientRegistration>,
) -> Result<PreparedAuthorization, String> {
    let metadata = manager.discover_metadata().await.map_err(auth_err)?;
    let supports_cimd = authorization_server_supports_cimd(&metadata);
    manager.set_metadata(metadata);

    // Client identification, in the priority order the MCP authorization spec
    // lays out (2025-11-25, "Client Registration Approaches"): a client the
    // user pre-registered themselves, then CIMD, then DCR, then ask.
    let mut used_manual_client = None;
    match manual_client.filter(|client| !client.client_id.trim().is_empty()) {
        Some(registration) => {
            let registration = ManualClientRegistration {
                client_id: registration.client_id.trim().to_string(),
                client_secret: registration
                    .client_secret
                    .as_deref()
                    .map(str::trim)
                    .filter(|secret| !secret.is_empty())
                    .map(str::to_string),
            };
            // `OAuthClientConfig::scopes` isn't read by `configure_client`
            // itself (it only feeds the actual authorize request, computed via
            // `select_scopes` below) — left empty here rather than computed
            // twice.
            let mut config = OAuthClientConfig::new(registration.client_id.clone(), redirect_uri);
            if let Some(secret) = registration.client_secret.clone() {
                config = config.with_client_secret(secret);
            }
            manager.configure_client(config).map_err(auth_err)?;
            used_manual_client = Some(registration);
        }
        None if supports_cimd => {
            // Nothing to register and nothing for the user to paste: the
            // client id *is* the URL of this app's published metadata document,
            // which the authorization server fetches to learn the client's name
            // and permitted redirect URIs. Public client, no secret — see
            // `CIMD_CLIENT_ID`.
            manager
                .configure_client(OAuthClientConfig::new(CIMD_CLIENT_ID, redirect_uri))
                .map_err(auth_err)?;
        }
        None => {
            register_client_with_retry(manager, redirect_uri)
                .await
                .map_err(|_| CLIENT_ID_REQUIRED_MESSAGE.to_string())?;
        }
    }

    let scopes = scopes_to_request(base_url, manager.select_scopes(None, &[]));
    let scope_refs: Vec<&str> = scopes.iter().map(String::as_str).collect();
    let authorize_url = manager
        .get_authorization_url(&scope_refs)
        .await
        .map_err(auth_err)?;

    Ok(PreparedAuthorization {
        authorize_url: with_extra_authorize_params(authorize_url),
        manual_client: used_manual_client,
    })
}

/// Query parameters extracted from the single HTTP request the loopback
/// listener accepts — a subset of what an OAuth authorization-code redirect
/// carries (`code`/`state` on success, `error`/`error_description` if the
/// user denied consent or the authorization server rejected the request), plus
/// RFC 9207's optional authorization-server issuer (`iss`).
#[derive(Debug, Default, Clone)]
pub(crate) struct CallbackParams {
    pub code: Option<String>,
    pub state: Option<String>,
    pub error: Option<String>,
    pub error_description: Option<String>,
    pub issuer: Option<String>,
}

/// A single-use, loopback-only HTTP listener that receives one OAuth
/// authorization redirect and then shuts down — reserved (via
/// [`LoopbackListener::bind_for`]) before the authorization URL is even built,
/// so its chosen port can be embedded as the `redirect_uri` sent to the
/// authorization server (both for dynamic client registration and the
/// authorize request itself).
pub(crate) struct LoopbackListener {
    listener: TcpListener,
    /// `http://<loopback-host>:<port>/` — the redirect URI to register and
    /// request authorization against. Most providers use the literal
    /// `127.0.0.1`; Slack requires `localhost` for a desktop PKCE redirect.
    ///
    /// Root path, not something like `/callback`, because an authorization
    /// server that allows loopback redirects generally still matches the
    /// *path* exactly and only waives the port (it can't know which ephemeral
    /// port a native app will get). Google is the case that forces the issue:
    /// a "Desktop app" client has `http://127.0.0.1`/`http://localhost`
    /// implicitly registered with no path, so any path at all comes back as
    /// `Error 400: redirect_uri_mismatch`. Root is also what every Google
    /// client library uses (gcloud, `InstalledAppFlow`), and it costs nothing
    /// elsewhere: [`parse_callback_request`] reads the query string and
    /// ignores the path, and this listener answers exactly one request.
    pub redirect_uri: String,
}

/// The stable loopback port this app uses for `server_id`'s OAuth redirect, so
/// the redirect URI is a fixed string the user can *register* with their
/// provider rather than a different one every attempt.
///
/// A random ephemeral port only works for providers that waive the port when
/// matching loopback redirects (Google does — but only for its "Desktop app"
/// client type). Everything else, including a Google "Web application" client
/// and every Slack app, compares the redirect URI exactly and answers
/// `Error 400: redirect_uri_mismatch` for a port it has never seen. Deriving the
/// port from the server id — deterministically, and per server so two providers
/// never contend — makes "register this one URI once" possible, which is the
/// whole bring-your-own-client model working as intended.
///
/// FNV-1a over the id, mapped into the IANA dynamic/private range
/// (49152-65535): no allocation, no dependency, and stable across restarts and
/// platforms, which a `DefaultHasher` (explicitly not stability-guaranteed)
/// would not be. Collisions between two server ids are possible. A
/// registration-free flow can fall back to an ephemeral port; a manually
/// registered client instead gets an actionable error because its provider may
/// compare the redirect URI, including this port, exactly.
pub(crate) fn loopback_port_for(server_id: &str) -> u16 {
    const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut hash = FNV_OFFSET;
    for byte in server_id.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    const FIRST_DYNAMIC_PORT: u64 = 49152;
    const DYNAMIC_PORT_COUNT: u64 = 65536 - FIRST_DYNAMIC_PORT;
    (FIRST_DYNAMIC_PORT + hash % DYNAMIC_PORT_COUNT) as u16
}

/// Slack recognizes `localhost` (not an IP literal) as a desktop redirect when
/// PKCE is enabled. Identify Slack by its actual MCP endpoint rather than the
/// user-editable server id, so renaming the entry cannot silently change its
/// registered redirect host. Other providers keep the narrower `127.0.0.1`
/// form.
fn loopback_redirect_host_for(base_url: &str) -> &'static str {
    let is_slack_mcp = url::Url::parse(base_url)
        .ok()
        .and_then(|url| url.host_str().map(str::to_owned))
        .is_some_and(|host| host.eq_ignore_ascii_case("mcp.slack.com"));
    if is_slack_mcp {
        "localhost"
    } else {
        "127.0.0.1"
    }
}

/// The redirect URI [`LoopbackListener::bind_for`] will use for `server_id`
/// when its port is free — i.e. the one to register with the provider. Shown in
/// Settings next to the client-id field (see `mcp_oauth_redirect_uri`).
pub(crate) fn preferred_redirect_uri(server_id: &str, base_url: &str) -> String {
    format!(
        "http://{}:{}/",
        loopback_redirect_host_for(base_url),
        loopback_port_for(server_id)
    )
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum LoopbackPortPolicy {
    /// The authorize server learns the actual URI through CIMD/DCR, so a
    /// different loopback port is safe if the preferred one is unavailable.
    AllowEphemeralFallback,
    /// A user registered the displayed URI out of band. Never substitute a
    /// URI that exact-match providers will reject.
    RequirePreferred,
}

impl LoopbackListener {
    /// Binds this server id's stable port (see [`loopback_port_for`]).
    /// Registration-free CIMD/DCR flows may fall back to an OS-assigned port;
    /// manually registered clients must use the preferred port exactly.
    ///
    /// Always `127.0.0.1` specifically (never `0.0.0.0`/`::`): this listener
    /// must only ever be reachable from the same machine's browser, not the
    /// network.
    async fn bind_for(
        server_id: &str,
        base_url: &str,
        policy: LoopbackPortPolicy,
    ) -> Result<Self, String> {
        let preferred = loopback_port_for(server_id);
        let redirect_host = loopback_redirect_host_for(base_url);
        match TcpListener::bind(("127.0.0.1", preferred)).await {
            Ok(listener) => Ok(Self {
                listener,
                redirect_uri: format!("http://{redirect_host}:{preferred}/"),
            }),
            Err(_) if policy == LoopbackPortPolicy::AllowEphemeralFallback => {
                Self::bind_with_redirect_host(redirect_host).await
            }
            Err(error) => Err(format!(
                "Could not bind the registered OAuth callback URI {} for MCP server '{}': \
                 port {} is unavailable ({error}). Close the process using that port, then retry; \
                 this provider may reject a callback on a different port.",
                preferred_redirect_uri(server_id, base_url),
                server_id,
                preferred
            )),
        }
    }

    /// Binds to an OS-assigned ephemeral port on `127.0.0.1` specifically
    /// (never `0.0.0.0`/`::`) — this listener must only ever be reachable
    /// from the same machine's browser, not the network.
    #[cfg(test)]
    pub async fn bind() -> Result<Self, String> {
        Self::bind_with_redirect_host("127.0.0.1").await
    }

    async fn bind_with_redirect_host(redirect_host: &str) -> Result<Self, String> {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .map_err(|e| format!("Failed to bind local OAuth callback listener: {e}"))?;
        let port = listener
            .local_addr()
            .map_err(|e| format!("Failed to read local OAuth callback listener's address: {e}"))?
            .port();
        Ok(Self {
            listener,
            redirect_uri: format!("http://{redirect_host}:{port}/"),
        })
    }

    /// Accepts exactly one connection (rejecting it, without ever reading
    /// its request, if the OS somehow reports a non-loopback peer — belt and
    /// suspenders on top of the `127.0.0.1`-only bind above), parses
    /// `code`/`state`/`error`/`error_description` from its request line's
    /// query string, responds with a short "you can close this tab" HTML
    /// page, and then shuts down — `self` is consumed, so the underlying
    /// `TcpListener` is dropped (and the OS frees the port) whether this
    /// returns `Ok` or `Err`, meaning a second connection attempt to the
    /// same address is refused at the TCP level from then on.
    pub(crate) async fn await_callback(self, timeout: Duration) -> Result<CallbackParams, String> {
        let (mut stream, peer_addr) = tokio::time::timeout(timeout, self.listener.accept())
            .await
            .map_err(|_| "Timed out waiting for the OAuth browser redirect".to_string())?
            .map_err(|e| format!("Failed to accept the OAuth callback connection: {e}"))?;

        if !peer_addr.ip().is_loopback() {
            return Err("Rejected a non-loopback OAuth callback connection".to_string());
        }

        // Read just enough to get the request line (and, defensively, cap
        // total reads so a misbehaving connection can't hang this forever)
        // — the redirect is a bare `GET /?...`, no body.
        let mut buf = Vec::with_capacity(2048);
        let mut chunk = [0u8; 1024];
        loop {
            let n = stream
                .read(&mut chunk)
                .await
                .map_err(|e| format!("Failed to read the OAuth callback request: {e}"))?;
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&chunk[..n]);
            if buf.windows(4).any(|w| w == b"\r\n\r\n") || buf.len() >= 16 * 1024 {
                break;
            }
        }

        let params = parse_callback_request(&buf);

        let body = "<html><body>You can close this tab and return to Little Monkey.</body></html>";
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        let _ = stream.write_all(response.as_bytes()).await;
        let _ = stream.shutdown().await;

        params.ok_or_else(|| "The OAuth callback request could not be parsed".to_string())
    }
}

/// Parses `GET /?code=...&state=...&error=...&iss=... HTTP/1.1` out of a raw request
/// buffer's first line — path-agnostic, so it also handles the `/callback`-style
/// paths other authorization servers may be configured with. Returns `None` only if the request line
/// itself is unparseable (not, e.g., for a request that carries none of the
/// query params — that's a valid `CallbackParams` with every field `None`,
/// left to the caller to reject as "missing code").
fn parse_callback_request(buf: &[u8]) -> Option<CallbackParams> {
    let text = String::from_utf8_lossy(buf);
    let request_line = text.lines().next()?;
    let mut parts = request_line.split_whitespace();
    let _method = parts.next()?;
    let path = parts.next()?;

    let full_url = format!("http://127.0.0.1{path}");
    let parsed = url::Url::parse(&full_url).ok()?;

    let mut params = CallbackParams::default();
    for (key, value) in parsed.query_pairs() {
        match key.as_ref() {
            "code" => params.code = Some(value.into_owned()),
            "state" => params.state = Some(value.into_owned()),
            "error" => params.error = Some(value.into_owned()),
            "error_description" => params.error_description = Some(value.into_owned()),
            "iss" => params.issuer = Some(value.into_owned()),
            _ => {}
        }
    }
    Some(params)
}

/// Emit an `mcp-oauth://status` event to all windows, mirroring
/// `mcp.rs::emit_status`. `phase` is one of: `"discovering"`,
/// `"needs_client_id"`, `"opening_browser"`, `"waiting_for_browser"`,
/// `"exchanging_token"`, `"connected"`, `"error"`, `"cancelled"`.
fn emit_progress<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    server_id: &str,
    phase: &str,
    error: Option<String>,
) {
    let _ = app.emit(
        "mcp-oauth://status",
        serde_json::json!({
            "serverId": server_id,
            "phase": phase,
            "error": error,
        }),
    );
}

/// Returns an error after emitting the terminal progress state the Settings UI
/// relies on to leave its in-progress phase. Keeping this at the command
/// boundary means setup failures and failures deep in the browser flow follow
/// the same contract.
fn fail_with_progress<T>(
    app: &tauri::AppHandle,
    server_id: &str,
    message: String,
) -> Result<T, String> {
    emit_progress(app, server_id, "error", Some(message.clone()));
    Err(message)
}

fn terminal_phase_for_connect_result(
    result: &Result<(), String>,
    cancelled: bool,
) -> Option<&'static str> {
    if cancelled {
        Some("cancelled")
    } else {
        match result {
            Err(message) if message != CLIENT_ID_REQUIRED_MESSAGE => Some("error"),
            Ok(()) | Err(_) => None,
        }
    }
}

/// Rejects a plaintext `http://` MCP server URL as an OAuth base — RFC
/// 8252/BCP 212 permit unencrypted loopback traffic for the installed app's
/// own redirect URI (see [`LoopbackListener`], which only ever binds
/// `127.0.0.1`), but the discovery/DCR/token-exchange traffic this module
/// sends to the MCP server's own origin is a different matter: it carries
/// the freshly issued access/refresh token in the code-exchange response, so
/// running it over plaintext HTTP would let any on-path network attacker
/// (shared Wi-Fi, a compromised router, a corporate proxy) read that
/// response — and tamper with the dynamic-client-registration request —
/// before the credentials ever reach the keychain. `https://` is required
/// for every host except loopback (`127.0.0.1`/`::1`/`localhost`), which is
/// exempted so a locally-running MCP server used for development still
/// works.
fn ensure_oauth_base_url_is_secure(base_url: &str) -> Result<(), String> {
    let parsed = url::Url::parse(base_url)
        .map_err(|e| format!("Invalid MCP server URL '{base_url}': {e}"))?;
    if parsed.scheme() == "https" {
        return Ok(());
    }
    let is_loopback = match parsed.host() {
        Some(url::Host::Domain(domain)) => domain.eq_ignore_ascii_case("localhost"),
        Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
        Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
        None => false,
    };
    if is_loopback {
        return Ok(());
    }
    Err(format!(
        "Refusing to run OAuth for MCP server against '{base_url}' over plaintext HTTP — only https:// (or a loopback address, for local development) is supported, since the authorization code exchange would otherwise send the access/refresh token over the network unencrypted."
    ))
}

/// Resolves the configured `McpServerEntry` for `server_id` and returns its
/// HTTP URL — errors for an unknown id or a `Stdio` transport (OAuth only
/// ever applies to `Http` servers), and for a non-`https://`,
/// non-loopback URL (see [`ensure_oauth_base_url_is_secure`]).
async fn resolve_http_base_url(app: &tauri::AppHandle, server_id: &str) -> Result<String, String> {
    let config = crate::mcp::load_config_impl(&crate::mcp::config_file_path(app)?)?;
    let entry = config
        .servers
        .iter()
        .find(|s| s.id == server_id)
        .ok_or_else(|| format!("Unknown MCP server '{server_id}'"))?;
    match &entry.transport {
        crate::mcp::McpTransport::Http { url } => {
            ensure_oauth_base_url_is_secure(url)?;
            Ok(url.clone())
        }
        crate::mcp::McpTransport::Stdio { .. } => Err(format!(
            "MCP server '{server_id}' is a stdio server — OAuth only applies to HTTP MCP servers"
        )),
    }
}

async fn cancellable_oauth_step<T>(
    cancel: &CancellationToken,
    future: impl std::future::Future<Output = Result<T, String>>,
) -> Result<T, String> {
    tokio::select! {
        result = future => result,
        _ = cancel.cancelled() => Err(OAUTH_CANCELLED_MESSAGE.to_string()),
    }
}

/// Runs the actual discover -> register-or-client-id -> authorize ->
/// (browser) -> loopback callback -> exchange pipeline for one connect
/// attempt. Split out from [`mcp_oauth_connect`] so that command's body is
/// just "set up cancellation bookkeeping around this".
async fn run_connect_flow(
    app: &tauri::AppHandle,
    state: &AppState,
    server_id: &str,
    base_url: &str,
    manual_client: Option<ManualClientRegistration>,
    cancel: &CancellationToken,
) -> Result<(), String> {
    emit_progress(app, server_id, "discovering", None);
    let mut manager = cancellable_oauth_step(cancel, async {
        AuthorizationManager::new(base_url).await.map_err(auth_err)
    })
    .await?;
    manager.set_credential_store(KeychainCredentialStore::new(server_id.to_string()));

    let port_policy = if manual_client.is_some() {
        LoopbackPortPolicy::RequirePreferred
    } else {
        LoopbackPortPolicy::AllowEphemeralFallback
    };
    let listener = cancellable_oauth_step(
        cancel,
        LoopbackListener::bind_for(server_id, base_url, port_policy),
    )
    .await?;
    let redirect_uri = listener.redirect_uri.clone();

    let prepared = match cancellable_oauth_step(
        cancel,
        prepare_authorization(
            &mut manager,
            base_url,
            &redirect_uri,
            manual_client.as_ref(),
        ),
    )
    .await
    {
        Ok(prepared) => prepared,
        Err(message) if message == CLIENT_ID_REQUIRED_MESSAGE => {
            emit_progress(app, server_id, "needs_client_id", Some(message.clone()));
            return Err(message);
        }
        Err(message) => return Err(message),
    };

    emit_progress(app, server_id, "opening_browser", None);
    if cancel.is_cancelled() {
        return Err(OAUTH_CANCELLED_MESSAGE.to_string());
    }
    {
        use tauri_plugin_opener::OpenerExt;
        app.opener()
            .open_url(prepared.authorize_url, None::<String>)
            .map_err(|e| format!("Failed to open the system browser for OAuth: {e}"))?;
    }

    emit_progress(app, server_id, "waiting_for_browser", None);
    let callback = cancellable_oauth_step(
        cancel,
        listener.await_callback(Duration::from_secs(LOOPBACK_TIMEOUT_SECS)),
    )
    .await?;

    if let Some(error) = callback.error {
        let message = match callback.error_description {
            Some(desc) => format!("OAuth authorization was not granted: {error} ({desc})"),
            None => format!("OAuth authorization was not granted: {error}"),
        };
        return Err(message);
    }

    let code = callback
        .code
        .ok_or_else(|| "The OAuth callback did not include an authorization code".to_string())?;
    let csrf = callback.state.ok_or_else(|| {
        "The OAuth callback did not include the expected state parameter".to_string()
    })?;

    emit_progress(app, server_id, "exchanging_token", None);
    let refresh_lock = refresh_lock_for(state, server_id);
    let _refresh_guard = tokio::select! {
        guard = refresh_lock.lock() => guard,
        _ = cancel.cancelled() => return Err(OAUTH_CANCELLED_MESSAGE.to_string()),
    };
    let staged_exchange = StagedOAuthExchange::begin(server_id, prepared.manual_client.as_ref())?;
    let exchange_result = tokio::select! {
        result = manager.exchange_code_for_token_with_issuer(
            &code,
            &csrf,
            callback.issuer.as_deref(),
        ) => result.map_err(auth_err),
        _ = cancel.cancelled() => Err(OAUTH_CANCELLED_MESSAGE.to_string()),
    };
    complete_oauth_exchange(staged_exchange, exchange_result)?;

    emit_progress(app, server_id, "connected", None);
    Ok(())
}

/// The credential records staged around one serialized token exchange.
///
/// The OAuth library persists the token inside `exchange_code_for_token`, so
/// saving the client afterward can strand a new token with old client
/// credentials if that second keychain write fails. Stage the client first and
/// snapshot the opaque prior token record as well. An unsuccessful or
/// cancelled exchange restores both records; `Drop` remains a last-resort
/// rollback if the command future itself is aborted.
struct StagedOAuthExchange {
    server_id: String,
    records: std::sync::Arc<dyn OAuthRecordStore>,
    previous_credentials: Option<String>,
    previous_manual_client: Option<Option<String>>,
    armed: bool,
}

impl StagedOAuthExchange {
    fn begin(server_id: &str, pending: Option<&ManualClientRegistration>) -> Result<Self, String> {
        Self::begin_with_record_store(
            server_id,
            pending,
            std::sync::Arc::new(KeychainOAuthRecordStore),
        )
    }

    #[cfg(test)]
    fn begin_with_store(
        server_id: &str,
        pending: Option<&ManualClientRegistration>,
        records: std::sync::Arc<dyn OAuthRecordStore>,
    ) -> Result<Self, String> {
        Self::begin_with_record_store(server_id, pending, records)
    }

    fn begin_with_record_store(
        server_id: &str,
        pending: Option<&ManualClientRegistration>,
        records: std::sync::Arc<dyn OAuthRecordStore>,
    ) -> Result<Self, String> {
        let previous_credentials = records.load_credentials(server_id)?;
        let previous_manual_client = if let Some(pending) = pending {
            let previous = records.load_manual_client(server_id)?;
            let record = serde_json::to_string(pending)
                .map_err(|e| format!("Failed to serialize OAuth client registration: {e}"))?;
            records.save_manual_client(server_id, &record)?;
            Some(previous)
        } else {
            None
        };
        Ok(Self {
            server_id: server_id.to_string(),
            records,
            previous_credentials,
            previous_manual_client,
            armed: true,
        })
    }

    fn restore_previous(&self) -> Result<(), String> {
        let credentials_result = match self.previous_credentials.as_deref() {
            Some(record) => self.records.save_credentials(&self.server_id, record),
            None => self.records.remove_credentials(&self.server_id),
        };
        let manual_result = match self.previous_manual_client.as_ref() {
            Some(Some(record)) => self.records.save_manual_client(&self.server_id, record),
            Some(None) => self.records.remove_manual_client(&self.server_id),
            None => Ok(()),
        };

        match (credentials_result, manual_result) {
            (Ok(()), Ok(())) => Ok(()),
            (Err(credentials_error), Ok(())) => Err(credentials_error),
            (Ok(()), Err(manual_error)) => Err(manual_error),
            (Err(credentials_error), Err(manual_error)) => Err(format!(
                "{credentials_error}; additionally failed to restore the previous OAuth client registration: {manual_error}"
            )),
        }
    }

    fn commit(mut self) {
        self.armed = false;
    }

    fn rollback(mut self) -> Result<(), String> {
        self.armed = false;
        self.restore_previous()
    }
}

impl Drop for StagedOAuthExchange {
    fn drop(&mut self) {
        if self.armed {
            let _ = self.restore_previous();
        }
    }
}

fn complete_oauth_exchange<T>(
    staged: StagedOAuthExchange,
    exchange_result: Result<T, String>,
) -> Result<T, String> {
    match exchange_result {
        Ok(value) => {
            staged.commit();
            Ok(value)
        }
        Err(exchange_error) => {
            if let Err(rollback_error) = staged.rollback() {
                return Err(format!(
                    "{exchange_error}; additionally failed to restore the previous OAuth records: {rollback_error}"
                ));
            }
            Err(exchange_error)
        }
    }
}

/// Returns the sticky, shared cancellation token for every overlapping OAuth
/// connect attempt for one server id. Unlike `Notify`, cancellation is retained
/// if it happens immediately before a caller starts awaiting it.
fn oauth_cancel_token_for(
    state: &AppState,
    server_id: &str,
) -> Result<std::sync::Arc<CancellationToken>, String> {
    let mut guard = state
        .mcp_oauth_cancel
        .lock()
        .map_err(|_| "OAuth cancel lock poisoned".to_string())?;
    let (token, active_attempts) = guard
        .entry(server_id.to_string())
        .or_insert_with(|| (std::sync::Arc::new(CancellationToken::new()), 0));
    *active_attempts = active_attempts
        .checked_add(1)
        .ok_or_else(|| "Too many concurrent OAuth connection attempts".to_string())?;
    Ok(token.clone())
}

/// Removes a server id's shared token only after the final overlapping connect
/// call has finished. A later, genuinely new attempt therefore receives a
/// fresh non-cancelled token.
fn release_oauth_cancel_token(state: &AppState, server_id: &str) {
    if let Ok(mut guard) = state.mcp_oauth_cancel.lock() {
        if let std::collections::hash_map::Entry::Occupied(mut entry) =
            guard.entry(server_id.to_string())
        {
            let active_attempts = &mut entry.get_mut().1;
            debug_assert!(
                *active_attempts > 0,
                "OAuth cancellation entry released without an active attempt"
            );
            if *active_attempts <= 1 {
                entry.remove();
            } else {
                *active_attempts -= 1;
            }
        }
    }
}

/// Runs a full OAuth 2.0 connect for HTTP MCP server `server_id`: discovery,
/// dynamic client registration (or `client_id`/`client_secret` — the user's own
/// OAuth app registration — as a fallback when the server doesn't support DCR),
/// opening the system browser, awaiting the loopback redirect, exchanging the
/// code, and saving the resulting credentials to the OS keychain.
///
/// Both client fields are optional and both are remembered (keychain, see
/// [`ManualClientRegistration`]): omitting them on a later connect for the same
/// server id reuses what was saved, so re-consenting is one click. Pass
/// `client_secret` only for a provider that requires client authentication at
/// the token endpoint — Google does, most MCP-native providers don't. Streams progress via `mcp-oauth://status`
/// events (see [`emit_progress`]) so the Settings UI isn't left frozen for
/// however long the user takes to complete browser consent — cancellable
/// mid-flight via [`mcp_oauth_cancel`].
///
/// Does not itself connect/reconnect the MCP server — `McpPanel.tsx`'s
/// `OAuthConnectSection` follows a successful resolution with the normal
/// `mcp_connect`, which then picks up the just-saved credentials via
/// `mcp::get_access_token_if_connected`.
///
/// Shares one sticky [`CancellationToken`] per server id across overlapping
/// calls (via `entry().or_insert_with()` plus an active-attempt count updated
/// under the map mutex) rather than unconditionally inserting/removing. Two
/// overlapping `mcp_oauth_connect` calls for the same `server_id` (a
/// double-click on "Connect via OAuth" before React disables the button, or
/// two Settings windows) must never let the second call's unconditional
/// `insert` silently orphan the first call's token from the map (breaking
/// `mcp_oauth_cancel` for it), nor let whichever call finishes first
/// unconditionally `remove` an entry the other call still needs in order to
/// ever be cancellable. The sticky cancelled state also closes `Notify`'s
/// lost-wakeup window.
#[tauri::command(rename_all = "snake_case")]
pub async fn mcp_oauth_connect(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    server_id: String,
    client_id: Option<String>,
    client_secret: Option<String>,
) -> Result<(), String> {
    if let Err(message) = crate::mcp::validate_id(&server_id) {
        return fail_with_progress(&app, &server_id, message);
    }
    let base_url = match resolve_http_base_url(&app, &server_id).await {
        Ok(base_url) => base_url,
        Err(message) => return fail_with_progress(&app, &server_id, message),
    };

    fn cleaned(value: Option<String>) -> Option<String> {
        value
            .map(|raw| raw.trim().to_string())
            .filter(|trimmed| !trimmed.is_empty())
    }
    let manual_client = match cleaned(client_id) {
        Some(client_id) => Some(ManualClientRegistration {
            client_id,
            client_secret: cleaned(client_secret),
        }),
        // Nothing pasted this time — fall back to whatever registration this
        // server id already has saved, so a reconnect after a revoked refresh
        // token doesn't send the user back to their provider's console.
        None => match load_manual_client(&server_id) {
            Ok(saved) => saved,
            Err(message) => return fail_with_progress(&app, &server_id, message),
        },
    };

    let cancel = match oauth_cancel_token_for(&state, &server_id) {
        Ok(cancel) => cancel,
        Err(message) => return fail_with_progress(&app, &server_id, message),
    };

    let result =
        run_connect_flow(&app, &state, &server_id, &base_url, manual_client, &cancel).await;
    let cancelled = result
        .as_ref()
        .is_err_and(|message| message == OAUTH_CANCELLED_MESSAGE);

    release_oauth_cancel_token(&state, &server_id);

    match terminal_phase_for_connect_result(&result, cancelled) {
        Some("cancelled") => emit_progress(&app, &server_id, "cancelled", None),
        Some("error") => emit_progress(&app, &server_id, "error", result.clone().err()),
        _ => {}
    }

    result
}

/// The loopback redirect URI this app will use for `server_id`'s OAuth flow —
/// what the user registers with their provider when they register their own
/// OAuth app (Settings shows it beside the client-id field).
///
/// Deterministic, so registering it once is enough: see [`loopback_port_for`].
/// Needs no network access and no saved credentials, so the UI can show it
/// before any connect attempt.
#[tauri::command(rename_all = "snake_case")]
pub async fn mcp_oauth_redirect_uri(
    app: tauri::AppHandle,
    server_id: String,
) -> Result<String, String> {
    crate::mcp::validate_id(&server_id)?;
    let base_url = resolve_http_base_url(&app, &server_id).await?;
    Ok(preferred_redirect_uri(&server_id, &base_url))
}

/// Cancels an in-flight [`mcp_oauth_connect`] for `server_id`, if one is
/// running. A no-op success if none is — the caller's desired end state
/// (no connect attempt in flight for this id) already holds.
#[tauri::command(rename_all = "snake_case")]
pub fn mcp_oauth_cancel(
    state: tauri::State<'_, AppState>,
    server_id: String,
) -> Result<(), String> {
    crate::mcp::validate_id(&server_id)?;
    let guard = state
        .mcp_oauth_cancel
        .lock()
        .map_err(|_| "OAuth cancel lock poisoned".to_string())?;
    if let Some((cancel, _active_attempts)) = guard.get(&server_id) {
        cancel.cancel();
    }
    Ok(())
}

/// Clears server `id`'s saved OAuth credentials from the keychain. A no-op
/// success if none were saved. Does not disconnect a currently-running MCP
/// connection — call `mcp_disconnect`/`mcp_connect` afterward if the server
/// should stop using the (now-removed) OAuth token immediately rather than
/// on its next natural reconnect.
#[tauri::command(rename_all = "snake_case")]
pub fn mcp_oauth_disconnect(server_id: String) -> Result<(), String> {
    crate::mcp::validate_id(&server_id)?;
    remove_oauth_credentials_impl(&server_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unique_server_id(label: &str) -> String {
        format!("oauth-test-{label}-{}", uuid::Uuid::new_v4())
    }

    #[derive(Default)]
    struct InMemoryOAuthRecordStore {
        credentials: std::sync::Mutex<std::collections::BTreeMap<String, String>>,
        manual_clients: std::sync::Mutex<std::collections::BTreeMap<String, String>>,
    }

    impl OAuthRecordStore for InMemoryOAuthRecordStore {
        fn load_credentials(&self, server_id: &str) -> Result<Option<String>, String> {
            self.credentials
                .lock()
                .map_err(|_| "in-memory OAuth credentials lock poisoned".to_string())
                .map(|records| records.get(server_id).cloned())
        }

        fn save_credentials(&self, server_id: &str, record: &str) -> Result<(), String> {
            self.credentials
                .lock()
                .map_err(|_| "in-memory OAuth credentials lock poisoned".to_string())?
                .insert(server_id.to_string(), record.to_string());
            Ok(())
        }

        fn remove_credentials(&self, server_id: &str) -> Result<(), String> {
            self.credentials
                .lock()
                .map_err(|_| "in-memory OAuth credentials lock poisoned".to_string())?
                .remove(server_id);
            Ok(())
        }

        fn load_manual_client(&self, server_id: &str) -> Result<Option<String>, String> {
            self.manual_clients
                .lock()
                .map_err(|_| "in-memory OAuth client lock poisoned".to_string())
                .map(|records| records.get(server_id).cloned())
        }

        fn save_manual_client(&self, server_id: &str, record: &str) -> Result<(), String> {
            self.manual_clients
                .lock()
                .map_err(|_| "in-memory OAuth client lock poisoned".to_string())?
                .insert(server_id.to_string(), record.to_string());
            Ok(())
        }

        fn remove_manual_client(&self, server_id: &str) -> Result<(), String> {
            self.manual_clients
                .lock()
                .map_err(|_| "in-memory OAuth client lock poisoned".to_string())?
                .remove(server_id);
            Ok(())
        }
    }

    fn load_test_manual_client(
        records: &InMemoryOAuthRecordStore,
        server_id: &str,
    ) -> ManualClientRegistration {
        serde_json::from_str(
            &records
                .load_manual_client(server_id)
                .unwrap()
                .expect("manual client record"),
        )
        .unwrap()
    }

    // --- KeychainCredentialStore ------------------------------------------

    #[tokio::test]
    async fn credential_store_roundtrips_via_the_keychain_and_never_touches_a_json_file() {
        let server_id = unique_server_id("roundtrip");
        let store = KeychainCredentialStore::new(server_id.clone());

        assert!(
            store.load().await.unwrap().is_none(),
            "a never-saved server id must start with no stored credentials"
        );

        let creds = StoredCredentials::new(
            "test-client-id".to_string(),
            None,
            vec!["mcp".to_string()],
            Some(1_700_000_000),
        );
        store.save(creds.clone()).await.unwrap();

        // Read back through both the store's own `load()` AND a raw
        // `keyring::Entry` for the exact same account, to confirm the value
        // actually lives in the OS keychain (not some in-memory fallback
        // this test just happens not to exercise the absence of).
        let loaded = store.load().await.unwrap().unwrap();
        assert_eq!(loaded.client_id, "test-client-id");
        assert_eq!(loaded.granted_scopes, vec!["mcp".to_string()]);
        assert_eq!(loaded.token_received_at, Some(1_700_000_000));

        let raw_entry =
            keyring::Entry::new(&KEYCHAIN_SERVICE, &keychain_account(&server_id)).unwrap();
        let raw_json = raw_entry.get_password().unwrap();
        assert!(
            raw_json.contains("test-client-id"),
            "the keychain entry must hold the serialized credentials directly, not a pointer to a file"
        );

        store.clear().await.unwrap();
        assert!(store.load().await.unwrap().is_none());
        match raw_entry.get_password() {
            Err(keyring::Error::NoEntry) => {}
            // Never the payload itself: on a failure this runs in CI, and a
            // panic that prints the entry would put whatever the keychain
            // still holds into the build log. That rules out `{error:?}` too
            // — `Error` derives Debug, and `BadEncoding`/`Ambiguous` carry the
            // stored bytes. Display is the variant that redacts them.
            Ok(_) => panic!("expected the keychain entry to be gone after clear(), but it still holds a credential"),
            Err(error) => panic!("expected the keychain entry to be gone after clear(), got {error}"),
        }
    }

    #[test]
    fn keychain_account_namespaces_oauth_credentials_separately_from_manual_tokens() {
        assert_eq!(keychain_account("my-server"), "mcp-oauth:my-server");
    }

    #[test]
    fn has_oauth_credentials_is_false_when_nothing_is_saved() {
        assert!(!has_oauth_credentials(
            "never-configured-oauth-server-id-xyz"
        ));
    }

    #[test]
    fn remove_of_unknown_credentials_is_a_no_op_success() {
        remove_oauth_credentials_impl(&unique_server_id("remove-missing")).unwrap();
    }

    #[test]
    fn paired_removal_restores_the_first_record_when_the_second_delete_fails() {
        let first = std::cell::RefCell::new(Some("saved-client".to_string()));

        let error = remove_pair_with_rollback(
            || Ok(first.borrow().clone()),
            || {
                first.borrow_mut().take();
                Ok(())
            },
            || Err("token delete failed".to_string()),
            |previous| {
                first.borrow_mut().replace(previous.clone());
                Ok(())
            },
        )
        .unwrap_err();

        assert_eq!(error, "token delete failed");
        assert_eq!(first.borrow().as_deref(), Some("saved-client"));
    }

    #[test]
    fn paired_removal_commits_both_deletes_when_the_second_succeeds() {
        let first = std::cell::RefCell::new(Some("saved-client".to_string()));
        let second = std::cell::RefCell::new(Some("saved-token".to_string()));

        remove_pair_with_rollback(
            || Ok(first.borrow().clone()),
            || {
                first.borrow_mut().take();
                Ok(())
            },
            || {
                second.borrow_mut().take();
                Ok(())
            },
            |previous| {
                first.borrow_mut().replace(previous.clone());
                Ok(())
            },
        )
        .unwrap();

        assert!(first.borrow().is_none());
        assert!(second.borrow().is_none());
    }

    // --- OAuth base URL scheme enforcement ---------------------------------

    #[test]
    fn secure_url_check_accepts_https() {
        ensure_oauth_base_url_is_secure("https://mcp.example.com").unwrap();
    }

    #[test]
    fn secure_url_check_rejects_plaintext_http_on_a_remote_host() {
        let err = ensure_oauth_base_url_is_secure("http://mcp.example.com").unwrap_err();
        assert!(err.contains("plaintext HTTP"), "unexpected error: {err}");
    }

    #[test]
    fn secure_url_check_allows_plaintext_http_on_loopback_for_local_dev() {
        ensure_oauth_base_url_is_secure("http://127.0.0.1:8080").unwrap();
        ensure_oauth_base_url_is_secure("http://localhost:8080").unwrap();
        ensure_oauth_base_url_is_secure("http://[::1]:8080").unwrap();
    }

    #[test]
    fn secure_url_check_rejects_a_domain_that_merely_contains_localhost() {
        // A naive substring check on "localhost" would wrongly allow this —
        // the actual host must equal "localhost", not just contain it.
        let err = ensure_oauth_base_url_is_secure("http://localhost.evil.example").unwrap_err();
        assert!(err.contains("plaintext HTTP"), "unexpected error: {err}");
    }

    // --- OAuth token-refresh serialization ----------------------------------

    #[tokio::test]
    async fn concurrent_get_access_token_calls_for_the_same_server_id_are_serialized_not_parallel()
    {
        // Regression test for the refresh-token race: two `mcp_connect`
        // calls for the same OAuth-connected server must never both reach
        // `AuthorizationManager::get_access_token` at once. Since there's no
        // saved credential for this id, `get_access_token_if_connected`
        // itself short-circuits to `Ok(None)` before ever taking the lock —
        // so this test instead exercises `refresh_lock_for` directly: the
        // same server id must hand back the exact same underlying `Arc` to
        // every concurrent caller (so a `.lock().await` in one genuinely
        // blocks the other), while a different server id must get its own,
        // independent lock.
        let state = AppState::default();
        let id = unique_server_id("refresh-lock");

        let lock_a = refresh_lock_for(&state, &id);
        let lock_b = refresh_lock_for(&state, &id);
        assert!(
            std::sync::Arc::ptr_eq(&lock_a, &lock_b),
            "two calls for the same server id must share one lock instance"
        );

        let other_id = unique_server_id("refresh-lock-other");
        let lock_other = refresh_lock_for(&state, &other_id);
        assert!(
            !std::sync::Arc::ptr_eq(&lock_a, &lock_other),
            "different server ids must never share a lock instance"
        );

        // With `lock_a` held, a concurrent attempt to acquire `lock_b`
        // (the same underlying lock) must not succeed until it's released.
        let guard = lock_a.lock().await;
        assert!(
            lock_b.try_lock().is_err(),
            "a second caller for the same server id must be blocked while the first holds the lock"
        );
        drop(guard);
        assert!(
            lock_b.try_lock().is_ok(),
            "the lock must be acquirable again once the first caller releases it"
        );
    }

    #[tokio::test]
    async fn oauth_cancellation_is_sticky_shared_and_replaced_after_overlapping_calls_finish() {
        let state = AppState::default();
        let server_id = unique_server_id("sticky-cancel");
        let first = oauth_cancel_token_for(&state, &server_id).unwrap();
        let second = oauth_cancel_token_for(&state, &server_id).unwrap();
        assert!(
            std::sync::Arc::ptr_eq(&first, &second),
            "overlapping connects must share one cancellation token"
        );

        // Cancel before the second waiter is first polled. CancellationToken
        // remembers this state; Notify::notify_waiters would lose it.
        first.cancel();
        tokio::time::timeout(Duration::from_millis(50), second.cancelled())
            .await
            .expect("cancellation must remain observable after the signal");

        // The first completion cannot remove the token while another
        // overlapping call still owns it.
        release_oauth_cancel_token(&state, &server_id);
        assert!(state
            .mcp_oauth_cancel
            .lock()
            .unwrap()
            .contains_key(&server_id));

        // Both caller-owned Arc clones deliberately remain alive here. The
        // old strong-count cleanup let both finishers observe the same count
        // and retain a cancelled map entry forever; the explicit attempt
        // count must remove it on the second release regardless of Arc drops.
        release_oauth_cancel_token(&state, &server_id);
        assert!(!state
            .mcp_oauth_cancel
            .lock()
            .unwrap()
            .contains_key(&server_id));

        let fresh = oauth_cancel_token_for(&state, &server_id).unwrap();
        assert!(
            !fresh.is_cancelled(),
            "a later independent connect must receive a fresh token"
        );
        release_oauth_cancel_token(&state, &server_id);
    }

    #[test]
    fn connect_results_map_every_ordinary_failure_to_a_terminal_error_phase() {
        assert_eq!(
            terminal_phase_for_connect_result(&Err("setup failed".to_string()), false),
            Some("error")
        );
        assert_eq!(
            terminal_phase_for_connect_result(&Err(CLIENT_ID_REQUIRED_MESSAGE.to_string()), false),
            None,
            "needs-client-id already has its own terminal UI phase"
        );
        assert_eq!(
            terminal_phase_for_connect_result(&Err("OAuth connection cancelled".to_string()), true),
            Some("cancelled")
        );
        assert_eq!(terminal_phase_for_connect_result(&Ok(()), false), None);
    }

    // --- LoopbackListener --------------------------------------------------

    /// Regression guard: a path on the loopback redirect URI makes Google
    /// answer `Error 400: redirect_uri_mismatch` before the user ever sees a
    /// consent screen, because a Desktop-app client's implicitly registered
    /// loopback URIs carry no path and only the *port* is waived. See
    /// `LoopbackListener::redirect_uri`.
    #[test]
    fn loopback_port_is_stable_per_server_and_inside_the_dynamic_range() {
        // Stability is the entire point: the user registers this URI with their
        // provider once, so it must survive restarts and app upgrades. These
        // are literal expected values, not a recomputation of the function
        // under test — a change to the derivation breaks already-registered
        // redirect URIs and has to be a deliberate, visible edit here.
        assert_eq!(loopback_port_for("gmail"), 51669);
        assert_eq!(loopback_port_for("slack"), 54471);
        assert_eq!(
            preferred_redirect_uri("gmail", "https://gmailmcp.googleapis.com/mcp/v1"),
            "http://127.0.0.1:51669/",
            "the redirect URI shown in Settings must match what the flow binds"
        );
        assert_eq!(
            preferred_redirect_uri("slack", "https://mcp.slack.com/mcp"),
            "http://localhost:54471/",
            "Slack only treats localhost as a desktop PKCE redirect"
        );

        // Distinct servers get distinct ports, so two providers' flows don't
        // contend, and every port is in the IANA dynamic/private range.
        let ports: Vec<u16> = ["gmail", "google-drive", "slack", "notion", "atlassian"]
            .iter()
            .map(|id| loopback_port_for(id))
            .collect();
        for port in &ports {
            assert!(*port >= 49152, "port {port} is outside the dynamic range");
        }
        let unique: std::collections::HashSet<&u16> = ports.iter().collect();
        assert_eq!(
            unique.len(),
            ports.len(),
            "unexpected port collision: {ports:?}"
        );
    }

    #[test]
    fn slack_redirect_host_is_derived_from_the_mcp_url_not_the_server_id() {
        assert!(
            preferred_redirect_uri("renamed-provider", "https://mcp.slack.com/mcp")
                .starts_with("http://localhost:"),
            "renaming the Slack server must not change its registered redirect host"
        );
        assert!(
            preferred_redirect_uri("slack", "https://mcp.notion.com/mcp")
                .starts_with("http://127.0.0.1:"),
            "an unrelated server named slack must not receive Slack-specific behavior"
        );
        assert!(
            preferred_redirect_uri("lookalike", "https://mcp.slack.com.evil.example/mcp")
                .starts_with("http://127.0.0.1:"),
            "a lookalike host must not receive Slack-specific behavior"
        );
    }

    #[tokio::test]
    async fn bind_for_falls_back_only_when_registration_mode_allows_it() {
        let server_id = "port-fallback-probe";
        let base_url = "https://mcp.example.com/mcp";
        let expected = preferred_redirect_uri(server_id, base_url);

        let listener = LoopbackListener::bind_for(
            server_id,
            base_url,
            LoopbackPortPolicy::AllowEphemeralFallback,
        )
        .await
        .unwrap();
        assert_eq!(listener.redirect_uri, expected);

        // CIMD/DCR learns the URI used for this attempt, so falling back is
        // safe when no manual registration exists.
        let fallback = LoopbackListener::bind_for(
            server_id,
            base_url,
            LoopbackPortPolicy::AllowEphemeralFallback,
        )
        .await
        .unwrap();
        assert_ne!(fallback.redirect_uri, expected);
        assert!(fallback.redirect_uri.starts_with("http://127.0.0.1:"));

        // A manually registered client must not silently substitute an
        // unregistered port. Keep the first listener alive to hold it.
        let error =
            LoopbackListener::bind_for(server_id, base_url, LoopbackPortPolicy::RequirePreferred)
                .await
                .err()
                .expect("a manually registered redirect must not change ports");
        assert!(error.contains(&expected), "unexpected error: {error}");
        assert!(
            error.contains("Close the process using that port"),
            "the error must tell the user how to recover: {error}"
        );
    }

    #[tokio::test]
    async fn loopback_redirect_uri_uses_the_root_path_so_google_accepts_it() {
        let listener = LoopbackListener::bind().await.unwrap();
        let parsed = url::Url::parse(&listener.redirect_uri).unwrap();
        assert_eq!(parsed.path(), "/", "unexpected redirect URI path");
        assert!(
            parsed.port().is_some(),
            "the ephemeral port must be present"
        );
    }

    #[tokio::test]
    async fn loopback_listener_binds_to_127_0_0_1_only_and_parses_the_callback_query() {
        let listener = LoopbackListener::bind().await.unwrap();
        assert!(
            listener.redirect_uri.starts_with("http://127.0.0.1:"),
            "must bind loopback-only, got redirect_uri {}",
            listener.redirect_uri
        );

        let redirect_uri = listener.redirect_uri.clone();
        let client = tokio::spawn(async move {
            reqwest::get(format!("{redirect_uri}?code=abc123&state=xyz789")).await
        });

        let params = listener
            // 20s, not a tighter bound: this races a real loopback socket
            // against a real `reqwest::get`, and a too-tight fixed timeout
            // was observed to fail intermittently under CPU/scheduler
            // contention (e.g. right after a `cargo check` in the same
            // session, or a busy CI runner) even though nothing was actually
            // wrong — see this test's own git history for the flake report.
            .await_callback(Duration::from_secs(20))
            .await
            .unwrap();
        assert_eq!(params.code.as_deref(), Some("abc123"));
        assert_eq!(params.state.as_deref(), Some("xyz789"));
        assert!(params.error.is_none());

        let response = client.await.unwrap().unwrap();
        assert!(response.status().is_success());
    }

    #[tokio::test]
    async fn loopback_listener_stops_accepting_connections_after_the_first_callback() {
        let listener = LoopbackListener::bind().await.unwrap();
        let addr = listener.redirect_uri.clone();

        let first_addr = addr.clone();
        tokio::spawn(async move {
            let _ = reqwest::get(format!("{first_addr}?code=x&state=y")).await;
        });
        listener
            // 20s, not a tighter bound: this races a real loopback socket
            // against a real `reqwest::get`, and a too-tight fixed timeout
            // was observed to fail intermittently under CPU/scheduler
            // contention (e.g. right after a `cargo check` in the same
            // session, or a busy CI runner) even though nothing was actually
            // wrong — see this test's own git history for the flake report.
            .await_callback(Duration::from_secs(20))
            .await
            .unwrap();

        // The `TcpListener` was consumed and dropped by `await_callback`, so
        // the OS has freed the port — a second connection attempt to the
        // exact same address must fail rather than being served.
        let second = reqwest::Client::builder()
            .timeout(Duration::from_millis(500))
            .build()
            .unwrap();
        let result = second.get(format!("{addr}?code=z&state=w")).send().await;
        assert!(
            result.is_err(),
            "expected the loopback listener to refuse a second connection after its single use"
        );
    }

    #[tokio::test]
    async fn loopback_listener_times_out_when_no_callback_ever_arrives() {
        let listener = LoopbackListener::bind().await.unwrap();
        let err = listener
            .await_callback(Duration::from_millis(50))
            .await
            .unwrap_err();
        assert!(err.contains("Timed out"), "unexpected error: {err}");
    }

    #[test]
    fn parse_callback_request_extracts_error_params_when_consent_is_denied() {
        let raw = b"GET /callback?error=access_denied&error_description=User+said+no&state=xyz&iss=https%3A%2F%2Fauth.example.com HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n";
        let params = parse_callback_request(raw).unwrap();
        assert_eq!(params.error.as_deref(), Some("access_denied"));
        assert_eq!(params.error_description.as_deref(), Some("User said no"));
        assert_eq!(params.state.as_deref(), Some("xyz"));
        assert_eq!(params.issuer.as_deref(), Some("https://auth.example.com"));
        assert!(params.code.is_none());
    }

    // --- OAuth connect state machine (mocked discovery/token-exchange) -----

    /// Spawns a minimal raw-TCP fake OAuth authorization server, mirroring
    /// `web.rs`'s hand-rolled `TcpListener` test-server idiom (no mocking
    /// crate dependency, no real network access — loopback only). Serves 404
    /// for every well-known discovery probe, which makes
    /// `AuthorizationManager::discover_metadata` fall back to its
    /// derive-from-base-url legacy endpoints (`{base}/authorize`,
    /// `{base}/token`, `{base}/register`); a Dynamic Client Registration
    /// response at `POST /register` when `register_client_id` is `Some`
    /// (otherwise 400, simulating a server without DCR support); and a
    /// token-exchange response at `POST /token`.
    fn spawn_fake_oauth_server(register_client_id: Option<&'static str>) -> String {
        use std::io::{Read, Write};
        use std::net::TcpListener as StdTcpListener;

        let listener = StdTcpListener::bind("127.0.0.1:0").expect("bind fake oauth server");
        let addr = listener.local_addr().unwrap();

        std::thread::spawn(move || {
            while let Ok((mut stream, _)) = listener.accept() {
                let mut buf = [0u8; 8192];
                let n = match stream.read(&mut buf) {
                    Ok(n) => n,
                    Err(_) => continue,
                };
                let request = String::from_utf8_lossy(&buf[..n]);
                let request_line = request.lines().next().unwrap_or("");
                let mut parts = request_line.split_whitespace();
                let method = parts.next().unwrap_or("");
                let path = parts.next().unwrap_or("");

                let response = if method == "POST" && path.starts_with("/register") {
                    match register_client_id {
                        Some(client_id) => {
                            let body =
                                format!(r#"{{"client_id":"{client_id}","redirect_uris":[]}}"#);
                            format!(
                                "HTTP/1.1 201 Created\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                                body.len(),
                                body
                            )
                        }
                        None => {
                            let body = r#"{"error":"invalid_request"}"#;
                            format!(
                                "HTTP/1.1 400 Bad Request\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                                body.len(),
                                body
                            )
                        }
                    }
                } else if method == "POST" && path.starts_with("/token") {
                    let body = r#"{"access_token":"test-access-token","token_type":"bearer","expires_in":3600,"refresh_token":"test-refresh-token","scope":"mcp"}"#;
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    )
                } else {
                    let body = "not found";
                    format!(
                        "HTTP/1.1 404 Not Found\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    )
                };
                let _ = stream.write_all(response.as_bytes());
            }
        });

        format!("http://{}", addr)
    }

    /// Like [`spawn_fake_oauth_server`], but serves real RFC 8414
    /// authorization-server metadata (so `additional_fields` is populated
    /// rather than synthesized by `rmcp`'s legacy fallback) with no
    /// `registration_endpoint` — i.e. a server that supports CIMD instead of
    /// DCR. `token_endpoint_auth_methods` is caller-controlled so a test can
    /// cover the "advertises CIMD but won't accept a public client" case.
    fn spawn_fake_cimd_oauth_server(
        cimd_supported: bool,
        token_endpoint_auth_methods: &'static str,
    ) -> String {
        use std::io::{Read, Write};
        use std::net::TcpListener as StdTcpListener;

        let listener = StdTcpListener::bind("127.0.0.1:0").expect("bind fake cimd oauth server");
        let addr = listener.local_addr().unwrap();

        std::thread::spawn(move || {
            let issuer = format!("http://{addr}");
            while let Ok((mut stream, _)) = listener.accept() {
                let mut buf = [0u8; 8192];
                let n = match stream.read(&mut buf) {
                    Ok(n) => n,
                    Err(_) => continue,
                };
                let request = String::from_utf8_lossy(&buf[..n]);
                let request_line = request.lines().next().unwrap_or("");
                let mut parts = request_line.split_whitespace();
                let method = parts.next().unwrap_or("");
                let path = parts.next().unwrap_or("");

                let response = if method == "GET"
                    && path.starts_with("/.well-known/oauth-authorization-server")
                {
                    let body = format!(
                        r#"{{"issuer":"{issuer}","authorization_endpoint":"{issuer}/authorize","token_endpoint":"{issuer}/token","response_types_supported":["code"],"code_challenge_methods_supported":["S256"],"token_endpoint_auth_methods_supported":{token_endpoint_auth_methods},"client_id_metadata_document_supported":{cimd_supported}}}"#
                    );
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    )
                } else if method == "POST" && path.starts_with("/token") {
                    let body = r#"{"access_token":"test-access-token","token_type":"bearer","expires_in":3600,"refresh_token":"test-refresh-token","scope":"mcp"}"#;
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    )
                } else {
                    let body = "not found";
                    format!(
                        "HTTP/1.1 404 Not Found\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    )
                };
                let _ = stream.write_all(response.as_bytes());
            }
        });

        format!("http://{}", addr)
    }

    /// Like [`spawn_fake_oauth_server`], but `POST /register` fails with a
    /// transient-looking `500` on its first call and only succeeds (`201`,
    /// with `register_client_id`) from the second call onward — simulates
    /// exactly the "momentary blip on a server that really does support DCR"
    /// scenario [`register_client_with_retry`] exists to paper over.
    fn spawn_flaky_then_ok_oauth_server(register_client_id: &'static str) -> String {
        use std::io::{Read, Write};
        use std::net::TcpListener as StdTcpListener;
        use std::sync::atomic::{AtomicU32, Ordering};

        let listener = StdTcpListener::bind("127.0.0.1:0").expect("bind fake oauth server");
        let addr = listener.local_addr().unwrap();
        let register_attempts = std::sync::Arc::new(AtomicU32::new(0));

        std::thread::spawn(move || {
            while let Ok((mut stream, _)) = listener.accept() {
                let mut buf = [0u8; 8192];
                let n = match stream.read(&mut buf) {
                    Ok(n) => n,
                    Err(_) => continue,
                };
                let request = String::from_utf8_lossy(&buf[..n]);
                let request_line = request.lines().next().unwrap_or("");
                let mut parts = request_line.split_whitespace();
                let method = parts.next().unwrap_or("");
                let path = parts.next().unwrap_or("");

                let response = if method == "POST" && path.starts_with("/register") {
                    let attempt = register_attempts.fetch_add(1, Ordering::SeqCst);
                    if attempt == 0 {
                        let body = r#"{"error":"internal_error"}"#;
                        format!(
                            "HTTP/1.1 500 Internal Server Error\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                            body.len(),
                            body
                        )
                    } else {
                        let body =
                            format!(r#"{{"client_id":"{register_client_id}","redirect_uris":[]}}"#);
                        format!(
                            "HTTP/1.1 201 Created\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                            body.len(),
                            body
                        )
                    }
                } else if method == "POST" && path.starts_with("/token") {
                    let body = r#"{"access_token":"test-access-token","token_type":"bearer","expires_in":3600,"refresh_token":"test-refresh-token","scope":"mcp"}"#;
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    )
                } else {
                    let body = "not found";
                    format!(
                        "HTTP/1.1 404 Not Found\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    )
                };
                let _ = stream.write_all(response.as_bytes());
            }
        });

        format!("http://{}", addr)
    }

    #[tokio::test]
    async fn prepare_authorization_recovers_from_a_transient_registration_failure_via_retry() {
        let base_url = spawn_flaky_then_ok_oauth_server("dcr-client-id-after-retry");
        let mut manager = AuthorizationManager::new(&base_url).await.unwrap();
        manager.set_credential_store(KeychainCredentialStore::new(unique_server_id("flaky-dcr")));

        // No `manual_client` given — this only succeeds if the first (500)
        // registration failure was retried rather than immediately treated as
        // "DCR unsupported".
        let auth_url =
            prepare_authorization(&mut manager, &base_url, "http://127.0.0.1:9999/", None)
                .await
                .unwrap()
                .authorize_url;
        assert!(
            auth_url.contains("client_id=dcr-client-id-after-retry"),
            "unexpected auth url: {auth_url}"
        );
    }

    #[tokio::test]
    async fn prepare_authorization_uses_dynamic_client_registration_when_supported() {
        let base_url = spawn_fake_oauth_server(Some("dcr-client-id"));
        let mut manager = AuthorizationManager::new(&base_url).await.unwrap();
        manager.set_credential_store(KeychainCredentialStore::new(unique_server_id("dcr")));

        let prepared =
            prepare_authorization(&mut manager, &base_url, "http://127.0.0.1:9999/", None)
                .await
                .unwrap();
        let auth_url = prepared.authorize_url;
        assert!(
            auth_url.contains("client_id=dcr-client-id"),
            "unexpected auth url: {auth_url}"
        );
        assert!(
            auth_url.contains("code_challenge="),
            "the authorize URL must carry a PKCE code_challenge: {auth_url}"
        );
        assert!(
            prepared.manual_client.is_none(),
            "a dynamically registered client must not be persisted as the user's own"
        );
    }

    #[test]
    fn cimd_client_id_is_an_https_url_with_a_path_as_the_spec_requires() {
        let parsed = url::Url::parse(CIMD_CLIENT_ID).unwrap();
        assert_eq!(parsed.scheme(), "https");
        assert_ne!(
            parsed.path(),
            "/",
            "a CIMD client_id must have a path component"
        );
    }

    /// The document the authorization server fetches has to allow the redirect
    /// URI this app actually sends (port waived per RFC 8252 §7.3). That
    /// document lives in the website repo, so this pins our half of the
    /// contract: root path on loopback, which
    /// `public/oauth/client-metadata.json` declares for both `127.0.0.1` and
    /// `localhost`.
    #[tokio::test]
    async fn loopback_redirect_matches_a_redirect_uri_declared_in_the_cimd_document() {
        let declared = [
            "http://127.0.0.1/",
            "http://localhost/",
            "http://127.0.0.1/callback",
            "http://localhost/callback",
        ];
        let listener = LoopbackListener::bind_for(
            "cimd-contract",
            "https://mcp.example.com/mcp",
            LoopbackPortPolicy::AllowEphemeralFallback,
        )
        .await
        .unwrap();
        let sent = url::Url::parse(&listener.redirect_uri).unwrap();
        assert!(
            declared.iter().any(|candidate| {
                let candidate = url::Url::parse(candidate).unwrap();
                candidate.scheme() == sent.scheme()
                    && candidate.host_str() == sent.host_str()
                    && candidate.path() == sent.path()
            }),
            "redirect_uri {} matches no entry the published CIMD document declares",
            listener.redirect_uri
        );
    }

    #[test]
    fn cimd_support_requires_both_the_capability_flag_and_public_client_auth() {
        fn metadata(fields: serde_json::Value) -> rmcp::transport::auth::AuthorizationMetadata {
            let mut metadata = rmcp::transport::auth::AuthorizationMetadata::default();
            for (key, value) in fields.as_object().unwrap() {
                metadata
                    .additional_fields
                    .insert(key.clone(), value.clone());
            }
            metadata
        }

        assert!(authorization_server_supports_cimd(&metadata(
            serde_json::json!({
                "client_id_metadata_document_supported": true,
                "token_endpoint_auth_methods_supported": ["none", "client_secret_post"],
            })
        )));

        // Advertises CIMD but authenticates clients at the token endpoint — this
        // app has no secret to authenticate a CIMD client with, so it must not
        // take that path.
        assert!(!authorization_server_supports_cimd(&metadata(
            serde_json::json!({
                "client_id_metadata_document_supported": true,
                "token_endpoint_auth_methods_supported": ["client_secret_basic"],
            })
        )));

        // RFC 8414's default is `client_secret_basic`, so an absent list is not
        // an invitation to send a public client.
        assert!(!authorization_server_supports_cimd(&metadata(
            serde_json::json!({
                "client_id_metadata_document_supported": true,
            })
        )));

        assert!(!authorization_server_supports_cimd(&metadata(
            serde_json::json!({
                "token_endpoint_auth_methods_supported": ["none"],
            })
        )));
        assert!(!authorization_server_supports_cimd(
            &rmcp::transport::auth::AuthorizationMetadata::default()
        ));
    }

    #[tokio::test]
    async fn prepare_authorization_identifies_with_cimd_when_the_server_supports_it() {
        let base_url = spawn_fake_cimd_oauth_server(true, r#"["none"]"#);
        let mut manager = AuthorizationManager::new(&base_url).await.unwrap();
        manager.set_credential_store(KeychainCredentialStore::new(unique_server_id("cimd")));

        let prepared =
            prepare_authorization(&mut manager, &base_url, "http://127.0.0.1:9999/", None)
                .await
                .unwrap();
        let authorize_url = url::Url::parse(&prepared.authorize_url).unwrap();
        let client_id = authorize_url
            .query_pairs()
            .find(|(key, _)| key == "client_id")
            .map(|(_, value)| value.into_owned())
            .expect("the authorize URL must carry a client_id");
        assert_eq!(client_id, CIMD_CLIENT_ID);
        assert!(
            prepared.manual_client.is_none(),
            "a CIMD client is not a user-supplied registration and must not be persisted as one"
        );
    }

    /// Spec priority order (MCP authorization 2025-11-25): a client the user
    /// pre-registered themselves wins over CIMD, so someone who deliberately
    /// pasted their own OAuth app keeps using it.
    #[tokio::test]
    async fn a_user_supplied_client_takes_priority_over_cimd() {
        let base_url = spawn_fake_cimd_oauth_server(true, r#"["none"]"#);
        let mut manager = AuthorizationManager::new(&base_url).await.unwrap();
        manager.set_credential_store(KeychainCredentialStore::new(unique_server_id("cimd-byo")));

        let prepared = prepare_authorization(
            &mut manager,
            &base_url,
            "http://127.0.0.1:9999/",
            Some(&ManualClientRegistration {
                client_id: "user-registered-client".to_string(),
                client_secret: None,
            }),
        )
        .await
        .unwrap();
        assert!(
            prepared
                .authorize_url
                .contains("client_id=user-registered-client"),
            "unexpected authorize url: {}",
            prepared.authorize_url
        );
    }

    /// A server advertising CIMD it can't actually honor for a public client
    /// must not silently produce a doomed authorize URL — with no DCR either,
    /// the flow falls through to asking the user, same as any other
    /// no-registration server.
    #[tokio::test]
    async fn a_cimd_server_that_rejects_public_clients_falls_through_to_asking_the_user() {
        let base_url = spawn_fake_cimd_oauth_server(true, r#"["client_secret_basic"]"#);
        let mut manager = AuthorizationManager::new(&base_url).await.unwrap();
        manager.set_credential_store(KeychainCredentialStore::new(unique_server_id("cimd-conf")));

        let err = prepare_authorization(&mut manager, &base_url, "http://127.0.0.1:9999/", None)
            .await
            .unwrap_err();
        assert_eq!(err, CLIENT_ID_REQUIRED_MESSAGE);
    }

    #[tokio::test]
    async fn prepare_authorization_requires_a_client_id_when_dcr_is_unsupported() {
        let base_url = spawn_fake_oauth_server(None);
        let mut manager = AuthorizationManager::new(&base_url).await.unwrap();
        manager.set_credential_store(KeychainCredentialStore::new(unique_server_id("no-dcr")));

        let err = prepare_authorization(&mut manager, &base_url, "http://127.0.0.1:9999/", None)
            .await
            .unwrap_err();
        assert_eq!(err, CLIENT_ID_REQUIRED_MESSAGE);
    }

    #[tokio::test]
    async fn prepare_authorization_falls_back_to_a_supplied_client_id_when_dcr_is_unsupported() {
        let base_url = spawn_fake_oauth_server(None);
        let mut manager = AuthorizationManager::new(&base_url).await.unwrap();
        manager.set_credential_store(KeychainCredentialStore::new(unique_server_id(
            "byo-client-id",
        )));

        let prepared = prepare_authorization(
            &mut manager,
            &base_url,
            "http://127.0.0.1:9999/",
            Some(&ManualClientRegistration {
                client_id: "  byo-client-id  ".to_string(),
                client_secret: Some("  byo-client-secret  ".to_string()),
            }),
        )
        .await
        .unwrap();
        assert!(
            prepared.authorize_url.contains("client_id=byo-client-id"),
            "unexpected auth url: {}",
            prepared.authorize_url
        );
        // The secret is never a query param — it belongs to the token
        // exchange — but it must come back for the caller to persist, trimmed.
        assert!(
            !prepared.authorize_url.contains("byo-client-secret"),
            "the client secret must never appear in an authorize URL: {}",
            prepared.authorize_url
        );
        let registration = prepared
            .manual_client
            .expect("a user-supplied client must be reported back for persistence");
        assert_eq!(registration.client_id, "byo-client-id");
        assert_eq!(
            registration.client_secret.as_deref(),
            Some("byo-client-secret")
        );
    }

    /// A blank-but-present client secret (the field left empty in Settings for
    /// a public PKCE client) must be stored as `None`, not as `Some("")` — an
    /// empty `client_secret` sent to a token endpoint is a different, and
    /// rejected, request from sending none at all.
    #[tokio::test]
    async fn prepare_authorization_treats_a_blank_client_secret_as_absent() {
        let base_url = spawn_fake_oauth_server(None);
        let mut manager = AuthorizationManager::new(&base_url).await.unwrap();
        manager.set_credential_store(KeychainCredentialStore::new(unique_server_id(
            "blank-secret",
        )));

        let prepared = prepare_authorization(
            &mut manager,
            &base_url,
            "http://127.0.0.1:9999/",
            Some(&ManualClientRegistration {
                client_id: "public-client".to_string(),
                client_secret: Some("   ".to_string()),
            }),
        )
        .await
        .unwrap();
        assert_eq!(
            prepared.manual_client.and_then(|c| c.client_secret),
            None,
            "a whitespace-only secret must be treated as no secret"
        );
    }

    #[test]
    fn manual_client_registration_roundtrips_via_the_keychain_and_is_cleared_on_disconnect() {
        let server_id = unique_server_id("manual-client");
        assert!(load_manual_client(&server_id).unwrap().is_none());

        save_manual_client(
            &server_id,
            &ManualClientRegistration {
                client_id: "own-client-id".to_string(),
                client_secret: Some("own-client-secret".to_string()),
            },
        )
        .unwrap();
        let loaded = load_manual_client(&server_id).unwrap().unwrap();
        assert_eq!(loaded.client_id, "own-client-id");
        assert_eq!(loaded.client_secret.as_deref(), Some("own-client-secret"));

        // Disconnecting clears the registration along with the credentials, so
        // a later connect starts from a clean slate rather than silently
        // reusing a client the user may have revoked.
        remove_oauth_credentials_impl(&server_id).unwrap();
        assert!(load_manual_client(&server_id).unwrap().is_none());
    }

    #[test]
    fn manual_client_persistence_is_transactional_with_token_exchange() {
        let server_id = unique_server_id("manual-client-transaction");
        let records = std::sync::Arc::new(InMemoryOAuthRecordStore::default());
        let previous = ManualClientRegistration {
            client_id: "previous-valid-client".to_string(),
            client_secret: Some("previous-valid-secret".to_string()),
        };
        let pending = ManualClientRegistration {
            client_id: "replacement-client".to_string(),
            client_secret: Some("replacement-secret".to_string()),
        };
        records
            .save_manual_client(&server_id, &serde_json::to_string(&previous).unwrap())
            .unwrap();
        records
            .save_credentials(&server_id, "previous-token-record")
            .unwrap();

        let staged =
            StagedOAuthExchange::begin_with_store(&server_id, Some(&pending), records.clone())
                .unwrap();
        let staged_record = load_test_manual_client(&records, &server_id);
        assert_eq!(
            staged_record.client_id, pending.client_id,
            "the matching client must be durable before token exchange can save a token"
        );
        records
            .save_credentials(&server_id, "rejected-token-record")
            .unwrap();

        let error = complete_oauth_exchange::<()>(staged, Err("token exchange rejected".into()))
            .unwrap_err();
        assert_eq!(error, "token exchange rejected");
        let still_saved = load_test_manual_client(&records, &server_id);
        assert_eq!(still_saved.client_id, previous.client_id);
        assert_eq!(
            still_saved.client_secret.as_deref(),
            previous.client_secret.as_deref()
        );
        assert_eq!(
            records.load_credentials(&server_id).unwrap().as_deref(),
            Some("previous-token-record")
        );

        let staged =
            StagedOAuthExchange::begin_with_store(&server_id, Some(&pending), records.clone())
                .unwrap();
        records
            .save_credentials(&server_id, "committed-token-record")
            .unwrap();
        complete_oauth_exchange(staged, Ok(())).unwrap();
        let committed = load_test_manual_client(&records, &server_id);
        assert_eq!(committed.client_id, pending.client_id);
        assert_eq!(
            committed.client_secret.as_deref(),
            pending.client_secret.as_deref()
        );
        assert_eq!(
            records.load_credentials(&server_id).unwrap().as_deref(),
            Some("committed-token-record")
        );

        let cancelled = ManualClientRegistration {
            client_id: "cancelled-replacement".to_string(),
            client_secret: None,
        };
        let staged =
            StagedOAuthExchange::begin_with_store(&server_id, Some(&cancelled), records.clone())
                .unwrap();
        records
            .save_credentials(&server_id, "cancelled-token-record")
            .unwrap();
        let cancellation =
            complete_oauth_exchange::<()>(staged, Err(OAUTH_CANCELLED_MESSAGE.to_string()))
                .unwrap_err();
        assert_eq!(cancellation, OAUTH_CANCELLED_MESSAGE);
        let restored_after_cancel = load_test_manual_client(&records, &server_id);
        assert_eq!(restored_after_cancel.client_id, pending.client_id);
        assert_eq!(
            records.load_credentials(&server_id).unwrap().as_deref(),
            Some("committed-token-record"),
            "normal cancellation must explicitly restore the committed token"
        );

        let staged =
            StagedOAuthExchange::begin_with_store(&server_id, Some(&cancelled), records.clone())
                .unwrap();
        records
            .save_credentials(&server_id, "aborted-token-record")
            .unwrap();
        drop(staged);
        let restored_after_drop = load_test_manual_client(&records, &server_id);
        assert_eq!(
            restored_after_drop.client_id, pending.client_id,
            "aborting the command future must still restore the last committed client"
        );
        assert_eq!(
            records.load_credentials(&server_id).unwrap().as_deref(),
            Some("committed-token-record"),
            "the Drop fallback must restore the committed token too"
        );
    }

    #[test]
    fn manual_client_keychain_account_is_namespaced_apart_from_credentials_and_tokens() {
        assert_eq!(
            manual_client_keychain_account("srv"),
            "mcp-oauth-client:srv"
        );
        assert_ne!(
            manual_client_keychain_account("srv"),
            keychain_account("srv")
        );
    }

    #[test]
    fn google_authorize_urls_get_offline_access_params_so_a_refresh_token_is_issued() {
        let url = with_extra_authorize_params(
            "https://accounts.google.com/o/oauth2/v2/auth?client_id=x&state=y".to_string(),
        );
        let parsed = url::Url::parse(&url).unwrap();
        let params: std::collections::HashMap<String, String> = parsed
            .query_pairs()
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect();
        assert_eq!(
            params.get("access_type").map(String::as_str),
            Some("offline")
        );
        assert_eq!(params.get("prompt").map(String::as_str), Some("consent"));
        assert_eq!(params.get("client_id").map(String::as_str), Some("x"));
        assert_eq!(params.get("state").map(String::as_str), Some("y"));
    }

    #[test]
    fn extra_authorize_params_never_override_what_the_authorization_server_already_set() {
        let url = with_extra_authorize_params(
            "https://accounts.google.com/o/oauth2/v2/auth?prompt=select_account".to_string(),
        );
        assert!(
            url.contains("prompt=select_account"),
            "unexpected url: {url}"
        );
        assert!(!url.contains("prompt=consent"), "unexpected url: {url}");
    }

    /// Google's Gmail MCP endpoint advertises five nested scopes (verified
    /// against the live endpoint); asking for all of them would over-request
    /// full-account access *and* mix in `gmail.metadata`, which Google
    /// documents as not combinable with the other Gmail scopes.
    #[test]
    fn google_mcp_scopes_are_narrowed_to_the_one_the_tools_actually_need() {
        let advertised = vec![
            "https://mail.google.com/".to_string(),
            "https://www.googleapis.com/auth/gmail.modify".to_string(),
            "https://www.googleapis.com/auth/gmail.compose".to_string(),
            "https://www.googleapis.com/auth/gmail.readonly".to_string(),
            "https://www.googleapis.com/auth/gmail.metadata".to_string(),
        ];
        assert_eq!(
            scopes_to_request("https://gmailmcp.googleapis.com/mcp/v1", advertised),
            vec!["https://www.googleapis.com/auth/gmail.modify".to_string()]
        );
    }

    #[test]
    fn scope_narrowing_defers_to_the_server_when_its_advertised_scopes_change() {
        // The preferred scope is no longer offered (a provider reorganization):
        // request what the server does advertise rather than a scope it just
        // said it doesn't support.
        let advertised = vec!["https://mail.google.com/".to_string()];
        assert_eq!(
            scopes_to_request("https://gmailmcp.googleapis.com/mcp/v1", advertised.clone()),
            advertised
        );
    }

    #[test]
    fn scope_narrowing_leaves_every_other_server_alone() {
        let advertised = vec!["mcp".to_string(), "offline_access".to_string()];
        assert_eq!(
            scopes_to_request("https://mcp.notion.com/mcp", advertised.clone()),
            advertised
        );
        // A host that merely *contains* a known one must not match.
        assert_eq!(
            scopes_to_request(
                "https://gmailmcp.googleapis.com.evil.example/mcp",
                advertised.clone()
            ),
            advertised
        );
    }

    #[test]
    fn authorize_urls_for_other_hosts_are_left_untouched() {
        let original = "https://mcp.notion.com/authorize?client_id=x".to_string();
        assert_eq!(with_extra_authorize_params(original.clone()), original);
        // A host that merely *contains* a known one must not match either.
        let lookalike = "https://accounts.google.com.evil.example/auth?client_id=x".to_string();
        assert_eq!(with_extra_authorize_params(lookalike.clone()), lookalike);
    }

    #[tokio::test]
    async fn full_authorization_code_exchange_persists_credentials_via_the_keychain_store() {
        let base_url = spawn_fake_oauth_server(Some("dcr-client-id"));
        let server_id = unique_server_id("full-exchange");
        let mut manager = AuthorizationManager::new(&base_url).await.unwrap();
        manager.set_credential_store(KeychainCredentialStore::new(server_id.clone()));

        let auth_url =
            prepare_authorization(&mut manager, &base_url, "http://127.0.0.1:9999/", None)
                .await
                .unwrap()
                .authorize_url;
        let csrf = url::Url::parse(&auth_url)
            .unwrap()
            .query_pairs()
            .find(|(k, _)| k == "state")
            .map(|(_, v)| v.into_owned())
            .expect("the authorize url must include a state param");

        manager
            .exchange_code_for_token("test-code", &csrf)
            .await
            .unwrap();

        let stored = KeychainCredentialStore::new(server_id.clone())
            .load()
            .await
            .unwrap()
            .unwrap();
        assert_eq!(stored.client_id, "dcr-client-id");
        assert!(stored.token_response.is_some());

        KeychainCredentialStore::new(server_id)
            .clear()
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn exchange_accepts_an_absent_issuer_when_the_server_does_not_require_it() {
        let base_url = spawn_fake_cimd_oauth_server(true, r#"["none"]"#);
        let server_id = unique_server_id("optional-issuer");
        let mut manager = AuthorizationManager::new(&base_url).await.unwrap();
        manager.set_credential_store(KeychainCredentialStore::new(server_id.clone()));

        let auth_url =
            prepare_authorization(&mut manager, &base_url, "http://127.0.0.1:9999/", None)
                .await
                .unwrap()
                .authorize_url;
        let csrf = url::Url::parse(&auth_url)
            .unwrap()
            .query_pairs()
            .find_map(|(key, value)| (key == "state").then(|| value.into_owned()))
            .unwrap();

        manager
            .exchange_code_for_token_with_issuer("test-code", &csrf, None)
            .await
            .expect("older authorization servers may omit RFC 9207 iss");

        KeychainCredentialStore::new(server_id)
            .clear()
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn exchange_rejects_a_callback_issuer_that_does_not_match_discovery() {
        let base_url = spawn_fake_cimd_oauth_server(true, r#"["none"]"#);
        let mut manager = AuthorizationManager::new(&base_url).await.unwrap();
        manager.set_credential_store(KeychainCredentialStore::new(unique_server_id(
            "mismatched-issuer",
        )));

        let auth_url =
            prepare_authorization(&mut manager, &base_url, "http://127.0.0.1:9999/", None)
                .await
                .unwrap()
                .authorize_url;
        let csrf = url::Url::parse(&auth_url)
            .unwrap()
            .query_pairs()
            .find_map(|(key, value)| (key == "state").then(|| value.into_owned()))
            .unwrap();

        let error = manager
            .exchange_code_for_token_with_issuer(
                "test-code",
                &csrf,
                Some("https://attacker.example"),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(error, AuthError::AuthorizationServerMismatch { .. }),
            "unexpected error: {error}"
        );
    }

    /// Exercises the `StateStore`'s CSRF round trip from the opposite
    /// direction: a token exchange presenting a CSRF token that was never
    /// issued by `get_authorization_url` (i.e. not present in the — in
    /// this attempt's process-local, in-memory — state store at all) must
    /// be rejected rather than silently proceeding.
    #[tokio::test]
    async fn exchange_rejects_a_csrf_token_that_was_never_issued() {
        let base_url = spawn_fake_oauth_server(Some("dcr-client-id"));
        let mut manager = AuthorizationManager::new(&base_url).await.unwrap();
        manager.set_credential_store(KeychainCredentialStore::new(unique_server_id(
            "forged-csrf",
        )));
        let _auth_url =
            prepare_authorization(&mut manager, &base_url, "http://127.0.0.1:9999/", None)
                .await
                .unwrap();

        let err = manager
            .exchange_code_for_token("test-code", "forged-csrf-token-never-issued")
            .await
            .unwrap_err();
        let message = format!("{err}");
        assert!(
            message.contains("state") || message.contains("Authorization"),
            "unexpected error: {message}"
        );
    }
}
