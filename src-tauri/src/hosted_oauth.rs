//! Brokered OAuth for MCP servers whose provider requires a confidential
//! client (Slack, Google Drive/Gmail) — confirmed against both providers'
//! own docs that `mcp_oauth.rs`'s generic RFC 7591 dynamic-client-
//! registration + ephemeral-loopback-redirect flow can't work for them:
//! neither supports DCR, both require a fixed pre-registered app, and
//! Slack's token exchange requires a `client_secret` that can never safely
//! ship inside a distributed desktop binary.
//!
//! The real client is a small Cloudflare Worker
//! (`little-monkey-website/worker/`, `api.getlittlemonkey.com`) that holds
//! the two providers' `client_secret`s — this module never sees them. Full
//! flow, driven by [`hosted_oauth_connect`]:
//!
//! 1. Generate a random `state`, remember it in `AppState::hosted_oauth_pending`
//!    keyed by that value, build the provider's authorize URL (public
//!    `client_id` only), open it in the system browser. **The command
//!    returns immediately here** — unlike `mcp_oauth.rs`'s
//!    `mcp_oauth_connect`, there's no local listener to block on; the rest
//!    of the flow is driven entirely by an OS event, not this command's own
//!    async body.
//! 2. User logs in on the provider's real site. The provider redirects to
//!    the Worker, which exchanges the code (secret stays server-side),
//!    stashes the token in KV under a one-time handoff code, and 302s the
//!    browser to `littlemonkey://oauth/callback?handoff=...&state=...`.
//! 3. The OS delivers that URL to this app via `tauri-plugin-deep-link`
//!    (macOS/mobile: live event, cold or warm; Windows/Linux: either the
//!    plugin's own CLI-arg parsing on a fresh launch, or
//!    `tauri-plugin-single-instance` piping a second launch's argv into the
//!    already-running instance — see [`register`]). [`handle_deep_link_urls`]
//!    validates `state` against the pending map (drops anything
//!    unmatched — CSRF/spoofing protection), then POSTs the handoff code to
//!    the Worker's `/mcp/oauth/exchange` to redeem the real token, and
//!    saves it to the OS keychain under this module's own namespace
//!    (distinct from `mcp_oauth.rs`'s — see [`keychain_account`]).
//!
//! Progress streams to the frontend via `hosted-oauth://status` events —
//! same shape/convention as `mcp_oauth.rs`'s `mcp-oauth://status`, but a
//! distinct event name (this app already has two *other* unrelated OAuth
//! surfaces — `mcp_oauth.rs` and `m4_commands.rs`'s sandboxed M4-services
//! OAuth — event/command names here must not collide with either).

use std::time::Duration;

use tauri::{AppHandle, Emitter, Manager, Runtime};

use crate::AppState;

/// Same keychain service every credential in this app lives under (see
/// `mcp_oauth.rs`/`mcp.rs`/`providers.rs`) — entries are disambiguated by
/// *account*, not service.
/// Profile-scoped (K23). The default profile keeps this exact service name, so
/// every credential stored before profiles existed still resolves; any other
/// profile's secrets live under `<service>.profile.<id>`, which is a different
/// keychain item that this profile's code never names.
static KEYCHAIN_SERVICE: std::sync::LazyLock<String> =
    std::sync::LazyLock::new(|| crate::profiles::keychain_service("com.littlemonkey.app"));

/// Public client ids — not secret, visible in every authorize URL a browser
/// ever sends anyway. Only the matching `client_secret`s (held by the
/// Cloudflare Worker, never this app) are actually sensitive.
///
/// Still placeholders, and nothing in the shipped UI routes here because of
/// it: the connector catalog (`ConnectorsPanel.tsx`) connects Slack/Google
/// Drive/Gmail through `mcp_oauth.rs`'s bring-your-own-OAuth-app flow, which
/// needs no broker and no credentials baked into a public binary. This module
/// stays for builds that *do* run their own broker (and for anyone whose
/// keychain still holds credentials it saved earlier); until these consts are
/// filled in, [`provider_client_id`] refuses the flow with a message pointing
/// at that alternative instead of opening a browser on an `invalid_client`
/// error page.
const SLACK_CLIENT_ID: &str = "TODO_SLACK_CLIENT_ID";
const GOOGLE_CLIENT_ID: &str = "TODO_GOOGLE_CLIENT_ID";

