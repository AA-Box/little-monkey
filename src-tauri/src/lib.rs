// `pub` so a future `lm-cli` parity command (matching `checkpoints`/`rules`/
// `memory`/`web`/`verify` above) could reuse `publish_impl`/`remove_impl`
// directly — no such command exists yet (rendering has no terminal surface,
// per the design doc's phase-4 note), but there's no reason to make this one
// module-private when every sibling with reusable core logic already isn't.
pub mod artifacts;
pub mod checkpoints;
mod git;
mod llama;
pub mod mcp;
mod models;
pub mod ollama;
pub mod providers;
// `pub` so `lm-cli` (slice 4) can reuse `load_impl`/`PromptEntry` directly,
// the same reasoning as `rules`/`checkpoints` above.
pub mod prompts;
mod sessions;
mod system;
mod tools;
// `pub` (unlike `sessions`/`tools`/`system`/`models`/`git`/`llama` above) so
// `lm-cli` (Plan/Act + risk-adaptive permissions design doc, phase 4) can
// call `permissions::path_risk_floor` directly for its own floor-only
// `"smart"` mode — the same AppHandle-free-core reasoning as `web`/`rules`/
// `memory`/`verify` above. Every other item in this module (the Tauri
// commands, `PermissionState`, `request_permission`) stays reachable too,
// but is only ever actually called from `main.rs`'s own Tauri app wiring,
// not from `lm-cli`.
pub mod permissions;
// `pub` (not `mod`, like `sessions`/`tools`/`system` above) so `lm-cli`
// (slice 5) can call `read_rules_impl`/`load_impl`/`add_fact_impl` directly
// from `little_monkey_lib`, the same way it already reuses `checkpoints`.
pub mod rules;
pub mod memory;
pub mod workspace;
// `pub` so `lm-cli` (phase 4) can call `web::fetch_impl` directly, the same
// AppHandle-free-core reasoning as `checkpoints`/`rules`/`memory` above.
pub mod web;
// `pub` so a future `lm-cli` `api-serve` subcommand (design doc phase 4) can
// call `server::handle_request` directly for headless use, the same
// AppHandle-free-core reasoning as `web`/`rules`/`memory` above.
pub mod server;
// `pub` so `lm-cli` (a later slice) can call `verify::run_command_impl`
// directly, the same AppHandle-free-core reasoning as `web`/`rules`/`memory`
// above.
pub mod verify;

// `Manager` brings `AppHandle::state`/`state::<T>()` into scope — used by
// `run()`'s `RunEvent::Exit` handler below to reach `AppState::mcp` for
// `mcp::disconnect_all`.
use tauri::Manager;

