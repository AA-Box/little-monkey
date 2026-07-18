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
//! 3. [`LoopbackListener::bind`] — reserves an ephemeral `127.0.0.1` port
//!    for the redirect URI before starting the flow, so it can be registered
//!    (DCR) or repeated back with the client-id fallback.
//! 4. [`prepare_authorization`] — dynamic client registration at that
//!    redirect URI, or (if the server doesn't support DCR)
//!    `client_id_override` configured against it instead; then builds the
//!    PKCE + CSRF authorization URL.
//! 5. The system browser is opened on that URL; the loopback listener
//!    awaits the single resulting redirect (bounded by
//!    [`LOOPBACK_TIMEOUT_SECS`]).
//! 6. `AuthorizationManager::exchange_code_for_token` — verifies the CSRF
//!    token against the (in-memory, single-attempt-scoped) `StateStore`,
//!    exchanges the code, and persists the result via
//!    [`KeychainCredentialStore`] (OS keychain, never a JSON file).
//!
//! Progress streams to the frontend via `mcp-oauth://status` events (mirrors
//! `mcp.rs`'s `mcp://status`), since steps 4-6 can take minutes — the user
//! has to actually complete browser consent.

use std::time::Duration;

use async_trait::async_trait;
use rmcp::transport::auth::{
    AuthError, AuthorizationManager, CredentialStore, OAuthClientConfig, StoredCredentials,
};
use tauri::Emitter;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use crate::AppState;

/// Same keychain service every credential in this app lives under (see
/// `mcp.rs`/`providers.rs`) — entries are disambiguated by *account*, not
/// service, hence this module's own `mcp-oauth:<id>` account prefix.
const KEYCHAIN_SERVICE: &str = "com.littlemonkey.app";

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

/// The keychain *account* name under which server `id`'s OAuth-derived
/// credentials (refresh token, last access token, granted scopes, client id)
/// are stored — `mcp-oauth:<id>`, distinct from `mcp.rs`'s own `mcp:<id>`
/// manual-bearer-token accounts in the same keychain service.
fn keychain_account(server_id: &str) -> String {
    format!("mcp-oauth:{}", server_id)
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
        keyring::Entry::new(KEYCHAIN_SERVICE, &keychain_account(&self.server_id))
            .map_err(|e| AuthError::InternalError(format!("Failed to access keychain: {e}")))
    }
}

#[async_trait]
impl CredentialStore for KeychainCredentialStore {
    async fn load(&self) -> Result<Option<StoredCredentials>, AuthError> {
        let entry = self.entry()?;
        match entry.get_password() {
            Ok(json) => serde_json::from_str(&json)
                .map(Some)
                .map_err(|e| AuthError::InternalError(format!("Corrupt stored OAuth credentials: {e}"))),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(AuthError::InternalError(format!(
                "Failed to read OAuth credentials from keychain: {e}"
            ))),
        }
    }