const BACKEND_BASE: &str = "https://api.getlittlemonkey.com";

const DEEP_LINK_SCHEME: &str = "littlemonkey";

const SLACK_SCOPES: &str = "search:read.public,search:read.private,search:read.mpim,search:read.im,search:read.files,search:read.users,chat:write,channels:history,groups:history,mpim:history,im:history,canvases:read,canvases:write,users:read,users:read.email,reactions:write,reactions:read,emoji:read,files:read,channels:write,groups:write,im:write,mpim:write,channels:read,groups:read,mpim:read";
const GOOGLE_DRIVE_SCOPE: &str = "https://www.googleapis.com/auth/drive";
const GOOGLE_GMAIL_SCOPE: &str = "https://www.googleapis.com/auth/gmail.modify";

/// How long a `state` value (and the pending-connect attempt it represents)
/// stays valid — generous since it spans an entire interactive browser
/// login, not a network round trip. Stale entries are swept opportunistically
/// on the next [`hosted_oauth_connect`] call rather than via a background
/// timer, mirroring every other small map on `AppState`.
const PENDING_TTL: Duration = Duration::from_secs(10 * 60);

/// One in-flight [`hosted_oauth_connect`] attempt waiting for its deep-link
/// callback — see `AppState::hosted_oauth_pending`.
pub struct PendingHostedOAuth {
    pub server_id: String,
    pub provider: String,
    created_at: std::time::Instant,
}

fn emit_progress<R: Runtime>(app: &AppHandle<R>, server_id: &str, phase: &str, error: Option<String>) {
    let _ = app.emit(
        "hosted-oauth://status",
        serde_json::json!({ "serverId": server_id, "phase": phase, "error": error }),
    );
}

fn keychain_account(server_id: &str) -> String {
    format!("hosted-oauth:{server_id}")
}

#[derive(serde::Serialize, serde::Deserialize, Clone)]
struct StoredHostedCredentials {
    provider: String,
    access_token: String,
    refresh_token: Option<String>,
    /// Unix seconds this access token is believed to expire at — `None` if
    /// the provider's response carried no `expires_in` (treated as
    /// non-expiring, same stance `mcp.rs` already takes toward a plain
    /// manually-pasted bearer token).
    expires_at: Option<i64>,
}

fn load_credentials(server_id: &str) -> Result<Option<StoredHostedCredentials>, String> {
    let entry = keyring::Entry::new(&KEYCHAIN_SERVICE, &keychain_account(server_id))
        .map_err(|e| format!("Failed to access keychain: {e}"))?;
    match entry.get_password() {
        Ok(json) => serde_json::from_str(&json)
            .map(Some)
            .map_err(|e| format!("Corrupt stored hosted-OAuth credentials: {e}")),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(e) => Err(format!("Failed to read hosted-OAuth credentials from keychain: {e}")),
    }
}

fn save_credentials(server_id: &str, credentials: &StoredHostedCredentials) -> Result<(), String> {
    let entry = keyring::Entry::new(&KEYCHAIN_SERVICE, &keychain_account(server_id))
        .map_err(|e| format!("Failed to access keychain: {e}"))?;
    let json = serde_json::to_string(credentials)
        .map_err(|e| format!("Failed to serialize hosted-OAuth credentials: {e}"))?;
    entry
        .set_password(&json)
        .map_err(|e| format!("Failed to save hosted-OAuth credentials to keychain: {e}"))
}

