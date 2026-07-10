//! MCP (Model Context Protocol) client support — Phase 1 (stdio only).
//!
//! Users configure external MCP servers, persisted at
//! `<app_data>/mcp_servers.json` (atomic temp+rename writes, exactly like
//! `sessions.rs`/`memory.rs`). Each server is either a `Stdio` child process
//! (spawned via `rmcp`'s `TokioChildProcess`) or an `Http` remote endpoint —
//! the `Http` variant is a stub for now (phase 4); calling it returns a
//! clear "not yet supported" error rather than panicking or silently
//! dropping the config.
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
//! the bottom of this file) and reusable from `lm-cli` later, while the
//! `#[tauri::command]`s are thin wrappers that resolve the config path,
//! gate permission, and emit `mcp://status` events.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use rmcp::model::{CallToolRequestParams, CallToolResult};
use rmcp::service::RunningService;
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

/// How a configured MCP server is reached. `Http` is a phase-4 stub: the
/// variant exists so config shapes/UI can be built against it now, but
/// [`connect_impl`]/[`call_tool_impl`] refuse to act on it with a clear
/// error rather than attempting an unsupported connection.
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
fn validate_id(id: &str) -> Result<(), String> {
    if !id.is_empty() && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
        Ok(())
    } else {
        Err(format!(
            "Invalid MCP server id '{}': must be non-empty and contain only letters, digits, '-', or '_'",
            id
        ))
    }
}

/// Resolves (and creates, if missing) `<app_data_dir>/mcp_servers.json`'s path.
fn config_file_path(app: &tauri::AppHandle) -> Result<PathBuf, String> {
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
        Ok(raw) => serde_json::from_str(&raw).map_err(|e| format!("Corrupt mcp_servers.json: {}", e)),
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
    std::fs::write(&tmp, &payload).map_err(|e| format!("Failed to write mcp_servers.json: {}", e))?;
    std::fs::rename(&tmp, path).map_err(|e| format!("Failed to finalize mcp_servers.json: {}", e))?;
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
        return Err(format!("An MCP server with id '{}' already exists", entry.id));
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
    /// ([`mcp_list_tools`], [`call_tool_impl`]), not baked in here, so
    /// `mcp_list_servers`/the Settings tool-allowlist checkboxes can still
    /// show the full set of what a server offers.
    pub tools: Vec<CachedMcpTool>,
    pub instructions: Option<String>,
}