    async fn save(&self, credentials: StoredCredentials) -> Result<(), AuthError> {
        let entry = self.entry()?;
        let json = serde_json::to_string(&credentials)
            .map_err(|e| AuthError::InternalError(format!("Failed to serialize OAuth credentials: {e}")))?;
        entry
            .set_password(&json)
            .map_err(|e| AuthError::InternalError(format!("Failed to save OAuth credentials to keychain: {e}")))
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
    keyring::Entry::new(KEYCHAIN_SERVICE, &keychain_account(server_id))
        .ok()
        .and_then(|e| e.get_password().ok())
        .is_some()
}

/// Core remove-credentials logic behind [`mcp_oauth_disconnect`], also
/// called (best-effort) by `mcp::mcp_remove_server` so deleting a server
/// never leaves an orphaned OAuth credential behind — same reasoning as
/// `mcp::remove_http_token_impl`. A missing entry is a no-op success.
pub(crate) fn remove_oauth_credentials_impl(server_id: &str) -> Result<(), String> {
    let entry = keyring::Entry::new(KEYCHAIN_SERVICE, &keychain_account(server_id))
        .map_err(|e| format!("Failed to access keychain: {e}"))?;
    match entry.delete_credential() {
        Ok(()) => Ok(()),
        Err(keyring::Error::NoEntry) => Ok(()),
        Err(e) => Err(format!("Failed to remove saved OAuth credentials: {e}")),
    }
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
fn refresh_lock_for(state: &AppState, server_id: &str) -> std::sync::Arc<tokio::sync::Mutex<()>> {
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
    match manager.register_client("Little Monkey", redirect_uri, &[]).await {
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

/// Discovers authorization server metadata, then either dynamically
/// registers an OAuth client at `redirect_uri` (RFC 7591, retried once on
/// failure — see [`register_client_with_retry`]) or — if that still fails —
/// configures `client_id_override` against it instead, and returns the
/// resulting PKCE + CSRF authorization URL to send the user to. AppHandle-free
/// and directly unit-testable (see the tests below); the `#[tauri::command]`
/// wrapper ([`mcp_oauth_connect`]) only adds progress events, opening the
/// system browser, and awaiting the loopback callback around this.
pub(crate) async fn prepare_authorization(
    manager: &mut AuthorizationManager,
    redirect_uri: &str,
    client_id_override: Option<&str>,
) -> Result<String, String> {
    let metadata = manager.discover_metadata().await.map_err(auth_err)?;
    manager.set_metadata(metadata);

    if register_client_with_retry(manager, redirect_uri).await.is_err() {
        let client_id = client_id_override
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| CLIENT_ID_REQUIRED_MESSAGE.to_string())?;
        // `OAuthClientConfig::scopes` isn't read by `configure_client` itself
        // (it only feeds the actual authorize request, computed via
        // `select_scopes` below) — left empty here rather than computed
        // twice.
        let config = OAuthClientConfig::new(client_id, redirect_uri);
        manager.configure_client(config).map_err(auth_err)?;
    }

    let scopes = manager.select_scopes(None, &[]);
    let scope_refs: Vec<&str> = scopes.iter().map(String::as_str).collect();
    manager
        .get_authorization_url(&scope_refs)
        .await
        .map_err(auth_err)
}

/// Query parameters extracted from the single HTTP request the loopback
/// listener accepts — a subset of what an OAuth authorization-code redirect
/// carries (`code`/`state` on success, `error`/`error_description` if the
/// user denied consent or the authorization server rejected the request).
#[derive(Debug, Default, Clone)]
pub(crate) struct CallbackParams {
    pub code: Option<String>,
    pub state: Option<String>,
    pub error: Option<String>,
    pub error_description: Option<String>,
}

/// A single-use, loopback-only HTTP listener that receives one OAuth
/// authorization redirect and then shuts down — reserved (via
/// [`LoopbackListener::bind`]) before the authorization URL is even built, so
/// its ephemeral port can be embedded as the `redirect_uri` sent to the
/// authorization server (both for dynamic client registration and the
/// authorize request itself).
pub(crate) struct LoopbackListener {
    listener: TcpListener,
    /// `http://127.0.0.1:<port>/callback` — the redirect URI to register
    /// and request authorization against.
    pub redirect_uri: String,
}

impl LoopbackListener {
    /// Binds to an OS-assigned ephemeral port on `127.0.0.1` specifically
    /// (never `0.0.0.0`/`::`) — this listener must only ever be reachable
    /// from the same machine's browser, not the network.
    pub async fn bind() -> Result<Self, String> {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .map_err(|e| format!("Failed to bind local OAuth callback listener: {e}"))?;
        let port = listener
            .local_addr()
            .map_err(|e| format!("Failed to read local OAuth callback listener's address: {e}"))?
            .port();
        Ok(Self {
            listener,
            redirect_uri: format!("http://127.0.0.1:{port}/callback"),
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
        // — the redirect is a bare `GET /callback?...`, no body.
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

/// Parses `GET /callback?code=...&state=...&error=... HTTP/1.1` out of a raw
/// request buffer's first line. Returns `None` only if the request line
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

/// Runs the actual discover -> register-or-client-id -> authorize ->
/// (browser) -> loopback callback -> exchange pipeline for one connect
/// attempt. Split out from [`mcp_oauth_connect`] so that command's body is
/// just "set up cancellation bookkeeping around this".
async fn run_connect_flow(
    app: &tauri::AppHandle,
    server_id: &str,
    base_url: &str,
    client_id: Option<String>,
) -> Result<(), String> {
    emit_progress(app, server_id, "discovering", None);
    let mut manager = AuthorizationManager::new(base_url).await.map_err(auth_err)?;
    manager.set_credential_store(KeychainCredentialStore::new(server_id.to_string()));

    let listener = LoopbackListener::bind().await?;
    let redirect_uri = listener.redirect_uri.clone();

    let auth_url = match prepare_authorization(&mut manager, &redirect_uri, client_id.as_deref())
        .await
    {
        Ok(url) => url,
        Err(message) if message == CLIENT_ID_REQUIRED_MESSAGE => {
            emit_progress(app, server_id, "needs_client_id", Some(message.clone()));
            return Err(message);
        }
        Err(message) => {
            emit_progress(app, server_id, "error", Some(message.clone()));
            return Err(message);
        }
    };

    emit_progress(app, server_id, "opening_browser", None);
    {
        use tauri_plugin_opener::OpenerExt;
        app.opener()
            .open_url(auth_url, None::<String>)
            .map_err(|e| format!("Failed to open the system browser for OAuth: {e}"))?;
    }

    emit_progress(app, server_id, "waiting_for_browser", None);
    let callback = listener
        .await_callback(Duration::from_secs(LOOPBACK_TIMEOUT_SECS))
        .await?;

    if let Some(error) = callback.error {
        let message = match callback.error_description {
            Some(desc) => format!("OAuth authorization was not granted: {error} ({desc})"),
            None => format!("OAuth authorization was not granted: {error}"),
        };
        emit_progress(app, server_id, "error", Some(message.clone()));
        return Err(message);
    }

    let code = callback
        .code
        .ok_or_else(|| "The OAuth callback did not include an authorization code".to_string())?;
    let csrf = callback
        .state
        .ok_or_else(|| "The OAuth callback did not include the expected state parameter".to_string())?;

    emit_progress(app, server_id, "exchanging_token", None);
    manager
        .exchange_code_for_token(&code, &csrf)
        .await
        .map_err(|e| {
            let message = auth_err(e);
            emit_progress(app, server_id, "error", Some(message.clone()));
            message
        })?;

    emit_progress(app, server_id, "connected", None);
    Ok(())
}

/// Runs a full OAuth 2.0 connect for HTTP MCP server `server_id`: discovery,
/// dynamic client registration (or `client_id` as a fallback when the
/// server doesn't support DCR), opening the system browser, awaiting the
/// loopback redirect, exchanging the code, and saving the resulting
/// credentials to the OS keychain. Streams progress via `mcp-oauth://status`
/// events (see [`emit_progress`]) so the Settings UI isn't left frozen for
/// however long the user takes to complete browser consent — cancellable
/// mid-flight via [`mcp_oauth_cancel`].
///
/// Does not itself connect/reconnect the MCP server — `McpPanel.tsx`'s
/// `OAuthConnectSection` follows a successful resolution with the normal
/// `mcp_connect`, which then picks up the just-saved credentials via
/// `mcp::get_access_token_if_connected`.
///
/// Shares one `Notify` per server id across overlapping calls (via
/// `entry().or_insert_with()`, only removing the map entry once
/// `Arc::strong_count` confirms no other concurrent call for this id still
/// references it) rather than unconditionally inserting/removing — mirrors
/// `tools.rs`'s `tool_run_shell` cancel bookkeeping exactly, for the same
/// reason: two overlapping `mcp_oauth_connect` calls for the same
/// `server_id` (a double-click on "Connect via OAuth" before React disables
/// the button, or two Settings windows) must never let the second call's
/// unconditional `insert` silently orphan the first call's `Notify` from the
/// map (breaking `mcp_oauth_cancel` for it), nor let whichever call finishes
/// first unconditionally `remove` an entry the other call still needs in
/// order to ever be cancellable.
#[tauri::command(rename_all = "snake_case")]
pub async fn mcp_oauth_connect(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    server_id: String,
    client_id: Option<String>,
) -> Result<(), String> {
    crate::mcp::validate_id(&server_id)?;
    let base_url = resolve_http_base_url(&app, &server_id).await?;

    let cancel = {
        let mut guard = state
            .mcp_oauth_cancel
            .lock()
            .map_err(|_| "OAuth cancel lock poisoned".to_string())?;
        guard
            .entry(server_id.clone())
            .or_insert_with(|| std::sync::Arc::new(tokio::sync::Notify::new()))
            .clone()
    };

    let result = tokio::select! {
        outcome = run_connect_flow(&app, &server_id, &base_url, client_id) => outcome,
        _ = cancel.notified() => {
            emit_progress(&app, &server_id, "cancelled", None);
            Err("OAuth connection cancelled".to_string())
        }
    };

    // Drop this server id's cancel channel once no other concurrent
    // `mcp_oauth_connect` call for the same id still holds it (strong count
    // 2 = the map's own `Arc` + this call's local `cancel` clone) — same
    // bookkeeping as `tool_run_shell`/`mcp_call_tool`. A racing new connect
    // for the same id simply recreates the entry via `or_insert_with` above.
    if let Ok(mut guard) = state.mcp_oauth_cancel.lock() {
        if guard
            .get(&server_id)
            .is_some_and(|n| std::sync::Arc::strong_count(n) <= 2)
        {
            guard.remove(&server_id);
        }
    }

    result
}

/// Cancels an in-flight [`mcp_oauth_connect`] for `server_id`, if one is
/// running. A no-op success if none is — the caller's desired end state
/// (no connect attempt in flight for this id) already holds.
#[tauri::command(rename_all = "snake_case")]
pub fn mcp_oauth_cancel(state: tauri::State<'_, AppState>, server_id: String) -> Result<(), String> {
    crate::mcp::validate_id(&server_id)?;
    let guard = state
        .mcp_oauth_cancel
        .lock()
        .map_err(|_| "OAuth cancel lock poisoned".to_string())?;
    if let Some(notify) = guard.get(&server_id) {
        notify.notify_waiters();
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

        let raw_entry = keyring::Entry::new(KEYCHAIN_SERVICE, &keychain_account(&server_id)).unwrap();
        let raw_json = raw_entry.get_password().unwrap();
        assert!(
            raw_json.contains("test-client-id"),
            "the keychain entry must hold the serialized credentials directly, not a pointer to a file"
        );

        store.clear().await.unwrap();
        assert!(store.load().await.unwrap().is_none());
        match raw_entry.get_password() {
            Err(keyring::Error::NoEntry) => {}
            other => panic!("expected the keychain entry to be gone after clear(), got {other:?}"),
        }
    }

    #[test]
    fn keychain_account_namespaces_oauth_credentials_separately_from_manual_tokens() {
        assert_eq!(keychain_account("my-server"), "mcp-oauth:my-server");
    }

    #[test]
    fn has_oauth_credentials_is_false_when_nothing_is_saved() {
        assert!(!has_oauth_credentials("never-configured-oauth-server-id-xyz"));
    }

    #[test]
    fn remove_of_unknown_credentials_is_a_no_op_success() {
        remove_oauth_credentials_impl(&unique_server_id("remove-missing")).unwrap();
    }

    // --- OAuth base URL scheme enforcement ---------------------------------

    #[test]
    fn secure_url_check_accepts_https() {
        ensure_oauth_base_url_is_secure("https://mcp.example.com").unwrap();
    }

    #[test]
    fn secure_url_check_rejects_plaintext_http_on_a_remote_host() {
        let err = ensure_oauth_base_url_is_secure("http://mcp.example.com").unwrap_err();
        assert!(
            err.contains("plaintext HTTP"),
            "unexpected error: {err}"
        );
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
    async fn concurrent_get_access_token_calls_for_the_same_server_id_are_serialized_not_parallel() {
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

    // --- LoopbackListener --------------------------------------------------

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
        let raw = b"GET /callback?error=access_denied&error_description=User+said+no&state=xyz HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n";
        let params = parse_callback_request(raw).unwrap();
        assert_eq!(params.error.as_deref(), Some("access_denied"));
        assert_eq!(params.error_description.as_deref(), Some("User said no"));
        assert_eq!(params.state.as_deref(), Some("xyz"));
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
        manager.set_credential_store(KeychainCredentialStore::new(unique_server_id(
            "flaky-dcr",
        )));

        // No `client_id_override` given — this only succeeds if the first
        // (500) registration failure was retried rather than immediately
        // treated as "DCR unsupported".
        let auth_url = prepare_authorization(&mut manager, "http://127.0.0.1:9999/callback", None)
            .await
            .unwrap();
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

        let auth_url = prepare_authorization(&mut manager, "http://127.0.0.1:9999/callback", None)
            .await
            .unwrap();
        assert!(
            auth_url.contains("client_id=dcr-client-id"),
            "unexpected auth url: {auth_url}"
        );
        assert!(
            auth_url.contains("code_challenge="),
            "the authorize URL must carry a PKCE code_challenge: {auth_url}"
        );
    }

    #[tokio::test]
    async fn prepare_authorization_requires_a_client_id_when_dcr_is_unsupported() {
        let base_url = spawn_fake_oauth_server(None);
        let mut manager = AuthorizationManager::new(&base_url).await.unwrap();
        manager.set_credential_store(KeychainCredentialStore::new(unique_server_id("no-dcr")));

        let err = prepare_authorization(&mut manager, "http://127.0.0.1:9999/callback", None)
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

        let auth_url = prepare_authorization(
            &mut manager,
            "http://127.0.0.1:9999/callback",
            Some("byo-client-id"),
        )
        .await
        .unwrap();
        assert!(
            auth_url.contains("client_id=byo-client-id"),
            "unexpected auth url: {auth_url}"
        );
    }

    #[tokio::test]
    async fn full_authorization_code_exchange_persists_credentials_via_the_keychain_store() {
        let base_url = spawn_fake_oauth_server(Some("dcr-client-id"));
        let server_id = unique_server_id("full-exchange");
        let mut manager = AuthorizationManager::new(&base_url).await.unwrap();
        manager.set_credential_store(KeychainCredentialStore::new(server_id.clone()));

        let auth_url = prepare_authorization(&mut manager, "http://127.0.0.1:9999/callback", None)
            .await
            .unwrap();
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

        KeychainCredentialStore::new(server_id).clear().await.unwrap();
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
        let _auth_url = prepare_authorization(&mut manager, "http://127.0.0.1:9999/callback", None)
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
