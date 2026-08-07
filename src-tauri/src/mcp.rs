//! MCP (Model Context Protocol) client support — stdio + streamable-HTTP.
//!
//! Users configure external MCP servers, persisted at
//! `<app_data>/mcp_servers.json` (atomic temp+rename writes, exactly like
//! `sessions.rs`/`memory.rs`). Each server is either a `Stdio` child process
//! (spawned via `rmcp`'s `TokioChildProcess`) or an `Http` remote endpoint
//! (via `rmcp`'s streamable-HTTP client transport, over `reqwest`). An HTTP
//! server's bearer token comes from one of two places, never from
//! `mcp_servers.json` itself: either a manually pasted static token saved to
//! the OS keychain via `mcp_set_http_token`/`mcp_remove_http_token` (same
//! `keyring::Entry::new(KEYCHAIN_SERVICE, ...)` convention as
//! `providers.rs`), or a generic MCP-spec OAuth 2.0 flow run via
//! `mcp_oauth.rs`'s `mcp_oauth_connect` (RFC 8414 discovery + RFC 7591
//! dynamic client registration + PKCE, built on `rmcp`'s
//! `transport::auth` module) — see [`connect_impl`]'s `Http` branch for how
//! the two are prioritized. Either way, the token is attached as an
//! `Authorization: Bearer <token>` header (via
//! `StreamableHttpClientTransportConfig::auth_header`) when connecting.
//!
//! Runtime connections live in `AppState::mcp`, a `tokio::sync::Mutex` (not
//! `std::sync::Mutex`, unlike every other map in `AppState`) because
//! connecting and calling a tool are both `.await`-ing operations — see
//! [`McpConnection`]. The mutex is never held across an `.await` on the
//! connection itself: every caller locks it just long enough to clone the
//! cheap `Peer<RoleClient>` handle (or, for connect/disconnect, to
//! insert/remove the whole `McpConnection`) and then drops the guard before
//! doing any real work, so two split-pane turns can call the same or
//! different servers concurrently without serializing on this lock.
//!
//! Every `rmcp` type this module needs (`RunningService`, `RoleClient`,
//! `TokioChildProcess`, `CallToolResult`, ...) is used here and nowhere
//! else in the crate, so a future `rmcp` upgrade only ever requires editing
//! this one file — see the design doc's "dependency friction" risk note.
//!
//! Follows the `checkpoints.rs`/`sessions.rs`/`memory.rs`/`rules.rs`
//! AppHandle-free `*_impl` split: config load/save and connect/call are all
//! plain functions taking `&Path`/`&AppState`, directly unit-testable (see
//! the bottom of this file) and reusable from `monkey-cli` later, while the
//! `#[tauri::command]`s are thin wrappers that resolve the config path,
//! gate permission, and emit `mcp://status` events.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use rmcp::model::{
    CallToolRequest, CallToolRequestParams, CallToolResult, CancelledNotificationParam,
    ClientRequest, ServerResult,
};
use rmcp::service::{PeerRequestOptions, RunningService, ServiceError};
use rmcp::transport::streamable_http_client::StreamableHttpError;
use rmcp::RoleClient;
use tauri::{Emitter, Manager};

use crate::{permissions, AppState};

/// Filename for the persisted server list under the app data directory.
const CONFIG_FILE: &str = "mcp_servers.json";

/// Current (and, so far, only) on-disk schema version.
const SCHEMA_VERSION: u8 = 1;

/// Default per-call timeout when a server entry doesn't override it via
/// `timeout_secs`.
const DEFAULT_TIMEOUT_SECS: u64 = 60;

/// Bound on how long connecting to (spawning/dialing, handshaking, and
/// listing tools for) a single MCP server may take before both
/// [`mcp_connect`] and monkey-cli's `connect_all` give up and report an
/// error/timeout — `connect_impl` itself has no internal timeout (see its
/// own doc comment), so a server whose process spawns (or whose HTTP
/// endpoint accepts the connection) but never completes the `initialize`
/// handshake would otherwise hang the caller forever: a stuck spinner with
/// no cancel affordance in the GUI, or a stalled startup in the CLI. Mirrors
/// `DEFAULT_TIMEOUT_SECS`'s analogous role for [`call_tool_with_cancel_impl`].
pub const CONNECT_TIMEOUT_SECS: u64 = 30;

/// The scope every MCP connection's own work runs under.
///
/// A named constant rather than the literal at the one call site, because the
/// value *is* the decision recorded in
/// [`crate::run_scope::Unattributed::SharedTransport`]'s doc comment — one
/// connection per configured server, shared by every run — and a reader who lands
/// on [`connect_impl`] should be sent there rather than left to infer it.
///
/// # The ceiling this carries, stated rather than left to be discovered
///
/// `run_scope` exists for K5's per-run egress allowlist, and nothing reads
/// `current()` for policy yet — no site in `mcp.rs`, `mcp_oauth.rs` or
/// `hosted_oauth.rs` calls `denial_sink::record` at all. When a policy *does* read
/// it, entering this scope has an enforcement consequence beyond attribution: the
/// one credentialed in-task round-trip it covers is the OAuth refresh, which is
/// reachable from the reauth retry in [`call_tool_with_cancel_impl`] — and that
/// retry runs *inside* a run's scope. So a refresh driven by a run would be
/// evaluated under the connection's policy rather than that run's.
///
/// That is the right default for the reason the decision gives: the token belongs
/// to the connection, is shared by every later run, and refreshing it on one run's
/// narrower allowlist would let whichever run happened to trigger the refresh
/// decide whether every other run's connection survives. But it is a real choice,
/// not an oversight, and the day a per-run allowlist lands it should be re-read
/// rather than rediscovered.
const CONNECTION_SCOPE: crate::run_scope::RunScope =
    crate::run_scope::RunScope::Unattributed(crate::run_scope::Unattributed::SharedTransport);

/// Keychain service name for HTTP servers' bearer tokens — same string
/// `providers.rs` uses for provider API keys (a separate private constant
/// there; keychain entries are disambiguated by *account*, not service, so
/// this file's `mcp:<id>` account prefix is what keeps the two features'
/// entries apart within the one service namespace).
const KEYCHAIN_SERVICE: &str = "com.littlemonkey.app";

/// The keychain *account* name under which server `id`'s HTTP bearer token
/// is stored — `mcp:<id>`, distinguishing it from `providers.rs`'s
/// `<provider_id>`-only accounts in the same keychain service.
fn keychain_account(server_id: &str) -> String {
    format!("mcp:{}", server_id)
}

/// Reads the bearer token saved for `server_id`'s HTTP transport, if any.
/// Absence is normal (no token configured, or the server doesn't require
/// one) — `None`, not an error, mirroring `providers::has_key`'s stance.
fn read_http_token(server_id: &str) -> Option<String> {
    keyring::Entry::new(KEYCHAIN_SERVICE, &keychain_account(server_id))
        .ok()?
        .get_password()
        .ok()
}

/// Core remove-token logic behind `mcp_remove_http_token`, also called
/// (best-effort) by `mcp_remove_server` so deleting a server doesn't leave
/// an orphaned credential behind. A missing entry is a no-op success, same
/// as `providers.rs::remove_key_impl`.
fn remove_http_token_impl(server_id: &str) -> Result<(), String> {
    let entry = keyring::Entry::new(KEYCHAIN_SERVICE, &keychain_account(server_id))
        .map_err(|e| format!("Failed to access keychain: {}", e))?;
    match entry.delete_credential() {
        Ok(()) => Ok(()),
        Err(keyring::Error::NoEntry) => Ok(()),
        Err(e) => Err(format!("Failed to remove saved token: {}", e)),
    }
}

/// How a configured MCP server is reached. `Stdio` spawns a local child
/// process; `Http` connects to a remote streamable-HTTP MCP endpoint (see
/// [`connect_impl`]), optionally authenticating with a bearer token read
/// from the OS keychain (never persisted in this struct/on disk).
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum McpTransport {
    Stdio {
        command: String,
        #[serde(default)]
        args: Vec<String>,
        #[serde(default)]
        env: BTreeMap<String, String>,
    },
    Http {
        url: String,
    },
}

/// One configured MCP server, as persisted in `mcp_servers.json`. Plain
/// snake_case field names (no serde renames) — like `providers.json`, this
/// file is meant to be hand-editable.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct McpServerEntry {
    pub id: String,
    pub label: String,
    pub transport: McpTransport,
    pub enabled: bool,
    #[serde(default)]
    pub tool_allowlist: Option<Vec<String>>,
    #[serde(default)]
    pub timeout_secs: Option<u64>,
}

/// The whole on-disk `mcp_servers.json` document.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct McpConfigFile {
    #[serde(default)]
    pub version: u8,
    #[serde(default)]
    pub servers: Vec<McpServerEntry>,
}

/// Reject anything that isn't a simple slug, so a crafted id can never
/// traverse outside expected bounds or break the `mcp__<id>__<tool>` tool
/// name composition (phase 2) that requires `^[a-zA-Z0-9_-]+$` on each half.
/// Deliberately allows `_` (unlike `checkpoints::validate_id`, which only
/// ever sees its own generated UUIDs) since server ids are user-chosen
/// slugs, e.g. "my_server".
pub(crate) fn validate_id(id: &str) -> Result<(), String> {
    if !id.is_empty()
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        Ok(())
    } else {
        Err(format!(
            "Invalid MCP server id '{}': must be non-empty and contain only letters, digits, '-', or '_'",
            id
        ))
    }
}

/// Resolves (and creates, if missing) `<app_data_dir>/mcp_servers.json`'s path.
pub(crate) fn config_file_path(app: &tauri::AppHandle) -> Result<PathBuf, String> {
    let dir = app
        .path()
        .app_data_dir()
        .map_err(|e| format!("Failed to resolve app data dir: {}", e))?;
    std::fs::create_dir_all(&dir).map_err(|e| format!("Failed to create app data dir: {}", e))?;
    Ok(dir.join(CONFIG_FILE))
}