/// Whether server `id` currently has hosted-OAuth credentials saved — never
/// the credentials themselves. Mirrors `mcp_oauth::has_oauth_credentials`'s
/// role for `McpServerInfo::has_oauth`; `mcp.rs` ORs both together (a given
/// server id only ever uses one flow, but the check itself doesn't know
/// which without trying both).
pub fn has_oauth_credentials(server_id: &str) -> bool {
    keyring::Entry::new(&KEYCHAIN_SERVICE, &keychain_account(server_id))
        .ok()
        .and_then(|e| e.get_password().ok())
        .is_some()
}

/// Clears server `id`'s saved hosted-OAuth credentials. A no-op success if
/// none were saved.
pub fn remove_oauth_credentials(server_id: &str) -> Result<(), String> {
    let entry = keyring::Entry::new(&KEYCHAIN_SERVICE, &keychain_account(server_id))
        .map_err(|e| format!("Failed to access keychain: {e}"))?;
    match entry.delete_credential() {
        Ok(()) => Ok(()),
        Err(keyring::Error::NoEntry) => Ok(()),
        Err(e) => Err(format!("Failed to remove hosted-OAuth credentials: {e}")),
    }
}

fn provider_scope(provider: &str, server_id: &str) -> Result<&'static str, String> {
    match provider {
        "slack" => Ok(SLACK_SCOPES),
        "google" => match server_id {
            "google-drive" => Ok(GOOGLE_DRIVE_SCOPE),
            "gmail" => Ok(GOOGLE_GMAIL_SCOPE),
            other => Err(format!("unknown Google hosted-oauth server id '{other}'")),
        },
        other => Err(format!("unknown hosted-oauth provider '{other}'")),
    }
}

/// The public client id for `provider`, or a clear error while it's still a
/// placeholder.
///
/// This whole module needs a deployed broker holding client *secrets*, which a
/// public open-source build can't ship — so on a build where these consts were
/// never filled in, connecting here would send the user to a provider error
/// page reading `invalid_client`. Failing before the browser opens, with the
/// alternative named, is the difference between "this app is broken" and "use
/// your own OAuth app": `mcp_oauth.rs`'s bring-your-own-client flow covers
/// every one of these providers without any broker at all (see
/// `docs/byo-oauth-clients.md`), and is what the Settings connector catalog
/// points at by default.
fn provider_client_id(provider: &str) -> Result<&'static str, String> {
    let (client_id, placeholder) = match provider {
        "slack" => (SLACK_CLIENT_ID, "TODO_SLACK_CLIENT_ID"),
        "google" => (GOOGLE_CLIENT_ID, "TODO_GOOGLE_CLIENT_ID"),
        other => return Err(format!("unknown hosted-oauth provider '{other}'")),
    };
    if client_id == placeholder {
        return Err(format!(
            "This build has no hosted OAuth client configured for {provider}, so the brokered sign-in can't run. Connect with your own OAuth app instead: open the server's Connection settings and use \"Connect via OAuth\" (see docs/byo-oauth-clients.md)."
        ));
    }
    Ok(client_id)
}

fn authorize_url(provider: &str, server_id: &str, state: &str) -> Result<String, String> {
    let redirect_uri = format!("{BACKEND_BASE}/mcp/oauth/{provider}/callback");
    let scope = provider_scope(provider, server_id)?;
    let client_id = provider_client_id(provider)?;
    let mut url = url::Url::parse(match provider {
        "slack" => "https://slack.com/oauth/v2_user/authorize",
        "google" => "https://accounts.google.com/o/oauth2/v2/auth",
        other => return Err(format!("unknown hosted-oauth provider '{other}'")),
    })
    .map_err(|e| e.to_string())?;
    {
        let mut pairs = url.query_pairs_mut();
        pairs
            .append_pair("client_id", client_id)
            .append_pair("redirect_uri", &redirect_uri)
            .append_pair("state", state);
        if provider == "slack" {
            pairs.append_pair("user_scope", scope);
        } else {
            pairs
                .append_pair("scope", scope)
                .append_pair("response_type", "code")
                // Google only returns a `refresh_token` on the *first*
                // consent for a given account+scope combination unless
                // `access_type=offline` + `prompt=consent` force a fresh
                // one every time — without this, reconnecting after a
                // revoke would silently come back with no refresh token.
                .append_pair("access_type", "offline")
                .append_pair("prompt", "consent");
        }
    }
    Ok(url.to_string())
}

