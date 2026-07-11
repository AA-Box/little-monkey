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
mod permissions;
// `pub` (not `mod`, like `permissions`/`sessions`/`tools`) so `lm-cli`
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
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let app = tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_shell::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_fs::init())
        .manage(AppState::default())
        .invoke_handler(tauri::generate_handler![
            llama::llama_start,
            llama::llama_stop,
            llama::llama_status,
            server::api_server_start,
            server::api_server_stop,
            server::api_server_status,
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