/// Core load logic, parameterized by path for testability. A missing file
/// (nothing configured yet — the common case) is simply the empty default,
/// never an error.
pub fn load_config_impl(path: &Path) -> Result<McpConfigFile, String> {
    match std::fs::read_to_string(path) {
        Ok(raw) => {
            serde_json::from_str(&raw).map_err(|e| format!("Corrupt mcp_servers.json: {}", e))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(McpConfigFile::default()),
        Err(e) => Err(format!("Failed to read mcp_servers.json: {}", e)),
    }
}

/// Core save logic: atomic sibling temp file + rename, same idiom as
/// `sessions.rs`'s `save_to` / `memory.rs`'s `save_impl`, so a crash
/// mid-write can never leave a truncated/corrupt config file behind.
pub fn save_config_impl(path: &Path, config: &McpConfigFile) -> Result<(), String> {
    let payload = serde_json::to_string_pretty(config)
        .map_err(|e| format!("Failed to serialize mcp_servers.json: {}", e))?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, &payload)
        .map_err(|e| format!("Failed to write mcp_servers.json: {}", e))?;
    std::fs::rename(&tmp, path)
        .map_err(|e| format!("Failed to finalize mcp_servers.json: {}", e))?;
    Ok(())
}

/// Validates an entry's shape (id format, non-empty transport fields) before
/// it's persisted — shared by add/update so both reject the same malformed
/// input.
fn validate_entry(entry: &McpServerEntry) -> Result<(), String> {
    validate_id(&entry.id)?;
    if entry.label.trim().is_empty() {
        return Err("MCP server label must not be empty".to_string());
    }
    match &entry.transport {
        McpTransport::Stdio { command, .. } if command.trim().is_empty() => {
            Err("Stdio MCP servers require a non-empty command".to_string())
        }
        McpTransport::Http { url } if url.trim().is_empty() => {
            Err("HTTP MCP servers require a non-empty url".to_string())
        }
        _ => Ok(()),
    }
}

/// Core add-server logic behind `mcp_add_server`. Errors if `entry.id`
/// already exists (use `mcp_update_server` to edit) or fails validation.
pub fn add_server_impl(path: &Path, entry: McpServerEntry) -> Result<McpServerEntry, String> {
    validate_entry(&entry)?;

    let mut config = load_config_impl(path)?;
    if config.servers.iter().any(|s| s.id == entry.id) {
        return Err(format!(
            "An MCP server with id '{}' already exists",
            entry.id
        ));
    }

    config.version = SCHEMA_VERSION;
    config.servers.push(entry.clone());
    save_config_impl(path, &config)?;
    Ok(entry)
}

/// Core update-server logic behind `mcp_update_server`. Replaces the entry
/// with a matching id in place (preserving list order); errors if no such
/// server is configured.
pub fn update_server_impl(path: &Path, entry: McpServerEntry) -> Result<McpServerEntry, String> {
    validate_entry(&entry)?;

    let mut config = load_config_impl(path)?;
    let slot = config
        .servers
        .iter_mut()
        .find(|s| s.id == entry.id)
        .ok_or_else(|| format!("Unknown MCP server '{}'", entry.id))?;
    *slot = entry.clone();

    save_config_impl(path, &config)?;
    Ok(entry)
}

/// Core remove-server logic behind `mcp_remove_server`. Removing an id
/// that isn't present is a no-op success — the caller's desired end state
/// (the server is gone) already holds, mirroring `memory.rs::delete_fact_impl`.
pub fn remove_server_impl(path: &Path, id: &str) -> Result<(), String> {
    let mut config = load_config_impl(path)?;
    let before = config.servers.len();
    config.servers.retain(|s| s.id != id);
    if config.servers.len() != before {
        save_config_impl(path, &config)?;
    }
    Ok(())
}

/// Core enable/disable logic behind `mcp_set_enabled`.
pub fn set_enabled_impl(path: &Path, id: &str, enabled: bool) -> Result<McpServerEntry, String> {
    let mut config = load_config_impl(path)?;
    let slot = config
        .servers
        .iter_mut()
        .find(|s| s.id == id)
        .ok_or_else(|| format!("Unknown MCP server '{}'", id))?;
    slot.enabled = enabled;
    let updated = slot.clone();

    save_config_impl(path, &config)?;
    Ok(updated)
}

/// A minimal, stable snapshot of one server-provided tool — deliberately
/// its own type (not `rmcp::model::Tool` directly) so this module is the
/// only place that has to change if `rmcp`'s `Tool` shape churns across
/// versions.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CachedMcpTool {
    pub name: String,
    pub description: Option<String>,
    pub input_schema: serde_json::Value,
}

fn cache_tool(tool: &rmcp::model::Tool) -> CachedMcpTool {
    CachedMcpTool {
        name: tool.name.to_string(),
        description: tool.description.as_ref().map(|d| d.to_string()),
        input_schema: serde_json::Value::Object((*tool.input_schema).clone()),
    }
}

/// A live connection to one MCP server: the running `rmcp` service handle
/// plus a cache of its tools and `initialize`-result instructions. Held in
/// `AppState::mcp`, keyed by server id.
pub struct McpConnection {
    pub service: RunningService<RoleClient, ()>,
    /// The server's full, unfiltered tool list — allowlist filtering (see
    /// `McpServerEntry::tool_allowlist`) is applied by callers
    /// (`mcp_list_servers`' frontend projection and [`call_tool_impl`]), not baked in here, so
    /// `mcp_list_servers`/the Settings tool-allowlist checkboxes can still
    /// show the full set of what a server offers.
    pub tools: Vec<CachedMcpTool>,
    pub instructions: Option<String>,
}

/// Connect to `entry` — stdio spawns a child process via `TokioChildProcess`,
/// HTTP connects to a remote streamable-HTTP endpoint via `rmcp`'s
/// `StreamableHttpClientTransport` (reqwest-backed), attaching a bearer
/// token from the keychain (see [`read_http_token`]) as the `Authorization`
/// header when one is saved for this server id. Either way, caches the
/// resulting tool list and `initialize`-result instructions, and replaces
/// (gracefully closing) any previous connection for the same id.
///
/// AppHandle-free and directly unit-testable: see the tests at the bottom
/// of this file, which spawn a real (trivial) MCP server over stdio.
///
/// Deliberately does NOT itself apply a timeout — a server whose process
/// spawns (or endpoint accepts the connection) but never completes the
/// `initialize` handshake would otherwise hang this `await` forever. See
/// [`mcp_connect`] (and monkey-cli's `connect_all`) for that, which wrap this
/// call in [`CONNECT_TIMEOUT_SECS`] — the same division of labor
/// [`call_tool_with_cancel_impl`] documents for permission/timeout/
/// cancellation around a tool call.
///
/// # Why this enters `CONNECTION_SCOPE`, shadowing any run it was called from
///
/// A connection here is shared by every run — see
/// [`crate::run_scope::Unattributed::SharedTransport`] for why that was chosen
/// over one transport per run — so the work of establishing it is the
/// connection's, not the caller's. The shadowing is the load-bearing half: this
/// function is also called from [`call_tool_with_cancel_impl`]'s
/// re-authorization retry, which *is* inside a run's scope, and without entering
/// a scope here the first run unlucky enough to trip a 401 would be billed for
/// re-handshaking a connection that four other runs go on to use.
///
/// # Which egress that actually reaches, and which it provably does not
///
/// This is worth stating exactly, because the tempting version of this comment
/// would claim more than the mechanism allows. `tokio::spawn` does not inherit a
/// task-local (`run_scope`'s own
/// `a_spawned_task_does_not_inherit_the_scope` pins it), and the streamable-HTTP
/// transport is built out of two bare `tokio::spawn`s:
///
/// - `StreamableHttpClientTransport::with_client` is `WorkerTransport::spawn`,
///   a `tokio::spawn` at rmcp-2.2.0's `transport/worker.rs:115`. Every HTTP
///   request the transport ever issues runs in that task.
/// - `rmcp::serve_client` ends in `serve_inner`, which spawns the service loop
///   with `tokio::spawn` at rmcp-2.2.0's `service.rs:945`.
///
/// And `Transport::send` for a `WorkerTransport` (`transport/worker.rs:189`) only
/// pushes onto an mpsc channel, so even the `initialize` POST that
/// `serve_client` *awaits in this task* is issued by the worker task, not here.
/// So the scope covers the OAuth token fetch below (a real, credentialed
/// round-trip that does run in this task), the keychain read in `read_http_token`,
/// and the pre-connect validation — and covers **none** of the transport's own
/// requests:
/// the POSTs, the SSE notification stream, its `Last-Event-ID` reconnects, the
/// session delete. Those are outside every scope and record neither a run nor a
/// reason, which is `run_scope`'s honest third state rather than a blank standing
/// in for one.
///
/// The same two spawns are why the transport's bytes are not *measured* either.
/// [`crate::egress::send`] counts a request it is handed as a `RequestBuilder`, and
/// rmcp's transport never surfaces one — it owns its `reqwest::Client` and issues
/// every request inside the worker task. So those bytes are absent from
/// `bytes_egressed` and from [`crate::egress::unattributed_egress_bytes`] alike,
/// which is the one place in this tree where a count is missing rather than merely
/// unattributed. The OAuth round-trip below *is* metered, because that one is an
/// ordinary call this file makes itself.
///
/// That is not a gap left open for want of trying. The only seam that could carry
/// a scope into the worker task is a `StreamableHttpClient` wrapper entering it
/// per request, and implementing that trait means naming `sse_stream::Sse` and
/// `http::HeaderName` — neither of which rmcp re-exports nor this crate depends
/// on directly. Two new dependencies and a stream wrapper, to establish a scope
/// that nothing on this path reads today: no site in `mcp.rs`, `mcp_oauth.rs` or
/// `hosted_oauth.rs` calls `denial_sink::record` at all. When one does, the
/// caller-task egress above is already labelled; the worker's still won't be, and
/// the fix then is a wrapper, not a wider comment.
pub async fn connect_impl(
    state: &AppState,
    entry: &McpServerEntry,
) -> Result<(Vec<CachedMcpTool>, Option<String>), String> {
    crate::run_scope::scoped(CONNECTION_SCOPE, connect_in_scope(state, entry)).await
}