/// Connect to `entry` (stdio only in this phase — `Http` returns an error),
/// caching its tool list and `initialize`-result instructions. Replaces
/// (and gracefully closes) any previous connection for the same id.
///
/// AppHandle-free and directly unit-testable: see the tests at the bottom
/// of this file, which spawn a real (trivial) MCP server over stdio.
pub async fn connect_impl(
    state: &AppState,
    entry: &McpServerEntry,
) -> Result<(Vec<CachedMcpTool>, Option<String>), String> {
    let (command, args, env) = match &entry.transport {
        McpTransport::Stdio { command, args, env } => (command, args, env),
        McpTransport::Http { .. } => {
            return Err(
                "HTTP MCP servers are not supported yet — coming in a later update. Use a stdio server for now."
                    .to_string(),
            );
        }
    };

    if command.trim().is_empty() {
        return Err(format!("MCP server '{}' has no command configured", entry.id));
    }

    let mut command_builder = tokio::process::Command::new(command);
    command_builder.args(args).envs(env);

    let child = rmcp::transport::TokioChildProcess::new(command_builder)
        .map_err(|e| format!("Failed to spawn MCP server '{}': {}", entry.id, e))?;

    let service = rmcp::serve_client((), child)
        .await
        .map_err(|e| format!("Failed to initialize MCP server '{}': {}", entry.id, e))?;

    let tools: Vec<CachedMcpTool> = service
        .peer()
        .list_all_tools()
        .await
        .map_err(|e| format!("Failed to list tools for MCP server '{}': {}", entry.id, e))?
        .iter()
        .map(cache_tool)
        .collect();

    let instructions = service.peer_info().and_then(|info| info.instructions.clone());

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

/// Validates that `tool_name` is both currently offered by the connected
/// server AND permitted by `entry.tool_allowlist` (when set), then calls it
/// with `arguments`. Returns the server's `CallToolResult` verbatim — mapping
/// its content blocks into a string for the model is the frontend's job
/// (phase 2), same division of labor as `tool_run_shell` returning raw
/// stdout/stderr.
///
/// Deliberately does NOT itself gate permission, apply a timeout, or watch
/// for turn cancellation — see `mcp_call_tool` for all three, mirroring how
/// `tool_run_shell` layers those around its own core logic. Kept separate so
/// this dispatch-and-validate core is directly unit-testable without a
/// running Tauri app.
pub async fn call_tool_impl(
    state: &AppState,
    entry: &McpServerEntry,
    tool_name: &str,
    arguments: serde_json::Value,
) -> Result<CallToolResult, String> {
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
        other => return Err(format!("MCP tool arguments must be a JSON object, got: {}", other)),
    };

    let mut params = CallToolRequestParams::new(tool_name.to_string());
    params.arguments = arguments_obj;

    peer.call_tool(params)
        .await
        .map_err(|e| format!("MCP tool call to '{}' on '{}' failed: {}", tool_name, entry.id, e))
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

/// Add a new MCP server to the config. Does not connect it — call
/// `mcp_connect` separately (the Settings UI does this right after adding).
#[tauri::command(rename_all = "snake_case")]
pub fn mcp_add_server(app: tauri::AppHandle, entry: McpServerEntry) -> Result<McpServerEntry, String> {
    add_server_impl(&config_file_path(&app)?, entry)
}

/// Replace an existing MCP server's config by id. Does not reconnect —
/// callers that changed connection-affecting fields (command/args/env/url)
/// should follow up with `mcp_disconnect` + `mcp_connect`.
#[tauri::command(rename_all = "snake_case")]
pub fn mcp_update_server(app: tauri::AppHandle, entry: McpServerEntry) -> Result<McpServerEntry, String> {
    update_server_impl(&config_file_path(&app)?, entry)
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
    remove_server_impl(&config_file_path(&app)?, &server_id)
}

/// Enable or disable a configured server. Disabling a currently-connected
/// server also disconnects it — a disabled server must not keep a child
/// process (or its tools) alive.
#[tauri::command(rename_all = "snake_case")]
pub async fn mcp_set_enabled(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    server_id: String,
    enabled: bool,
) -> Result<McpServerEntry, String> {
    validate_id(&server_id)?;
    let updated = set_enabled_impl(&config_file_path(&app)?, &server_id, enabled)?;
    if !enabled {
        disconnect_impl(state.inner(), &server_id).await;
        emit_status(&app, &server_id, "disconnected", None, None);
    }
    Ok(updated)
}

/// Connect to a configured MCP server (stdio only in this phase), caching
/// its tool list. Emits `mcp://status` transitions through `"connecting"`
/// and then `"connected"`/`"error"`.
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

    match connect_impl(state.inner(), &entry).await {
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

/// One connected server's (allowlist-filtered) cached tools — what the
/// frontend actually merges into the model's tool set (phase 2), as
/// distinct from `mcp_list_servers`' unfiltered tool list (used to render
/// the Settings allowlist checkboxes).
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct McpServerTools {
    pub server_id: String,
    pub tools: Vec<CachedMcpTool>,
}

/// List cached tools for one connected server (`server_id = Some(..)`) or
/// every connected, enabled server (`None`) — called by the frontend before
/// each turn to build the merged tool set. Cheap: reads the in-memory cache
/// populated by `mcp_connect`, never re-queries the server.
#[tauri::command(rename_all = "snake_case")]
pub async fn mcp_list_tools(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    server_id: Option<String>,
) -> Result<Vec<McpServerTools>, String> {
    let config = load_config_impl(&config_file_path(&app)?)?;
    let guard = state.mcp.lock().await;

    let mut out = Vec::new();
    for entry in &config.servers {
        if !entry.enabled {
            continue;
        }
        if let Some(ref wanted) = server_id {
            if wanted != &entry.id {
                continue;
            }
        }
        let Some(connection) = guard.get(&entry.id) else {
            continue;
        };
        let tools = match &entry.tool_allowlist {
            Some(allow) => connection
                .tools
                .iter()
                .filter(|t| allow.iter().any(|a| a == &t.name))
                .cloned()
                .collect(),
            None => connection.tools.clone(),
        };
        out.push(McpServerTools { server_id: entry.id.clone(), tools });
    }
    Ok(out)
}

/// Call a tool on a connected MCP server. Permission-gated
/// (`mcp:<server_id>:<tool_name>`, previewing the tool name and
/// pretty-printed arguments — same convention as `tool_run_shell`'s command
/// preview), turn-scoped cancellable via the same `AppState::tool_cancel`
/// mechanism `tool_run_shell` uses, and bounded by a timeout (per-server
/// `timeout_secs`, default 60s).
#[tauri::command(rename_all = "snake_case")]
pub async fn mcp_call_tool(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    server_id: String,
    tool_name: String,
    arguments: serde_json::Value,
    turn_id: Option<String>,
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

    let outcome = tokio::select! {
        result = call_tool_impl(state.inner(), &entry, &tool_name, arguments) => result,
        _ = cancel.notified() => Err("MCP tool call cancelled by the user".to_string()),
        _ = tokio::time::sleep(timeout) => Err(format!(
            "MCP tool '{}' on server '{}' timed out after {} seconds",
            tool_name, server_id, timeout.as_secs()
        )),
    };

    // Drop this turn's cancel channel once no other MCP/shell call for the
    // same turn still holds it — same bookkeeping as `tool_run_shell`.
    {
        let mut guard = state
            .tool_cancel
            .lock()
            .map_err(|_| "Tool-cancel lock poisoned".to_string())?;
        if guard.get(&cancel_key).is_some_and(|n| std::sync::Arc::strong_count(n) <= 2) {
            guard.remove(&cancel_key);
        }
    }

    outcome
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

        assert!(!path.with_extension("json.tmp").exists(), "temp file must not linger");
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
        assert!(err.contains("Invalid MCP server id"), "unexpected error: {err}");
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
        assert!(err.contains("Unknown MCP server"), "unexpected error: {err}");
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
        assert!(err.contains("Unknown MCP server"), "unexpected error: {err}");
    }

    #[test]
    fn validate_id_rejects_empty_and_bad_chars() {
        assert!(validate_id("").is_err());
        assert!(validate_id("has space").is_err());
        assert!(validate_id("has/slash").is_err());
        assert!(validate_id("good_id-123").is_ok());
    }

    // Tests that connect/call a *real* stdio MCP server (spawning the
    // trivial `mcp_test_server` bin) live in `tests/mcp_stdio.rs` instead of
    // here: `CARGO_BIN_EXE_<name>` (needed to find that bin's compiled path)
    // is only defined for integration tests, not `--lib` unit tests — see
    // that file's module doc for details.

    #[tokio::test]
    async fn connect_rejects_http_transport_for_now() {
        let state = AppState::default();
        let entry = McpServerEntry {
            id: "http-srv".to_string(),
            label: "HTTP server".to_string(),
            transport: McpTransport::Http { url: "https://example.com/mcp".to_string() },
            enabled: true,
            tool_allowlist: None,
            timeout_secs: None,
        };

        let err = connect_impl(&state, &entry).await.unwrap_err();
        assert!(err.contains("not supported yet"), "unexpected error: {err}");
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
}