/// Response shape both the Worker's `/mcp/oauth/exchange` and
/// `/mcp/oauth/refresh` endpoints return — mirrors the OAuth token endpoint
/// JSON they themselves relay/produce.
#[derive(serde::Deserialize)]
struct BackendTokenResponse {
    access_token: String,
    refresh_token: Option<String>,
    expires_in: Option<i64>,
    error: Option<String>,
}

/// The client both relay calls use.
///
/// `egress::hardened()` because the bodies these POST are the credentials
/// themselves — a one-time handoff code in one case, a long-lived refresh token
/// in the other. A default client would replay that body to whatever host a
/// `302` named (reqwest preserves the body across a 307/308, and re-POSTs are
/// exactly what a relay redirect would produce), and would wait forever on a
/// relay that accepted the connection and then stalled. `BACKEND_BASE` being a
/// constant limits who can trigger that, but a compromised or misconfigured
/// relay is precisely the case worth bounding.
fn relay_client() -> Result<reqwest::Client, String> {
    crate::egress::hardened()
        .build()
        .map_err(|e| format!("Failed to build the OAuth relay HTTP client: {e}"))
}

async fn redeem_handoff(handoff: &str) -> Result<BackendTokenResponse, String> {
    let response = crate::egress::send(
        relay_client()?
            .post(format!("{BACKEND_BASE}/mcp/oauth/exchange"))
            .json(&serde_json::json!({ "handoff": handoff })),
    )
    .await
    .map_err(|e| format!("Failed to reach the OAuth relay: {e}"))?;
    let body: BackendTokenResponse = response
        .json()
        .await
        .map_err(|e| format!("Unexpected response from the OAuth relay: {e}"))?;
    if let Some(error) = body.error {
        return Err(format!("OAuth relay could not redeem the handoff: {error}"));
    }
    Ok(body)
}

async fn refresh_via_backend(provider: &str, refresh_token: &str) -> Result<BackendTokenResponse, String> {
    let response = crate::egress::send(
        relay_client()?
            .post(format!("{BACKEND_BASE}/mcp/oauth/refresh"))
            .json(&serde_json::json!({ "provider": provider, "refresh_token": refresh_token })),
    )
    .await
    .map_err(|e| format!("Failed to reach the OAuth relay: {e}"))?;
    let body: BackendTokenResponse = response
        .json()
        .await
        .map_err(|e| format!("Unexpected response from the OAuth relay: {e}"))?;
    if let Some(error) = body.error {
        return Err(format!("OAuth relay could not refresh the token: {error}"));
    }
    Ok(body)
}

/// Seconds of slack subtracted from a token's advertised expiry before
/// treating it as expired — avoids a connect racing a token that's valid
/// when checked but expires mid-request.
const REFRESH_SAFETY_MARGIN_SECS: i64 = 60;