/// What scope [`connect_in_scope`] actually observed, for the test that pins it.
///
/// A test-only observation point rather than an assertion inside production code. The
/// claim being pinned — that connect-time work runs under [`CONNECTION_SCOPE`] and not
/// under whatever run called it — is otherwise unfalsifiable from outside: every
/// alternative test passes with the wrapper deleted, because they exercise
/// `run_scope::scoped` directly rather than this call path. Two earlier D3 adoptions set
/// the bar at "reverting the wrapper turns a test red", and this is what meets it here.
#[cfg(test)]
static OBSERVED_CONNECT_SCOPE: std::sync::Mutex<Option<crate::run_scope::RunScope>> =
    std::sync::Mutex::new(None);

/// [`connect_impl`]'s body, split out only so the scope entry above is a single
/// readable line instead of an `async move` block wrapping the whole function.
async fn connect_in_scope(
    state: &AppState,
    entry: &McpServerEntry,
) -> Result<(Vec<CachedMcpTool>, Option<String>), String> {
    // Recorded before any transport branch, so the cheapest possible failure — a blank
    // command or URL, which returns without opening a socket — still exercises it.
    #[cfg(test)]
    {
        *OBSERVED_CONNECT_SCOPE
            .lock()
            .expect("the observation point is never held across a panic") =
            crate::run_scope::current();
    }
    let service = match &entry.transport {
        McpTransport::Stdio { command, args, env } => {
            if command.trim().is_empty() {
                return Err(format!(
                    "MCP server '{}' has no command configured",
                    entry.id
                ));
            }

            let mut command_builder = tokio::process::Command::new(command);
            command_builder.args(args).envs(env);

            let child = rmcp::transport::TokioChildProcess::new(command_builder)
                .map_err(|e| format!("Failed to spawn MCP server '{}': {}", entry.id, e))?;

            rmcp::serve_client((), child)
                .await
                .map_err(|e| format!("Failed to initialize MCP server '{}': {}", entry.id, e))?
        }
        McpTransport::Http { url } => {
            if url.trim().is_empty() {
                return Err(format!("MCP server '{}' has no URL configured", entry.id));
            }

            let mut config =
                rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig::with_uri(
                    url.clone(),
                );
            // An OAuth-connected server (either flow — see below) takes
            // priority over a manually pasted static token: its presence
            // means the user explicitly ran an OAuth flow for this server
            // id, and both flows' `get_access_token_if_connected` auto-
            // refresh an expired token. `Ok(None)` from both means no OAuth
            // credentials are stored for this id at all — fall back to
            // whatever static token (if any) was saved via
            // `mcp_set_http_token`. An `Err` (stored credentials exist but
            // can't produce a usable token — e.g. refresh failed and
            // re-authorization is required) is surfaced as a real connect
            // failure rather than silently degrading to the static token,
            // since that static token may be stale/absent precisely because
            // OAuth was set up instead.
            //
            // Two flows, not one, because not every provider can use the
            // first: `mcp_oauth`'s generic RFC 7591 dynamic-client-
            // registration + loopback-redirect flow works for servers that
            // support it (Atlassian, Notion, Stripe, PostHog, ...); Slack
            // and Google Drive/Gmail don't (confirmed against their own
            // docs — no DCR, and Slack additionally requires a
            // `client_secret` no desktop binary can hold safely), so those
            // three go through `hosted_oauth`'s Cloudflare-Worker-brokered
            // flow instead. A given server id only ever has credentials in
            // one of the two.
            let oauth_token =
                match crate::hosted_oauth::get_access_token_if_connected(state, &entry.id).await? {
                    Some(token) => Some(token),
                    None => {
                        crate::mcp_oauth::get_access_token_if_connected(state, &entry.id, url)
                            .await?
                    }
                };
            match oauth_token {
                Some(token) => config = config.auth_header(token),
                None => {
                    if let Some(token) = read_http_token(&entry.id) {
                        config = config.auth_header(token);
                    }
                }
            }

            // The client handed to `rmcp` carries whatever token the block above
            // resolved (OAuth bearer or a pasted static one) on *every* request
            // the transport makes, including any it makes after a redirect. A
            // default client would follow up to ten hops to an arbitrary host —
            // `url` is user-configurable, so that host could be a loopback
            // service with no authentication of its own.
            //
            // The read timeout `hardened()` brings is defence in depth here
            // rather than the only bound: `mcp_connect` already wraps
            // [`connect_impl`] in [`CONNECT_TIMEOUT_SECS`] and
            // [`call_tool_with_cancel_impl`] bounds each call by the per-server
            // `timeout_secs`. It matters for what those do not cover — the
            // standalone SSE notification stream, which rmcp's
            // `SseAutoReconnectStream` resumes with `Last-Event-ID` after an
            // error, so a stalled stream reconnects instead of going quiet
            // forever.
            // The silence budget must never be tighter than the budget this server
            // was explicitly configured with, or a long-running tool that sends no
            // progress notifications would be cut short of its own timeout. So the
            // default acts as a floor here, not a ceiling.
            let configured =
                Duration::from_secs(entry.timeout_secs.unwrap_or(DEFAULT_TIMEOUT_SECS));
            let budget = configured.max(crate::egress::READ_TIMEOUT);
            let http_client = crate::egress::hardened_with_read_budget(budget)
                .build()
                .map_err(|e| format!("Failed to build the MCP HTTP client: {e}"))?;
            let transport =
                rmcp::transport::StreamableHttpClientTransport::with_client(http_client, config);

            rmcp::serve_client((), transport)
                .await
                .map_err(|e| format!("Failed to initialize MCP server '{}': {}", entry.id, e))?
        }
    };

    let tools: Vec<CachedMcpTool> = service
        .peer()
        .list_all_tools()
        .await
        .map_err(|e| format!("Failed to list tools for MCP server '{}': {}", entry.id, e))?
        .iter()
        .map(cache_tool)
        .collect();

    let instructions = service
        .peer_info()
        .and_then(|info| info.instructions.clone());

    let connection = McpConnection {
        service,
        tools: tools.clone(),
        instructions: instructions.clone(),
    };

    // Swap in the new connection, closing any previous one for this id
    // outside the lock (dropping it would eventually kill the child process
    // too, via `TokioChildProcess`'s own cleanup, but an explicit graceful
    // close is faster and doesn't rely on that).
    let previous = {
        let mut guard = state.mcp.lock().await;
        guard.insert(entry.id.clone(), connection)
    };
    if let Some(previous) = previous {
        let _ = previous.service.cancel().await;
    }

    Ok((tools, instructions))
}

/// Disconnect (and gracefully close) the connection for `server_id`, if any.
/// A no-op if it isn't currently connected.
pub async fn disconnect_impl(state: &AppState, server_id: &str) {
    let removed = {
        let mut guard = state.mcp.lock().await;
        guard.remove(server_id)
    };
    if let Some(connection) = removed {
        let _ = connection.service.cancel().await;
    }
}

/// Disconnects every currently-connected MCP server, best-effort. Called
/// from `lib.rs`'s `RunEvent::Exit` handler on app quit — see that call
/// site's own comment for why this is the *only* chance a connected stdio
/// server's child process gets to actually die before the app process
/// itself exits (Tauri's event loop calls `std::process::exit` right after,
/// which skips Rust's Drop-based cleanup entirely).
///
/// Bounded by a short per-server timeout so one unresponsive server can
/// never hang application shutdown. Note this is still best-effort even
/// within that bound: `RunningService::cancel()` awaits until this
/// connection's own run loop task finishes — which is when its transport
/// (and, for stdio, `rmcp`'s `TokioChildProcess`/`ChildWithCleanup`) gets
/// dropped and *schedules* (via `tokio::spawn`) the actual `child.kill()`,
/// rather than performing it synchronously — so this also waits a brief
/// grace period afterward to give that spawned kill task a chance to
/// actually run before returning control to a caller that's about to let
/// the process exit.
pub async fn disconnect_all(state: &AppState) {
    let connections: Vec<McpConnection> = {
        let mut guard = state.mcp.lock().await;
        guard.drain().map(|(_, connection)| connection).collect()
    };
    if connections.is_empty() {
        return;
    }
    for connection in connections {
        let _ = tokio::time::timeout(Duration::from_secs(3), connection.service.cancel()).await;
    }
    tokio::time::sleep(Duration::from_millis(200)).await;
}