/// Shared application state, managed by Tauri and accessed from every
/// #[tauri::command] via `tauri::State<'_, AppState>`.
#[derive(Default)]
pub struct AppState {
    pub llama: std::sync::Mutex<llama::LlamaState>,
    pub ollama: std::sync::Mutex<ollama::OllamaState>,
    /// Attached workspace folders, primary first. Empty means no workspace
    /// is open. See `workspace.rs`.
    pub workspace_roots: std::sync::Mutex<Vec<workspace::WorkspaceRoot>>,
    pub permissions: permissions::PermissionState,
    /// Cancellation handles for in-flight `providers_stream_chat` requests,
    /// keyed by `request_id` — see `providers::providers_cancel_chat`.
    pub stream_cancels: std::sync::Mutex<std::collections::HashMap<String, std::sync::Arc<tokio::sync::Notify>>>,
    /// Per-turn file checkpoints currently in flight, keyed by checkpoint id.
    /// With the split pane open, two turns (and thus two checkpoints) can be
    /// active concurrently — see `checkpoints.rs`.
    pub checkpoints: std::sync::Mutex<std::collections::HashMap<String, checkpoints::ActiveCheckpoint>>,
    /// Checkpoint ids with a `checkpoint_revert`/`checkpoint_reapply` call
    /// currently in progress. `MessageList.tsx`'s `CheckpointRow` and
    /// `CheckpointTimeline.tsx`'s `TimelineRow` can both render controls for
    /// the same checkpoint at once, and both call these commands with only a
    /// component-local `busy` flag guarding each — nothing shared prevents
    /// two concurrent revert/reapply calls for the same id from racing on
    /// the same `redo/<n>.bak` files. Membership here is that lock — see
    /// `checkpoints::acquire_revert_lock`.
    pub checkpoint_locks: std::sync::Mutex<std::collections::HashSet<String>>,
    /// Per-turn cancellation channels used by `tools::tools_cancel_running`
    /// to kill in-flight `tool_run_shell` child processes when the user hits
    /// Stop — keyed by the owning turn's id (empty string for callers that
    /// don't thread one) so stopping one pane's turn never kills a command
    /// the other pane's turn is still running.
    pub tool_cancel: std::sync::Mutex<std::collections::HashMap<String, std::sync::Arc<tokio::sync::Notify>>>,
    /// Serializes `memories.json` read-modify-write cycles (see `memory.rs`)
    /// so two concurrent split-pane `tool_remember` calls can never race and
    /// clobber each other's fact — the whole file is rewritten on every add
    /// or delete, so unsynchronized concurrent writers could silently drop
    /// one of them.
    pub memory_lock: std::sync::Mutex<()>,
    /// Serializes `mcp_servers.json` read-modify-write cycles (see `mcp.rs`)
    /// — same reasoning as `memory_lock` above protects `memories.json`.
    /// `mcp_add_server`/`mcp_update_server` are synchronous commands (Tauri
    /// can dispatch those on genuinely concurrent OS threads) and
    /// `mcp_remove_server`/`mcp_set_enabled` are async commands (the tokio
    /// runtime can run those in parallel too), so without a shared lock two
    /// concurrent config-mutating calls (e.g. two Settings toggles fired
    /// close together) can both load the same "before" config and the
    /// later save silently clobbers the earlier one's change. A plain
    /// `std::sync::Mutex`, not `tokio::sync::Mutex` like `AppState::mcp`:
    /// every critical section this guards is the synchronous
    /// `load_config_impl`/`save_config_impl` pair with no `.await` in
    /// between, so there's nothing async to ever hold it across.
    pub mcp_config_lock: std::sync::Mutex<()>,
    /// Live MCP server connections, keyed by server id (see `mcp.rs`). A
    /// `tokio::sync::Mutex` — unlike every other map here — because
    /// connecting and calling a tool are both `.await`-ing operations; every
    /// caller clones the cheap `Peer` handle out (or swaps the whole
    /// connection in/out) and drops the guard before awaiting anything on
    /// the connection itself, so this lock is never held across a
    /// `call_tool`/`connect`/`disconnect` await.
    pub mcp: tokio::sync::Mutex<std::collections::HashMap<String, mcp::McpConnection>>,
    /// Serializes `web_settings.json` writes (see `web.rs::web_set_settings`)
    /// — same reasoning as `mcp_config_lock` protects `mcp_servers.json`.
    /// `web_set_settings` is a synchronous command (Tauri can dispatch two
    /// calls onto genuinely concurrent OS threads), and its save is a plain
    /// temp-file-write-then-rename with a deterministic temp path; without
    /// this lock, two concurrent saves can both `std::fs::write` the same
    /// `web_settings.json.tmp` at once and leave a torn/interleaved file
    /// behind for whichever `rename` lands last to publish. A plain
    /// `std::sync::Mutex`, not `tokio::sync::Mutex`: the guarded section is
    /// synchronous with no `.await` in between.
    pub web_settings_lock: std::sync::Mutex<()>,
    /// In-memory lifecycle state for the local OpenAI-compatible API server
    /// (`server.rs`) — mirrors `llama: Mutex<llama::LlamaState>` above.
    /// Binds `127.0.0.1` only and exposes just `/health`, `/v1/models`,
    /// `/v1/chat/completions` — never the agent tools (`tool_run_shell` et
    /// al. over HTTP would be a remote-code-execution surface, an explicit
    /// non-goal of this feature — see `server.rs`'s module doc).
    pub api_server: std::sync::Mutex<server::ApiServerState>,
    /// Serializes `api_server.json` read-modify-write cycles (config
    /// get/set, token create/revoke, and the per-request `last_used_at`
    /// bump) — same reasoning as `web_settings_lock`/`mcp_config_lock`
    /// protect their own files: several of these are synchronous commands
    /// Tauri can dispatch onto genuinely concurrent OS threads, and without
    /// a shared lock two concurrent read-modify-write cycles (e.g. minting
    /// two tokens back to back, or a token-used bump racing a revoke) could
    /// both load the same "before" file and the later save silently
    /// clobbers the earlier one's change.
    pub api_server_config_lock: std::sync::Mutex<()>,
    /// Published tier-2 (interactive HTML) artifacts, keyed by a
    /// server-generated uuid — served by the `artifact://` custom protocol
    /// registered below. See `artifacts.rs`'s module doc for the full
    /// security model; content lives only here in memory, never on disk.
    pub artifacts: std::sync::Mutex<std::collections::HashMap<String, artifacts::PublishedArtifact>>,
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let app = tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_shell::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_fs::init())
        .manage(AppState::default())
        // Tier-2 interactive-artifact protocol — serves a previously
        // `artifact_publish`-ed document by id with a strict per-document
        // CSP (`connect-src 'none'`, no capability granted to this scheme —
        // see `artifacts.rs`'s module doc for the full security argument).
        // The frontend's consuming iframe uses `sandbox="allow-scripts"`
        // WITHOUT `allow-same-origin`, so this protocol is never reachable
        // with IPC/cookies/storage regardless of what it serves.
        .register_uri_scheme_protocol("artifact", |ctx, request| {
            artifacts::handle_request(ctx.app_handle().state::<AppState>().inner(), &request)
        })
        .setup(|app| {
            // Autostart the local API server if `api_server.json` says to —
            // the only reader of that file at launch time, since every
            // other consumer (the Settings panel) fetches it on demand via
            // `api_server_get_config`. Spawned rather than awaited here:
            // `setup` must return promptly, and a bind failure just leaves
            // the server in its default "stopped"/"error" state for the
            // user to retry from the panel, the same as any other failed
            // manual start.
            let app_handle = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                let Ok(path) = server::config_file_path(&app_handle) else { return };
                let Ok(config) = server::load_config_impl(&path) else { return };
                if config.autostart {
                    let _ = server::api_server_start(app_handle).await;
                }
            });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            llama::llama_start,
            llama::llama_stop,
            llama::llama_status,
            server::api_server_start,
            server::api_server_stop,
            server::api_server_status,
            server::api_server_get_config,
            server::api_server_set_config,
            server::api_server_create_token,
            server::api_server_revoke_token,
            server::api_server_list_tokens,
            ollama::ollama_status,
            ollama::ollama_start,
            ollama::ollama_list_models,
            ollama::ollama_example_cloud_tags,
            ollama::ollama_pull_model,
            ollama::ollama_import_model,
            ollama::ollama_remove_model,
            ollama::ollama_signin,
            providers::providers_list_presets,
            providers::providers_list_configured,
            providers::providers_add_custom,
            providers::providers_remove_custom,
            providers::providers_set_key,
            providers::providers_remove_key,
            providers::providers_list_models,
            providers::providers_stream_chat,
            providers::providers_cancel_chat,
            models::models_list_curated,
            models::models_list_installed,
            models::models_download,
            models::models_delete,
            models::models_add_external,
            models::models_remove_external,
            permissions::permission_respond,
            permissions::set_permission_mode,
            permissions::get_permission_mode,
            tools::tool_read_file,
            tools::tool_list_dir,
            tools::tool_grep,
            tools::tool_glob,
            tools::tool_write_file,
            tools::tool_edit_file,
            tools::tool_run_shell,
            tools::tools_cancel_running,
            tools::list_workspace_paths,
            tools::tool_remember,
            web::tool_web_fetch,
            web::tool_web_search,
            web::web_get_settings,
            web::web_set_settings,
            web::web_has_brave_key,
            web::web_set_brave_key,
            web::web_remove_brave_key,
            rules::rules_read,
            rules::rules_write,
            memory::memory_list,
            memory::memory_add,
            memory::memory_delete,
            memory::memory_update,
            memory::memory_clear,
            sessions::sessions_load,
            sessions::sessions_save,
            prompts::prompts_load,
            prompts::prompts_save,
            prompts::prompts_read_external,
            prompts::prompts_write_external,
            checkpoints::checkpoint_begin,
            checkpoints::checkpoint_end,
            checkpoints::checkpoint_revert,
            checkpoints::checkpoint_reapply,
            checkpoints::checkpoint_list,
            artifacts::artifact_publish,
            artifacts::artifact_remove,
            workspace::set_primary_workspace_root,
            workspace::add_secondary_workspace_root,
            workspace::remove_secondary_workspace_root,
            workspace::get_workspace_roots,
            workspace::get_recent_workspaces,
            git::git_status,
            git::git_commit,
            mcp::mcp_list_servers,
            mcp::mcp_add_server,
            mcp::mcp_update_server,
            mcp::mcp_remove_server,
            mcp::mcp_set_enabled,
            mcp::mcp_connect,
            mcp::mcp_disconnect,
            mcp::mcp_set_http_token,
            mcp::mcp_remove_http_token,
            mcp::mcp_list_tools,
            mcp::mcp_call_tool,
            system::reveal_in_finder,
            system::open_in_terminal,
            system::open_in_editor,
            system::open_session_window,
            verify::verify_get_config,
            verify::verify_set_config,
            verify::verify_run,
        ])
        .build(tauri::generate_context!())
        .expect("error while building tauri application");

    app.run(|app_handle, event| {
        // `App::run` never returns — once the event loop is done, the
        // underlying `tao` runtime calls `std::process::exit` directly
        // (see its own doc comment), which skips Rust's Drop-based cleanup
        // entirely. That means any live MCP stdio child process (held in
        // `AppState::mcp`, cleaned up only via `McpConnection::service`'s
        // `Drop`/`.cancel()`) would otherwise be silently orphaned on every
        // normal app quit. `RunEvent::Exit` fires synchronously on the main
        // thread right before that happens, so blocking here on
        // `mcp::disconnect_all` (bounded and best-effort — see its own doc
        // comment) is the only chance those child processes get to actually
        // be killed before the process itself exits.
        if let tauri::RunEvent::Exit = event {
            let state = app_handle.state::<AppState>();
            tauri::async_runtime::block_on(mcp::disconnect_all(state.inner()));
        }
    });
}