/// If server `id` has hosted-OAuth credentials saved, returns a currently
/// valid access token for it (refreshing via the Worker if the cached one
/// has expired) — `Ok(None)` if no hosted-OAuth credentials exist at all for
/// this id. An `Err` means credentials DO exist but no usable token could be
/// produced (e.g. the refresh token was revoked) — re-authorization via
/// [`hosted_oauth_connect`] is required.
///
/// Deliberately does not (and cannot) reuse `mcp_oauth::get_access_token_if_connected`
/// — that function refreshes by re-discovering the MCP server's own OAuth
/// metadata and calling its token endpoint directly, with no way to attach
/// a `client_secret`. Every network call here goes through the Worker
/// instead, so the secret never needs to leave it.
pub async fn get_access_token_if_connected(
    state: &AppState,
    server_id: &str,
) -> Result<Option<String>, String> {
    let Some(mut creds) = load_credentials(server_id)? else {
        return Ok(None);
    };

    let now = chrono::Utc::now().timestamp();
    let expired = creds
        .expires_at
        .is_some_and(|exp| now >= exp - REFRESH_SAFETY_MARGIN_SECS);
    if !expired {
        return Ok(Some(creds.access_token));
    }

    let Some(refresh_token) = creds.refresh_token.clone() else {
        return Err(format!(
            "Hosted OAuth for '{server_id}' has expired and no refresh token was saved — reconnect via OAuth in Settings."
        ));
    };

    // Same reasoning as `mcp_oauth::refresh_lock_for`'s doc comment: two
    // overlapping `mcp_connect` calls for the same server id must not both
    // redeem the same still-current refresh token concurrently.
    let lock = crate::mcp_oauth::refresh_lock_for(state, server_id);
    let _guard = lock.lock().await;

    let refreshed = refresh_via_backend(&creds.provider, &refresh_token).await?;
    creds.access_token = refreshed.access_token.clone();
    creds.expires_at = refreshed.expires_in.map(|secs| now + secs);
    if let Some(rotated) = refreshed.refresh_token {
        creds.refresh_token = Some(rotated);
    }
    save_credentials(server_id, &creds)?;
    Ok(Some(creds.access_token))
}

/// Starts a hosted-OAuth connect for `server_id` against `provider`
/// (`"slack"` or `"google"`) — generates a CSRF `state`, opens the system
/// browser on the provider's real authorize page, and returns immediately.
/// Completion (success, error, or timeout) streams later via
/// `hosted-oauth://status` events, driven by [`handle_deep_link_urls`].
#[tauri::command(rename_all = "snake_case")]
pub fn hosted_oauth_connect<R: Runtime>(
    app: AppHandle<R>,
    state: tauri::State<'_, AppState>,
    server_id: String,
    provider: String,
) -> Result<(), String> {
    crate::mcp::validate_id(&server_id)?;
    let csrf_state = uuid::Uuid::new_v4().to_string();
    let url = authorize_url(&provider, &server_id, &csrf_state)?;

    {
        let mut pending = state
            .hosted_oauth_pending
            .lock()
            .map_err(|_| "hosted OAuth pending-state lock poisoned".to_string())?;
        pending.retain(|_, p| p.created_at.elapsed() < PENDING_TTL);
        pending.insert(
            csrf_state,
            PendingHostedOAuth {
                server_id: server_id.clone(),
                provider,
                created_at: std::time::Instant::now(),
            },
        );
    }

    emit_progress(&app, &server_id, "opening_browser", None);
    {
        use tauri_plugin_opener::OpenerExt;
        app.opener()
            .open_url(url, None::<String>)
            .map_err(|e| format!("Failed to open the system browser for OAuth: {e}"))?;
    }
    emit_progress(&app, &server_id, "waiting_for_browser", None);
    Ok(())
}

/// Drops any pending hosted-OAuth attempt(s) for `server_id` — there is no
/// in-flight task to interrupt (unlike `mcp_oauth_cancel`, this command's
/// own async work already finished the moment the browser opened), this
/// just makes a later, now-unwanted deep-link callback a no-op and resets
/// the UI's "waiting" state.
#[tauri::command(rename_all = "snake_case")]
pub fn hosted_oauth_cancel<R: Runtime>(app: AppHandle<R>, state: tauri::State<'_, AppState>, server_id: String) -> Result<(), String> {
    let mut pending = state
        .hosted_oauth_pending
        .lock()
        .map_err(|_| "hosted OAuth pending-state lock poisoned".to_string())?;
    pending.retain(|_, p| p.server_id != server_id);
    drop(pending);
    emit_progress(&app, &server_id, "cancelled", None);
    Ok(())
}

/// Clears server `id`'s saved hosted-OAuth credentials from the keychain.
/// Does not disconnect a currently-running MCP connection.
#[tauri::command(rename_all = "snake_case")]
pub fn hosted_oauth_disconnect(server_id: String) -> Result<(), String> {
    remove_oauth_credentials(&server_id)
}