/// Resolves and validates the live connection for `entry`/`tool_name` (must
/// be connected, in the allowlist if one is set, and in the server's cached
/// tool list) and builds the `CallToolRequestParams` to send. Shared setup
/// for [`call_tool_with_cancel_impl`], factored out so that function's own
/// body reads as just "dispatch the (cancellable) request" — dropping the
/// `state.mcp` lock before returning, per module docs, since everything past
/// this point awaits.
async fn resolve_call_tool(
    state: &AppState,
    entry: &McpServerEntry,
    tool_name: &str,
    arguments: serde_json::Value,
) -> Result<(rmcp::Peer<RoleClient>, CallToolRequestParams), String> {
    let peer = {
        let guard = state.mcp.lock().await;
        let connection = guard
            .get(&entry.id)
            .ok_or_else(|| format!("MCP server '{}' is not connected", entry.id))?;

        let allowed = entry
            .tool_allowlist
            .as_ref()
            .map(|allow| allow.iter().any(|t| t == tool_name))
            .unwrap_or(true);
        if !allowed {
            return Err(format!(
                "Tool '{}' is not in the allowlist configured for MCP server '{}'",
                tool_name, entry.id
            ));
        }
        if !connection.tools.iter().any(|t| t.name == tool_name) {
            return Err(format!(
                "Unknown tool '{}' on MCP server '{}' — it isn't in the server's cached tool list",
                tool_name, entry.id
            ));
        }

        // Clone the cheap `Peer` handle out and drop the map lock before
        // doing anything that awaits — see module docs.
        connection.service.peer().clone()
    };

    let arguments_obj = match arguments {
        serde_json::Value::Object(map) => Some(map),
        serde_json::Value::Null => None,
        other => {
            return Err(format!(
                "MCP tool arguments must be a JSON object, got: {}",
                other
            ))
        }
    };

    let mut params = CallToolRequestParams::new(tool_name.to_string());
    params.arguments = arguments_obj;

    Ok((peer, params))
}

/// A single call attempt keeps a genuine streamable-HTTP authentication
/// rejection distinct from every other failure until the retry decision has
/// been made. In particular, a JSON-RPC/tool error is always `Other`, even if
/// its message happens to contain "401", "unauthorized", or "invalid_token".
#[derive(Debug)]
enum ToolCallAttemptError {
    TransportAuthRequired(String),
    Other(String),
}

impl ToolCallAttemptError {
    fn into_message(self) -> String {
        match self {
            Self::TransportAuthRequired(message) | Self::Other(message) => message,
        }
    }
}

fn transport_auth_status_message<T>(outcome: &Result<T, ToolCallAttemptError>) -> Option<&str> {
    match outcome {
        Err(ToolCallAttemptError::TransportAuthRequired(message)) => Some(message),
        Ok(_) | Err(ToolCallAttemptError::Other(_)) => None,
    }
}

/// Whether `error` is the streamable-HTTP transport itself reporting an HTTP
/// 401. `rmcp` normally represents that as `AuthRequired`; a server that omits
/// the required `WWW-Authenticate` header instead becomes an
/// `UnexpectedServerResponse("HTTP 401 ...")`, so accept that narrowly typed
/// transport case too. Never inspect a JSON-RPC/tool error's text.
fn is_transport_auth_required(error: &ServiceError) -> bool {
    let ServiceError::TransportSend(transport_error) = error else {
        return false;
    };
    let Some(http_error) = transport_error
        .error
        .downcast_ref::<StreamableHttpError<reqwest::Error>>()
    else {
        return false;
    };

    match http_error {
        StreamableHttpError::AuthRequired(_) => true,
        StreamableHttpError::UnexpectedServerResponse(message) => message
            .strip_prefix("HTTP ")
            .is_some_and(|rest| rest.starts_with("401 ") || rest.starts_with("401:")),
        StreamableHttpError::Client(error) => {
            error.status() == Some(reqwest::StatusCode::UNAUTHORIZED)
        }
        _ => false,
    }
}

fn classify_tool_call_error(
    tool_name: &str,
    server_id: &str,
    error: ServiceError,
) -> ToolCallAttemptError {
    let auth_required = is_transport_auth_required(&error);
    let message = format!(
        "MCP tool call to '{}' on '{}' failed: {}",
        tool_name, server_id, error
    );
    if auth_required {
        ToolCallAttemptError::TransportAuthRequired(message)
    } else {
        ToolCallAttemptError::Other(message)
    }
}

/// Whether a failed tool call is worth retrying after re-establishing the
/// connection: only for an HTTP server whose token this app can actually
/// re-mint, i.e. one with OAuth credentials saved. A static pasted token
/// wouldn't change on reconnect, and a stdio server has no token at all.
fn can_retry_after_reauth(entry: &McpServerEntry) -> bool {
    matches!(entry.transport, McpTransport::Http { .. })
        && (crate::mcp_oauth::has_oauth_credentials(&entry.id)
            || crate::hosted_oauth::has_oauth_credentials(&entry.id))
}

#[derive(Debug, PartialEq)]
enum ReconnectAttemptError {
    Cancelled(String),
    Failed(String),
}

/// Re-establishes an authenticated connection without letting the reconnect
/// bypass the call's existing Stop/timeout signal. The explicit connection
/// bound is needed because [`connect_impl`] is deliberately unbounded.
async fn reconnect_with_cancel<F, C, T>(
    reconnect: F,
    mut cancel: std::pin::Pin<&mut C>,
    server_id: &str,
    timeout: Duration,
) -> Result<T, ReconnectAttemptError>
where
    F: std::future::Future<Output = Result<T, String>>,
    C: std::future::Future<Output = String> + ?Sized,
{
    tokio::select! {
        result = reconnect => result.map_err(ReconnectAttemptError::Failed),
        reason = &mut cancel => Err(ReconnectAttemptError::Cancelled(reason)),
        _ = tokio::time::sleep(timeout) => Err(ReconnectAttemptError::Failed(format!(
            "Reconnecting to MCP server '{}' timed out after {} seconds",
            server_id,
            timeout.as_secs()
        ))),
    }
}

/// Validates that `tool_name` is both currently offered by the connected
/// server AND permitted by `entry.tool_allowlist` (when set), then calls it
/// with `arguments` — genuinely cancellably: if `cancel` resolves with a
/// reason string before the server responds, this sends the server a real
/// `notifications/cancelled` for the in-flight request id (via
/// `Peer::notify_cancelled`) and returns `Err(reason)` immediately.
///
/// This matters because dispatching the request through
/// `Peer::call_tool`/`Peer::send_request` (the non-cancellable path
/// `call_tool_impl` used before this function existed) and then merely
/// racing that future against a timeout/cancel signal in a `tokio::select!`
/// only ever abandons the *client's* wait for a response — the JSON-RPC
/// request was already sent, and the server keeps executing the tool call
/// (and any side effects it performs) to completion regardless, with no way
/// for the user or the model to know that happened. Using
/// `Peer::send_cancellable_request` instead gets us a `RequestHandle` whose
/// request id we can (and, on cancellation, do) actually tell the server to
/// stop.
///
/// Returns the server's `CallToolResult` verbatim — mapping its content
/// blocks into a string for the model is the frontend's job, same division
/// of labor as `tool_run_shell` returning raw stdout/stderr.
///
/// Deliberately does NOT itself gate permission — see `mcp_call_tool` for
/// that, mirroring how `tool_run_shell` layers permission around its own
/// core logic.
pub async fn call_tool_with_cancel_impl(
    state: &AppState,
    entry: &McpServerEntry,
    tool_name: &str,
    arguments: serde_json::Value,
    cancel: impl std::future::Future<Output = String>,
) -> Result<CallToolResult, String> {
    call_tool_with_cancel_classified(state, entry, tool_name, arguments, cancel)
        .await
        .map_err(ToolCallAttemptError::into_message)
}

async fn call_tool_with_cancel_classified(
    state: &AppState,
    entry: &McpServerEntry,
    tool_name: &str,
    arguments: serde_json::Value,
    cancel: impl std::future::Future<Output = String>,
) -> Result<CallToolResult, ToolCallAttemptError> {
    tokio::pin!(cancel);

    // An HTTP server's bearer token is baked into its transport's
    // `Authorization` header when `connect_impl` builds it, so a token that
    // expires while the connection is up (an OAuth access token typically
    // lasts an hour; connections here live as long as the app does) turns
    // every subsequent tool call into a 401 that no amount of retrying at the
    // same connection can fix. Reconnect once — which re-reads the keychain
    // and refreshes the access token via `get_access_token_if_connected` —
    // and replay the call.
    //
    // Safe to replay specifically because the failure was an auth rejection:
    // the server refused the request before running the tool, so this can't
    // duplicate a side effect. No other error class is retried here.
    match call_tool_once(state, entry, tool_name, arguments.clone(), cancel.as_mut()).await {
        Err(ToolCallAttemptError::TransportAuthRequired(message))
            if can_retry_after_reauth(entry) =>
        {
            match reconnect_with_cancel(
                connect_impl(state, entry),
                cancel.as_mut(),
                &entry.id,
                Duration::from_secs(CONNECT_TIMEOUT_SECS),
            )
            .await
            {
                Ok(_) => {}
                Err(ReconnectAttemptError::Cancelled(reason)) => {
                    return Err(ToolCallAttemptError::Other(reason));
                }
                Err(ReconnectAttemptError::Failed(reconnect_error)) => {
                    return Err(ToolCallAttemptError::TransportAuthRequired(format!(
                        "{message}\n\nReconnecting to refresh authorization also failed: {reconnect_error}"
                    )));
                }
            }
            call_tool_once(state, entry, tool_name, arguments, cancel.as_mut()).await
        }
        Err(error) => Err(error),
        Ok(result) => Ok(result),
    }
}

/// One attempt of [`call_tool_with_cancel_impl`] — see that function for the
/// contract; this is split out only so the auth-expiry retry above can run the
/// same body twice against the same (still-pending) `cancel` future.
async fn call_tool_once(
    state: &AppState,
    entry: &McpServerEntry,
    tool_name: &str,
    arguments: serde_json::Value,
    mut cancel: std::pin::Pin<&mut impl std::future::Future<Output = String>>,
) -> Result<CallToolResult, ToolCallAttemptError> {
    let (peer, params) = resolve_call_tool(state, entry, tool_name, arguments)
        .await
        .map_err(ToolCallAttemptError::Other)?;

    let handle = peer
        .send_cancellable_request(
            ClientRequest::CallToolRequest(CallToolRequest::new(params)),
            PeerRequestOptions::no_options(),
        )
        .await
        .map_err(|error| classify_tool_call_error(tool_name, &entry.id, error))?;

    // Captured before `handle` is moved into `handle.await_response()` below
    // — everything `notify_cancelled` needs to tell the server to stop this
    // exact in-flight request, without needing the (about-to-be-consumed)
    // handle itself.
    let cancel_peer = handle.peer.clone();
    let cancel_request_id = handle.id.clone();

    tokio::select! {
        response = handle.await_response() => response
            .map_err(|error| classify_tool_call_error(tool_name, &entry.id, error))
            .and_then(|result| match result {
                ServerResult::CallToolResult(result) => Ok(result),
                _ => Err(ToolCallAttemptError::Other(format!(
                    "MCP server '{}' returned an unexpected response type for tool '{}'",
                    entry.id, tool_name
                ))),
            }),
        reason = &mut cancel => {
            let _ = cancel_peer
                .notify_cancelled(CancelledNotificationParam::new(Some(cancel_request_id), Some(reason.clone())))
                .await;
            Err(ToolCallAttemptError::Other(reason))
        }
    }
}

/// [`call_tool_with_cancel_impl`] with a `cancel` that never resolves — the
/// entry point for callers that don't need real mid-call cancellation: this
/// module's own tests (below) and monkey-cli's `mcp_cli::call`, which only ever
/// wraps this in a plain `tokio::time::timeout` (see that function's own doc
/// comment on why the CLI, unlike the GUI, has no concurrent "Stop"
/// affordance to wire a genuine cancel signal from).
pub async fn call_tool_impl(
    state: &AppState,
    entry: &McpServerEntry,
    tool_name: &str,
    arguments: serde_json::Value,
) -> Result<CallToolResult, String> {
    call_tool_with_cancel_impl(state, entry, tool_name, arguments, std::future::pending()).await
}

/// A configured server plus its live status, for `mcp_list_servers`.
/// Frontend-facing (unlike `McpServerEntry`/`McpConfigFile`), so this one
/// uses `camelCase` field names to match the rest of the app's TS-consumed
/// command results (e.g. `checkpoints.rs::CheckpointSummary`).
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct McpServerInfo {
    pub id: String,
    pub label: String,
    pub transport: McpTransport,
    pub enabled: bool,
    pub tool_allowlist: Option<Vec<String>>,
    pub timeout_secs: Option<u64>,
    pub status: String,
    pub error: Option<String>,
    pub tools: Vec<CachedMcpTool>,
    pub instructions: Option<String>,
    /// Whether a bearer token is currently saved in the keychain for this
    /// server — never the token itself. Always `false` for `Stdio` servers
    /// (they have no keychain entry); lets the Settings UI show a "token
    /// saved" state without ever reading the secret back out.
    pub has_http_token: bool,
    /// Whether this server currently has OAuth-derived credentials saved
    /// (see `mcp_oauth.rs`) — never the credentials themselves. Always
    /// `false` for `Stdio` servers. When both this and `has_http_token` are
    /// `true`, [`connect_impl`] prefers the OAuth-derived token (see its
    /// `Http` branch).
    pub has_oauth: bool,
}

fn build_info(
    entry: &McpServerEntry,
    status: &str,
    error: Option<String>,
    tools: Vec<CachedMcpTool>,
    instructions: Option<String>,
) -> McpServerInfo {
    McpServerInfo {
        id: entry.id.clone(),
        label: entry.label.clone(),
        transport: entry.transport.clone(),
        enabled: entry.enabled,
        tool_allowlist: entry.tool_allowlist.clone(),
        timeout_secs: entry.timeout_secs,
        status: status.to_string(),
        error,
        tools,
        instructions,
        has_http_token: matches!(entry.transport, McpTransport::Http { .. })
            && read_http_token(&entry.id).is_some(),
        has_oauth: matches!(entry.transport, McpTransport::Http { .. })
            && (crate::mcp_oauth::has_oauth_credentials(&entry.id)
                || crate::hosted_oauth::has_oauth_credentials(&entry.id)),
    }
}

/// Emit an `mcp://status` event to all windows, mirroring `llama.rs`'s
/// `emit_status` for `llama://status`.
fn emit_status<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    server_id: &str,
    status: &str,
    error: Option<String>,
    tool_count: Option<usize>,
) {
    let _ = app.emit(
        "mcp://status",
        serde_json::json!({
            "serverId": server_id,
            "status": status,
            "error": error,
            "toolCount": tool_count,
        }),
    );
}

/// List every configured MCP server with its live connection status.
#[tauri::command]
pub async fn mcp_list_servers(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
) -> Result<Vec<McpServerInfo>, String> {
    let config = load_config_impl(&config_file_path(&app)?)?;
    let guard = state.mcp.lock().await;
    Ok(config
        .servers
        .iter()
        .map(|entry| match guard.get(&entry.id) {
            Some(connection) => build_info(
                entry,
                "connected",
                None,
                connection.tools.clone(),
                connection.instructions.clone(),
            ),
            None => build_info(entry, "disconnected", None, Vec::new(), None),
        })
        .collect())
}

/// Core logic behind [`mcp_add_server`]: serializes against every other
/// config-mutating call via `state.mcp_config_lock` (see its doc comment on
/// `AppState`) so two concurrent calls can never race on the same
/// load-then-save cycle and silently drop one another's change, then revokes
/// any stale "allow for session" grant for the (possibly just-freed,
/// possibly-about-to-be-reused) id — see
/// `permissions::revoke_session_allow_for_mcp_server`. AppHandle-free and
/// directly unit-testable, same `*_impl` split as `add_server_impl` itself
/// (which this wraps); factored out from the `#[tauri::command]` itself
/// because that command's `app: tauri::AppHandle` is the concrete
/// default-runtime type alias, which can't be constructed against
/// `tauri::test::MockRuntime` for a unit test.
fn add_server_with_state_impl(
    state: &AppState,
    path: &Path,
    entry: McpServerEntry,
) -> Result<McpServerEntry, String> {
    let _guard = state
        .mcp_config_lock
        .lock()
        .map_err(|_| "MCP config lock poisoned".to_string())?;
    let saved = add_server_impl(path, entry)?;
    // Defensive: `add_server_impl` only succeeds for an id that isn't
    // currently configured, but if this id was just freed up by a
    // `mcp_remove_server` call, any "allow for session" grant approved for
    // whatever *previously* answered to it must never silently apply to
    // this new, unrelated server.
    permissions::revoke_session_allow_for_mcp_server(state, &saved.id);
    Ok(saved)
}

/// Add a new MCP server to the config. Does not connect it — call
/// `mcp_connect` separately (the Settings UI does this right after adding).
#[tauri::command(rename_all = "snake_case")]
pub fn mcp_add_server(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    entry: McpServerEntry,
) -> Result<McpServerEntry, String> {
    add_server_with_state_impl(state.inner(), &config_file_path(&app)?, entry)
}

/// Core logic behind [`mcp_update_server`] — see
/// [`add_server_with_state_impl`]'s doc comment for why this exists as an
/// AppHandle-free, directly unit-testable wrapper around `update_server_impl`.
fn update_server_with_state_impl(
    state: &AppState,
    path: &Path,
    entry: McpServerEntry,
) -> Result<McpServerEntry, String> {
    let _guard = state
        .mcp_config_lock
        .lock()
        .map_err(|_| "MCP config lock poisoned".to_string())?;
    let saved = update_server_impl(path, entry)?;
    // The transport this id points at may have just changed — any existing
    // "allow for session" grant for it was approved against whatever the
    // OLD prompt showed, which may no longer describe what this id now
    // does. See `revoke_session_allow_for_mcp_server`'s doc comment.
    permissions::revoke_session_allow_for_mcp_server(state, &saved.id);
    Ok(saved)
}

/// Replace an existing MCP server's config by id. Does not reconnect —
/// callers that changed connection-affecting fields (command/args/env/url)
/// should follow up with `mcp_disconnect` + `mcp_connect`.
#[tauri::command(rename_all = "snake_case")]
pub fn mcp_update_server(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    entry: McpServerEntry,
) -> Result<McpServerEntry, String> {
    update_server_with_state_impl(state.inner(), &config_file_path(&app)?, entry)
}

/// Core logic behind [`mcp_remove_server`]'s config mutation — see
/// [`add_server_with_state_impl`]'s doc comment for why this exists as an
/// AppHandle-free, directly unit-testable wrapper around `remove_server_impl`.
/// Deliberately doesn't disconnect the live connection or clear the keychain
/// token itself — those need an `AppState`/`await` and an `AppHandle`
/// respectively, so stay in the `#[tauri::command]` wrapper.
fn remove_server_with_state_impl(
    state: &AppState,
    path: &Path,
    server_id: &str,
) -> Result<(), String> {
    let _guard = state
        .mcp_config_lock
        .lock()
        .map_err(|_| "MCP config lock poisoned".to_string())?;
    remove_server_impl(path, server_id)?;
    // This id may be reused by a completely different server later (see
    // `AddMcpServerForm`'s label-to-id slugify) — any "allow for session"
    // grant approved for the server that just got removed must not silently
    // apply to whatever answers to the same id next.
    permissions::revoke_session_allow_for_mcp_server(state, server_id);
    Ok(())
}