/// Handles every deep-link URL the OS delivers — the single entry point
/// both `register`'s `on_open_url` callback (macOS/mobile, and Windows/
/// Linux's own fresh-launch CLI-arg parsing) and the single-instance
/// closure's manual argv scan (Windows/Linux, app already running) funnel
/// into, so there's exactly one implementation of "what a
/// `littlemonkey://oauth/callback` URL means."
pub fn handle_deep_link_urls<R: Runtime>(app: &AppHandle<R>, urls: Vec<url::Url>) {
    for url in urls {
        if url.scheme() != DEEP_LINK_SCHEME {
            continue;
        }
        let app = app.clone();
        tauri::async_runtime::spawn(async move {
            handle_callback_url(&app, &url).await;
        });
    }
}

async fn handle_callback_url<R: Runtime>(app: &AppHandle<R>, url: &url::Url) {
    let params: std::collections::HashMap<String, String> = url.query_pairs().into_owned().collect();
    let Some(csrf_state) = params.get("state").cloned() else {
        return;
    };

    let pending = {
        let app_state = app.state::<AppState>();
        let mut guard = match app_state.hosted_oauth_pending.lock() {
            Ok(g) => g,
            Err(_) => return,
        };
        guard.remove(&csrf_state)
    };
    // No matching pending attempt: either a stale/expired entry, a replayed
    // link, or an outright spoofed one — silently drop it either way, same
    // as `mcp_oauth.rs`'s CSRF-token check inside `exchange_code_for_token`.
    let Some(pending) = pending else { return };

    if let Some(error) = params.get("error") {
        emit_progress(app, &pending.server_id, "error", Some(error.clone()));
        return;
    }

    let Some(handoff) = params.get("handoff") else {
        emit_progress(
            app,
            &pending.server_id,
            "error",
            Some("The OAuth callback was missing its handoff code.".to_string()),
        );
        return;
    };

    emit_progress(app, &pending.server_id, "exchanging_token", None);
    match redeem_handoff(handoff).await {
        Ok(token) => {
            let now = chrono::Utc::now().timestamp();
            let creds = StoredHostedCredentials {
                provider: pending.provider.clone(),
                access_token: token.access_token,
                refresh_token: token.refresh_token,
                expires_at: token.expires_in.map(|secs| now + secs),
            };
            if let Err(message) = save_credentials(&pending.server_id, &creds) {
                emit_progress(app, &pending.server_id, "error", Some(message));
                return;
            }
            emit_progress(app, &pending.server_id, "connected", None);
        }
        Err(message) => {
            emit_progress(app, &pending.server_id, "error", Some(message));
        }
    }
}

/// Registers the deep-link plugin's `on_open_url` listener — call once from
/// `lib.rs`'s `.setup()`. Covers macOS/mobile (live events, cold or warm
/// launch) and Windows/Linux's own fresh-launch CLI-arg parsing; a
/// still-running instance on Windows/Linux is covered separately by
/// [`extract_deep_link_from_argv`] wired into the single-instance plugin.
pub fn register<R: Runtime>(app: &AppHandle<R>) {
    use tauri_plugin_deep_link::DeepLinkExt;
    let handle = app.clone();
    app.deep_link().on_open_url(move |event| {
        handle_deep_link_urls(&handle, event.urls());
    });
}

/// Windows/Linux only: when the app is already running and the OS spawns a
/// second process for a `littlemonkey://` link, `tauri-plugin-single-
/// instance` intercepts that launch and hands this its raw `argv` — the
/// deep-link plugin's own CLI-arg parsing never runs in that doomed second
/// process, so this replicates just enough of it (single trailing argument
/// that parses as our scheme) to find the URL, if any.
pub fn extract_deep_link_from_argv(argv: &[String]) -> Option<url::Url> {
    let arg = argv.get(1)?;
    if argv.len() != 2 {
        return None;
    }
    let url = arg.parse::<url::Url>().ok()?;
    (url.scheme() == DEEP_LINK_SCHEME).then_some(url)
}