/// Remove an MCP server from the config, disconnecting it first if it's
/// currently connected.
#[tauri::command(rename_all = "snake_case")]
pub async fn mcp_remove_server(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    server_id: String,
) -> Result<(), String> {
    validate_id(&server_id)?;
    disconnect_impl(state.inner(), &server_id).await;
    emit_status(&app, &server_id, "disconnected", None, None);
    remove_server_with_state_impl(state.inner(), &config_file_path(&app)?, &server_id)?;
    // Best-effort: an HTTP server that never had a token saved (or a stdio
    // server, which never has one) hits the `NoEntry` no-op path — never
    // fails the removal itself over keychain cleanup.
    let _ = remove_http_token_impl(&server_id);
    let _ = crate::mcp_oauth::remove_oauth_credentials_impl(&server_id);
    let _ = crate::hosted_oauth::remove_oauth_credentials(&server_id);
    Ok(())
}

/// Enable or disable a configured server. Disabling a currently-connected
/// server also disconnects it — a disabled server must not keep a child
/// process (or its tools) alive.
///
/// Serialized against every other config-mutating command via
/// `AppState::mcp_config_lock` (see its doc comment).
#[tauri::command(rename_all = "snake_case")]
pub async fn mcp_set_enabled(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    server_id: String,
    enabled: bool,
) -> Result<McpServerEntry, String> {
    validate_id(&server_id)?;
    let updated = {
        let _guard = state
            .mcp_config_lock
            .lock()
            .map_err(|_| "MCP config lock poisoned".to_string())?;
        set_enabled_impl(&config_file_path(&app)?, &server_id, enabled)?
    };
    if !enabled {
        disconnect_impl(state.inner(), &server_id).await;
        emit_status(&app, &server_id, "disconnected", None, None);
    }
    Ok(updated)
}

/// Connect to a configured MCP server (stdio only in this phase), caching
/// its tool list. Emits `mcp://status` transitions through `"connecting"`
/// and then `"connected"`/`"error"`. Bounded by [`CONNECT_TIMEOUT_SECS`] —
/// `connect_impl` itself has no internal timeout (see its doc comment), so
/// without this a server whose process spawns (or endpoint accepts the
/// connection) but never completes the `initialize` handshake would leave
/// the Settings UI's reconnect spinner stuck forever with no way to cancel.
#[tauri::command(rename_all = "snake_case")]
pub async fn mcp_connect(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    server_id: String,
) -> Result<McpServerInfo, String> {
    validate_id(&server_id)?;
    let config = load_config_impl(&config_file_path(&app)?)?;
    let entry = config
        .servers
        .iter()
        .find(|s| s.id == server_id)
        .cloned()
        .ok_or_else(|| format!("Unknown MCP server '{}'", server_id))?;

    emit_status(&app, &server_id, "connecting", None, None);

    let outcome = tokio::time::timeout(
        Duration::from_secs(CONNECT_TIMEOUT_SECS),
        connect_impl(state.inner(), &entry),
    )
    .await
    .unwrap_or_else(|_elapsed| {
        Err(format!(
            "Connecting to MCP server '{}' timed out after {} seconds",
            server_id, CONNECT_TIMEOUT_SECS
        ))
    });

    match outcome {
        Ok((tools, instructions)) => {
            emit_status(&app, &server_id, "connected", None, Some(tools.len()));
            Ok(build_info(&entry, "connected", None, tools, instructions))
        }
        Err(e) => {
            emit_status(&app, &server_id, "error", Some(e.clone()), None);
            Err(e)
        }
    }
}

/// Disconnect a currently-connected MCP server. A no-op (still succeeds)
/// if it wasn't connected.
#[tauri::command(rename_all = "snake_case")]
pub async fn mcp_disconnect(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    server_id: String,
) -> Result<(), String> {
    validate_id(&server_id)?;
    disconnect_impl(state.inner(), &server_id).await;
    emit_status(&app, &server_id, "disconnected", None, None);
    Ok(())
}

/// Save (or overwrite) the bearer token used to authenticate an HTTP MCP
/// server's connection — kept in the OS keychain only, never in
/// `mcp_servers.json`. Takes effect on the next `mcp_connect` (reconnect an
/// already-connected server to pick up a newly-saved/changed token).
#[tauri::command(rename_all = "snake_case")]
pub fn mcp_set_http_token(server_id: String, token: String) -> Result<(), String> {
    validate_id(&server_id)?;
    let token = token.trim();
    if token.is_empty() {
        return Err("Bearer token must not be empty".to_string());
    }
    let entry = keyring::Entry::new(KEYCHAIN_SERVICE, &keychain_account(&server_id))
        .map_err(|e| format!("Failed to access keychain: {}", e))?;
    entry
        .set_password(token)
        .map_err(|e| format!("Failed to save token to keychain: {}", e))
}

/// Remove a saved HTTP bearer token. A no-op success if none was saved.
#[tauri::command(rename_all = "snake_case")]
pub fn mcp_remove_http_token(server_id: String) -> Result<(), String> {
    validate_id(&server_id)?;
    remove_http_token_impl(&server_id)
}

/// Call a tool on a connected MCP server. Permission-gated
/// (`mcp:<server_id>:<tool_name>`, previewing the tool name and
/// pretty-printed arguments — same convention as `tool_run_shell`'s command
/// preview), turn-scoped cancellable via the same `AppState::tool_cancel`
/// mechanism `tool_run_shell` uses, and bounded by a timeout (per-server
/// `timeout_secs`, default 60s). Both the Stop-button cancellation and the
/// timeout are real, protocol-level cancellations (a `notifications/cancelled`
/// sent to the server via [`call_tool_with_cancel_impl`]), not just the
/// client abandoning its own wait for a response the server keeps executing
/// regardless.
#[tauri::command(rename_all = "snake_case")]
pub async fn mcp_call_tool(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    server_id: String,
    tool_name: String,
    arguments: serde_json::Value,
    turn_id: Option<String>,
    tool_call_id: Option<String>,
) -> Result<CallToolResult, String> {
    validate_id(&server_id)?;

    let config = load_config_impl(&config_file_path(&app)?)?;
    let entry = config
        .servers
        .iter()
        .find(|s| s.id == server_id)
        .cloned()
        .ok_or_else(|| format!("Unknown MCP server '{}'", server_id))?;

    if !entry.enabled {
        return Err(format!("MCP server '{}' is disabled", server_id));
    }

    let pretty_args =
        serde_json::to_string_pretty(&arguments).unwrap_or_else(|_| arguments.to_string());
    let detail = format!("{} → {}\n{}", entry.label, tool_name, pretty_args);

    permissions::request_permission(
        &app,
        state.inner(),
        &format!("mcp:{}:{}", server_id, tool_name),
        detail,
        turn_id.as_deref(),
        tool_call_id.as_deref(),
        None,
        None,
    )
    .await?;

    let timeout = Duration::from_secs(entry.timeout_secs.unwrap_or(DEFAULT_TIMEOUT_SECS));

    // Per-turn cancellation channel, mirroring `tool_run_shell` exactly: a
    // Stop in one split-pane turn must never cancel the other pane's
    // in-flight MCP call.
    let cancel_key = turn_id.clone().unwrap_or_default();
    let cancel = state
        .tool_cancel
        .lock()
        .map_err(|_| "Tool-cancel lock poisoned".to_string())?
        .entry(cancel_key.clone())
        .or_insert_with(|| std::sync::Arc::new(tokio::sync::Notify::new()))
        .clone();

    // Resolves with the reason to report — and, inside
    // `call_tool_with_cancel_impl`, the reason actually sent to the server
    // in a `notifications/cancelled` — the moment either the Stop button or
    // the per-server timeout fires, whichever comes first.
    let cancel_reason = async {
        tokio::select! {
            _ = cancel.notified() => "MCP tool call cancelled by the user".to_string(),
            _ = tokio::time::sleep(timeout) => format!(
                "MCP tool '{}' on server '{}' timed out after {} seconds",
                tool_name, server_id, timeout.as_secs()
            ),
        }
    };

    let outcome = call_tool_with_cancel_classified(
        state.inner(),
        &entry,
        &tool_name,
        arguments,
        cancel_reason,
    )
    .await;

    // Drop this turn's cancel channel once no other MCP/shell call for the
    // same turn still holds it — same bookkeeping as `tool_run_shell`.
    {
        let mut guard = state
            .tool_cancel
            .lock()
            .map_err(|_| "Tool-cancel lock poisoned".to_string())?;
        if guard
            .get(&cancel_key)
            .is_some_and(|n| std::sync::Arc::strong_count(n) <= 2)
        {
            guard.remove(&cancel_key);
        }
    }

    if let Some(message) = transport_auth_status_message(&outcome) {
        emit_status(&app, &server_id, "error", Some(message.to_string()), None);
    }

    outcome.map_err(ToolCallAttemptError::into_message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn temp_path(name: &str) -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "little_monkey_mcp_test_{}_{}_{}_{}",
            std::process::id(),
            n,
            nanos,
            name
        ))
    }

    fn stdio_entry(id: &str, command: &str, args: &[&str]) -> McpServerEntry {
        McpServerEntry {
            id: id.to_string(),
            label: format!("Test server {id}"),
            transport: McpTransport::Stdio {
                command: command.to_string(),
                args: args.iter().map(|a| a.to_string()).collect(),
                env: BTreeMap::new(),
            },
            enabled: true,
            tool_allowlist: None,
            timeout_secs: None,
        }
    }

    fn http_entry(id: &str) -> McpServerEntry {
        McpServerEntry {
            id: id.to_string(),
            label: format!("Test server {id}"),
            transport: McpTransport::Http {
                url: "https://example.test/mcp".to_string(),
            },
            enabled: true,
            tool_allowlist: None,
            timeout_secs: None,
        }
    }

    // --- expired-token retry ---------------------------------------------

    #[test]
    fn typed_streamable_http_auth_rejection_is_retry_eligible() {
        let http_error = StreamableHttpError::<reqwest::Error>::AuthRequired(
            rmcp::transport::streamable_http_client::AuthRequiredError::new(
                "Bearer realm=\"test\"".to_string(),
            ),
        );
        let error =
            ServiceError::TransportSend(rmcp::transport::DynamicTransportError::from_parts(
                "test streamable HTTP transport",
                std::any::TypeId::of::<()>(),
                Box::new(http_error),
            ));

        assert!(is_transport_auth_required(&error));
    }

    #[test]
    fn typed_headerless_http_401_is_retry_eligible() {
        let http_error = StreamableHttpError::<reqwest::Error>::UnexpectedServerResponse(
            std::borrow::Cow::Borrowed("HTTP 401 Unauthorized: token expired"),
        );
        let error =
            ServiceError::TransportSend(rmcp::transport::DynamicTransportError::from_parts(
                "test streamable HTTP transport",
                std::any::TypeId::of::<()>(),
                Box::new(http_error),
            ));

        assert!(is_transport_auth_required(&error));
    }

    #[test]
    fn tool_level_401_is_never_retry_eligible() {
        let error = ServiceError::McpError(rmcp::model::ErrorData::internal_error(
            "401 Unauthorized: invalid_token while creating the draft",
            None,
        ));

        let outcome: Result<(), ToolCallAttemptError> =
            Err(classify_tool_call_error("create_draft", "gmail", error));

        assert!(matches!(
            &outcome,
            Err(ToolCallAttemptError::Other(message))
                if message.contains("401 Unauthorized")
                    && message.contains("invalid_token")
        ));
        assert!(transport_auth_status_message(&outcome).is_none());
    }

    #[test]
    fn typed_transport_auth_rejection_surfaces_as_status_error() {
        let http_error = StreamableHttpError::<reqwest::Error>::UnexpectedServerResponse(
            std::borrow::Cow::Borrowed("HTTP 401 Unauthorized: token expired"),
        );
        let error =
            ServiceError::TransportSend(rmcp::transport::DynamicTransportError::from_parts(
                "test streamable HTTP transport",
                std::any::TypeId::of::<()>(),
                Box::new(http_error),
            ));
        let outcome: Result<(), ToolCallAttemptError> =
            Err(classify_tool_call_error("send_message", "slack", error));

        let status_message =
            transport_auth_status_message(&outcome).expect("transport 401 should update status");
        assert!(status_message.contains("send_message"));
        assert!(status_message.contains("slack"));
        assert!(status_message.contains("HTTP 401 Unauthorized"));
    }

    #[test]
    fn non_http_transport_error_text_is_never_retry_eligible() {
        let error =
            ServiceError::TransportSend(rmcp::transport::DynamicTransportError::from_parts(
                "test non-HTTP transport",
                std::any::TypeId::of::<()>(),
                Box::new(std::io::Error::other(
                    "Auth required: HTTP 401 Unauthorized invalid_token",
                )),
            ));

        assert!(!is_transport_auth_required(&error));
    }

    #[test]
    fn only_oauth_connected_http_servers_are_worth_retrying_after_reauth() {
        // A stdio server has no bearer token to re-mint, so a reconnect+replay
        // would only duplicate the failure.
        assert!(!can_retry_after_reauth(&stdio_entry("local", "echo", &[])));

        // An HTTP server with no OAuth credentials saved (the case that
        // produced the original Gmail failure, and a static pasted token) —
        // reconnecting can't produce a token it doesn't have.
        assert!(!can_retry_after_reauth(&http_entry("no-credentials-saved")));
    }

    #[tokio::test]
    async fn reauth_reconnect_obeys_existing_cancel_signal() {
        let mut cancel = Box::pin(async { "MCP tool call cancelled by the user".to_string() });
        let result: Result<(), ReconnectAttemptError> = reconnect_with_cancel(
            std::future::pending(),
            cancel.as_mut(),
            "gmail",
            Duration::from_secs(CONNECT_TIMEOUT_SECS),
        )
        .await;

        assert_eq!(
            result.unwrap_err(),
            ReconnectAttemptError::Cancelled("MCP tool call cancelled by the user".to_string())
        );
    }

    #[tokio::test]
    async fn reauth_reconnect_has_a_connection_timeout() {
        let mut cancel = Box::pin(std::future::pending::<String>());
        let result: Result<(), ReconnectAttemptError> = reconnect_with_cancel(
            std::future::pending(),
            cancel.as_mut(),
            "gmail",
            Duration::ZERO,
        )
        .await;

        assert_eq!(
            result.unwrap_err(),
            ReconnectAttemptError::Failed(
                "Reconnecting to MCP server 'gmail' timed out after 0 seconds".to_string()
            )
        );
    }

    // --- config load/save/CRUD -------------------------------------------

    #[test]
    fn load_returns_default_when_file_missing() {
        let path = temp_path("missing.json");
        let config = load_config_impl(&path).unwrap();
        assert!(config.servers.is_empty());
    }

    #[test]
    fn add_then_load_roundtrips_and_persists_atomically() {
        let path = temp_path("add.json");
        let entry = stdio_entry("my-server", "echo", &[]);

        add_server_impl(&path, entry.clone()).unwrap();

        assert!(
            !path.with_extension("json.tmp").exists(),
            "temp file must not linger"
        );
        let reloaded = load_config_impl(&path).unwrap();
        assert_eq!(reloaded.servers.len(), 1);
        assert_eq!(reloaded.servers[0], entry);
    }

    #[test]
    fn add_rejects_duplicate_id() {
        let path = temp_path("dup.json");
        add_server_impl(&path, stdio_entry("dup", "echo", &[])).unwrap();

        let err = add_server_impl(&path, stdio_entry("dup", "echo", &[])).unwrap_err();
        assert!(err.contains("already exists"), "unexpected error: {err}");
    }

    #[test]
    fn add_rejects_invalid_id() {
        let path = temp_path("badid.json");
        let err = add_server_impl(&path, stdio_entry("bad id!", "echo", &[])).unwrap_err();
        assert!(
            err.contains("Invalid MCP server id"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn add_rejects_empty_stdio_command() {
        let path = temp_path("nocmd.json");
        let mut entry = stdio_entry("nocmd", "echo", &[]);
        entry.transport = McpTransport::Stdio {
            command: "  ".to_string(),
            args: Vec::new(),
            env: BTreeMap::new(),
        };
        let err = add_server_impl(&path, entry).unwrap_err();
        assert!(err.contains("non-empty command"), "unexpected error: {err}");
    }

    #[test]
    fn update_replaces_matching_entry() {
        let path = temp_path("update.json");
        add_server_impl(&path, stdio_entry("srv", "echo", &["one"])).unwrap();

        let updated = stdio_entry("srv", "echo", &["two"]);
        update_server_impl(&path, updated.clone()).unwrap();

        let reloaded = load_config_impl(&path).unwrap();
        assert_eq!(reloaded.servers.len(), 1);
        assert_eq!(reloaded.servers[0], updated);
    }

    #[test]
    fn update_errors_for_unknown_id() {
        let path = temp_path("update_missing.json");
        let err = update_server_impl(&path, stdio_entry("ghost", "echo", &[])).unwrap_err();
        assert!(
            err.contains("Unknown MCP server"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn remove_deletes_the_matching_entry_only() {
        let path = temp_path("remove.json");
        add_server_impl(&path, stdio_entry("a", "echo", &[])).unwrap();
        add_server_impl(&path, stdio_entry("b", "echo", &[])).unwrap();

        remove_server_impl(&path, "a").unwrap();

        let reloaded = load_config_impl(&path).unwrap();
        assert_eq!(reloaded.servers.len(), 1);
        assert_eq!(reloaded.servers[0].id, "b");
    }

    #[test]
    fn remove_of_unknown_id_is_a_no_op_success() {
        let path = temp_path("remove_missing.json");
        add_server_impl(&path, stdio_entry("a", "echo", &[])).unwrap();

        remove_server_impl(&path, "does-not-exist").unwrap();

        assert_eq!(load_config_impl(&path).unwrap().servers.len(), 1);
    }

    #[test]
    fn set_enabled_toggles_the_flag() {
        let path = temp_path("enable.json");
        add_server_impl(&path, stdio_entry("srv", "echo", &[])).unwrap();

        let updated = set_enabled_impl(&path, "srv", false).unwrap();
        assert!(!updated.enabled);
        assert!(!load_config_impl(&path).unwrap().servers[0].enabled);
    }

    #[test]
    fn set_enabled_errors_for_unknown_id() {
        let path = temp_path("enable_missing.json");
        let err = set_enabled_impl(&path, "ghost", true).unwrap_err();
        assert!(
            err.contains("Unknown MCP server"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn validate_id_rejects_empty_and_bad_chars() {
        assert!(validate_id("").is_err());
        assert!(validate_id("has space").is_err());
        assert!(validate_id("has/slash").is_err());
        assert!(validate_id("good_id-123").is_ok());
    }

    // Tests that connect/call a *real* stdio MCP server (spawning the
    // trivial `mcp_test_server` bin) live in `tests/mcp_stdio.rs`, and tests
    // that connect/call a *real* streamable-HTTP MCP server (a hand-rolled
    // raw-TCP responder speaking `rmcp`'s own JSON-RPC model types) live in
    // `tests/mcp_http.rs` — both integration tests rather than unit tests
    // here, since `tests/mcp_stdio.rs` needs `CARGO_BIN_EXE_<name>` (only
    // defined for integration tests, not `--lib` unit tests) and it's
    // simplest to keep the two real-server suites together.

    #[tokio::test]
    async fn connect_rejects_empty_http_url() {
        let state = AppState::default();
        let entry = McpServerEntry {
            id: "http-srv".to_string(),
            label: "HTTP server".to_string(),
            transport: McpTransport::Http {
                url: "   ".to_string(),
            },
            enabled: true,
            tool_allowlist: None,
            timeout_secs: None,
        };

        let err = connect_impl(&state, &entry).await.unwrap_err();
        assert!(err.contains("no URL configured"), "unexpected error: {err}");
    }

    // --- connection scope (D3) -------------------------------------------

    /// The load-bearing one: drives `connect_impl` itself from inside a run and checks
    /// what scope its body actually saw.
    ///
    /// Every other test in this section exercises `run_scope::scoped` directly, so all of
    /// them stay green with the wrapper on `connect_impl` deleted — which makes them pins
    /// on `run_scope`, not on this adoption. Reverting that wrapper turns this one red
    /// with `Some(Run("run:establishes-a-connection"))` where the connection reason
    /// belongs.
    ///
    /// Hermetic: the blank URL returns before any socket is opened, and the observation
    /// point is written before the transport branch is chosen.
    #[tokio::test]
    async fn connect_runs_under_the_connection_scope_even_when_a_run_established_it() {
        let entry = McpServerEntry {
            id: "http-srv".to_string(),
            label: "HTTP server".to_string(),
            transport: McpTransport::Http {
                url: "   ".to_string(),
            },
            enabled: true,
            tool_allowlist: None,
            timeout_secs: None,
        };

        let observed = crate::run_scope::scoped(
            crate::run_scope::RunScope::run("run:establishes-a-connection"),
            async {
                let state = AppState::default();
                // The caller really is inside a run at this point, which is the case the
                // reauth retry in `call_tool_with_cancel_impl` creates.
                assert_eq!(
                    crate::run_scope::current_run_id().as_deref(),
                    Some("run:establishes-a-connection")
                );
                let _ = connect_impl(&state, &entry).await;
                OBSERVED_CONNECT_SCOPE
                    .lock()
                    .expect("observation point")
                    .clone()
            },
        )
        .await;

        assert_eq!(
            observed,
            Some(CONNECTION_SCOPE),
            "connect-time work belongs to the connection, not to whichever run opened it"
        );

        // Counter-assertion: the probe can see a run when there is one to see, so the
        // result above is the shadowing working rather than the observation point being
        // blind. Without this, an implementation that never recorded anything would pass.
        crate::run_scope::scoped(crate::run_scope::RunScope::run("run:direct"), async {
            *OBSERVED_CONNECT_SCOPE.lock().expect("observation point") =
                crate::run_scope::current();
        })
        .await;
        assert_eq!(
            OBSERVED_CONNECT_SCOPE
                .lock()
                .expect("observation point")
                .as_ref()
                .and_then(crate::run_scope::RunScope::run_id),
            Some("run:direct")
        );
    }

    /// A connection's own work is the *connection's*, even when the call that
    /// establishes it came from inside a run — which the reauth retry in
    /// [`call_tool_with_cancel_impl`] does.
    ///
    /// The counter-assertion is the half that makes this test able to fail:
    /// without it, an implementation that simply never carried a run id would
    /// pass, and so would a probe that could not see a scope at all. So the same
    /// probe is run once inside the connection scope and once outside it, and the
    /// two answers have to differ.
    #[tokio::test]
    async fn a_connection_scopes_its_own_work_to_the_connection_and_not_the_calling_run() {
        let probe = || async { crate::run_scope::current() };

        let inside = crate::run_scope::scoped(
            crate::run_scope::RunScope::run("run:calls-a-tool"),
            crate::run_scope::scoped(CONNECTION_SCOPE, probe()),
        )
        .await;

        assert_eq!(
            inside
                .as_ref()
                .and_then(crate::run_scope::RunScope::unattributed)
                .map(|reason| reason.code()),
            Some("unattributed.shared-transport"),
            "a shared transport's own work must carry the connection's reason"
        );
        assert_eq!(
            inside.as_ref().and_then(crate::run_scope::RunScope::run_id),
            None,
            "and must not be billed to whichever run happened to trigger the connect"
        );

        // Same probe, same enclosing run, no connection scope: it sees the run.
        let outside =
            crate::run_scope::scoped(crate::run_scope::RunScope::run("run:calls-a-tool"), probe())
                .await;
        assert_eq!(
            outside.and_then(|scope| scope.run_id().map(str::to_string)),
            Some("run:calls-a-tool".to_string()),
            "the probe can see a run when one is in scope, so the assertion above is not vacuous"
        );
    }

    /// The real [`connect_impl`], end to end: entering a scope for the connection
    /// must not corrupt the caller's.
    ///
    /// Uses the blank-URL entry so the whole call is hermetic — it returns before
    /// any keychain read or socket — while still going through the actual scope
    /// wrapper rather than a stand-in for it.
    #[tokio::test]
    async fn a_connect_leaves_the_calling_run_s_scope_intact() {
        let state = AppState::default();
        let entry = McpServerEntry {
            id: "http-srv".to_string(),
            label: "HTTP server".to_string(),
            transport: McpTransport::Http {
                url: "   ".to_string(),
            },
            enabled: true,
            tool_allowlist: None,
            timeout_secs: None,
        };

        let after = crate::run_scope::scoped(crate::run_scope::RunScope::run("run:outer"), async {
            assert!(connect_impl(&state, &entry).await.is_err());
            crate::run_scope::current_run_id()
        })
        .await;

        assert_eq!(
            after.as_deref(),
            Some("run:outer"),
            "the run that asked for the connection keeps its identity afterwards"
        );
    }

    #[test]
    fn keychain_account_namespaces_by_mcp_prefix() {
        assert_eq!(keychain_account("my-server"), "mcp:my-server");
    }

    #[test]
    fn read_http_token_is_none_when_nothing_is_saved() {
        // A random, never-configured id: no keychain entry exists for it, so
        // this must return `None` rather than erroring — same "absence is
        // normal" stance as `providers::has_key`. Doesn't touch the keychain
        // beyond a single read of a nonexistent entry.
        assert_eq!(read_http_token("never-configured-mcp-server-id-xyz"), None);
    }

    #[tokio::test]
    async fn disconnect_of_unconnected_server_is_a_no_op() {
        let state = AppState::default();
        disconnect_impl(&state, "never-heard-of-it").await; // must not panic
    }

    #[tokio::test]
    async fn call_tool_errors_when_server_not_connected() {
        let state = AppState::default();
        let entry = stdio_entry("never-connected", "irrelevant-command", &[]);

        let err = call_tool_impl(&state, &entry, "echo", serde_json::json!({}))
            .await
            .unwrap_err();
        assert!(err.contains("is not connected"), "unexpected error: {err}");
    }

    #[tokio::test]
    async fn disconnect_all_with_no_connections_is_a_no_op() {
        let state = AppState::default();
        disconnect_all(&state).await; // must not panic or hang
        assert!(state.mcp.lock().await.is_empty());
    }

    #[test]
    fn concurrent_add_server_calls_under_the_config_lock_do_not_lose_updates() {
        // Regression test for the mcp_servers.json race: `mcp_add_server` is
        // a synchronous `#[tauri::command]`, which Tauri can dispatch onto
        // genuinely concurrent OS threads for real Settings actions. This
        // calls the exact same AppHandle-free core `mcp_add_server` now
        // delegates to (`add_server_with_state_impl`) across real parallel
        // threads sharing one `AppState`. Without `AppState::mcp_config_lock`
        // serializing the load-mutate-save cycle, two threads that both load
        // the same "before" config and save one after another would silently
        // drop one of their two new servers.
        let path = temp_path("concurrent_add.json");
        let state = AppState::default();

        std::thread::scope(|scope| {
            for i in 0..8 {
                let path = &path;
                let state = &state;
                scope.spawn(move || {
                    add_server_with_state_impl(
                        state,
                        path,
                        stdio_entry(&format!("concurrent-{i}"), "echo", &[]),
                    )
                    .unwrap();
                });
            }
        });

        let config = load_config_impl(&path).unwrap();
        assert_eq!(
            config.servers.len(),
            8,
            "a concurrent mcp_add_server call's entry was lost"
        );
    }

    #[test]
    fn update_server_revokes_stale_session_allow_grants_for_that_id() {
        let path = temp_path("update_revokes.json");
        let state = AppState::default();

        add_server_with_state_impl(&state, &path, stdio_entry("docs", "echo", &["v1"])).unwrap();
        state
            .permissions
            .session_allow
            .lock()
            .unwrap()
            .insert("mcp:docs:search".to_string());

        update_server_with_state_impl(&state, &path, stdio_entry("docs", "echo", &["v2"])).unwrap();

        assert!(
            !state
                .permissions
                .session_allow
                .lock()
                .unwrap()
                .contains("mcp:docs:search"),
            "a grant approved against the old transport must not survive an update"
        );
    }

    #[test]
    fn remove_then_add_with_the_same_id_revokes_the_old_grant() {
        let path = temp_path("remove_readd_revokes.json");
        let state = AppState::default();

        add_server_with_state_impl(&state, &path, stdio_entry("docs", "echo", &[])).unwrap();
        state
            .permissions
            .session_allow
            .lock()
            .unwrap()
            .insert("mcp:docs:search".to_string());

        remove_server_with_state_impl(&state, &path, "docs").unwrap();
        assert!(
            !state
                .permissions
                .session_allow
                .lock()
                .unwrap()
                .contains("mcp:docs:search"),
            "removing a server must revoke its grants"
        );

        // Reuse the id for a genuinely different server — a leftover grant
        // must never silently apply to it.
        add_server_with_state_impl(
            &state,
            &path,
            stdio_entry("docs", "curl", &["https://evil.example"]),
        )
        .unwrap();
        assert!(
            !state
                .permissions
                .session_allow
                .lock()
                .unwrap()
                .contains("mcp:docs:search"),
            "an id reused by a different server must not inherit the old server's grants"
        );
    }
}
