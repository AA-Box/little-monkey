// `pub` so every module below (and `monkey-cli`, which has no `AppHandle`)
// resolves the app-data directory through one shared `data_dir()` instead of
// each hardcoding the same identifier string independently — see the module
// doc for the drift risk this replaces.
pub mod app_paths;
// `pub` so a future `monkey-cli` parity command (matching `checkpoints`/`rules`/
// `memory`/`web`/`verify` above) could reuse `publish_impl`/`remove_impl`
// directly — no such command exists yet (rendering has no terminal surface,
// per the design doc's phase-4 note), but there's no reason to make this one
// module-private when every sibling with reusable core logic already isn't.
pub mod artifacts;
pub mod checkpoints;
// `pub` only for the doc-comment convention every sibling module below
// follows (a future `monkey-cli` command could call `install_if_needed`
// directly, though none exists yet — the CLI installing itself onto its own
// `PATH` isn't a meaningful operation).
pub mod cli_install;
mod git;
// `pub` so `monkey-cli`'s `embed_cli` module (RAG design doc slice 4 CLI parity)
// can reuse `find_llama_server_binary`/`embed_server_args`/`EMBED_PORT`/
// `LlamaState::for_embeddings` directly instead of re-implementing the
// embeddings-only `llama-server` process's binary discovery and flags — the
// same AppHandle-free-core reasoning as `stacks`/`checkpoints`/`rules` below,
// just exposing a few specific items rather than a `*_impl` set (the rest of
// this module's Tauri-command surface stays desktop-app-only).
pub mod llama;
pub mod mcp;
mod models;
pub mod ollama;
pub mod providers;
// `pub` so a future `monkey-cli` `Stacks` subcommand (RAG design doc, slice 4)
// can call `stacks::list_impl`/`reindex_impl`/`query_impl` directly, the
// same AppHandle-free-core reasoning as `checkpoints`/`rules`/`memory`.
pub mod stacks;
// `pub` so `monkey-cli` (slice 4) can reuse `load_impl`/`PromptEntry` directly,
// the same reasoning as `rules`/`checkpoints` above.
pub mod prompts;
mod sessions;
mod system;
mod tools;
// `pub` (unlike `sessions`/`tools`/`system`/`models`/`git`/`llama` above) so
// `monkey-cli` (Plan/Act + risk-adaptive permissions design doc, phase 4) can
// call `permissions::path_risk_floor` directly for its own floor-only
// `"smart"` mode — the same AppHandle-free-core reasoning as `web`/`rules`/
// `memory`/`verify` above. Every other item in this module (the Tauri
// commands, `PermissionState`, `request_permission`) stays reachable too,
// but is only ever actually called from `main.rs`'s own Tauri app wiring,
// not from `monkey-cli`.
pub mod permissions;
// `pub` (not `mod`, like `sessions`/`tools`/`system` above) so `monkey-cli`
// (slice 5) can call `read_rules_impl`/`load_impl`/`add_fact_impl` directly
// from `little_monkey_lib`, the same way it already reuses `checkpoints`.
pub mod rules;
pub mod memory;
pub mod workspace;
// `pub` so `monkey-cli` (phase 4) can call `web::fetch_impl` directly, the same
// AppHandle-free-core reasoning as `checkpoints`/`rules`/`memory` above.
pub mod web;
// `pub` so a future `monkey-cli` `api-serve` subcommand (design doc phase 4) can
// call `server::handle_request` directly for headless use, the same
// AppHandle-free-core reasoning as `web`/`rules`/`memory` above.
pub mod server;
// `pub` so `monkey-cli` (a later slice) can call `verify::run_command_impl`
// directly, the same AppHandle-free-core reasoning as `web`/`rules`/`memory`
// above.
pub mod verify;
// `pub` so `monkey-cli`'s `task.rs` (design doc slice 1) can call
// `parse_recipe`/`render_recipe`/`discover_recipes`/`resolve_recipe`
// directly, the same AppHandle-free-core reasoning as every other module
// above.
pub mod recipes;
// `pub` so a future `monkey-cli` `task schedule` subcommand could reuse
// `validate_cron_impl`/`next_occurrences_impl` directly, the same
// AppHandle-free-core reasoning as every other module above — no such
// subcommand exists yet (it emits a launchd/crontab line, no in-process
// scheduling), but there's no reason to make this one module-private either.
pub mod automations;

// `Manager` brings `AppHandle::state`/`state::<T>()` into scope — used by
// `run()`'s `RunEvent::Exit` handler below to reach `AppState::mcp` for
// `mcp::disconnect_all`.
use tauri::Manager;

/// Shared application state, managed by Tauri and accessed from every
/// #[tauri::command] via `tauri::State<'_, AppState>`.
///
/// No `#[derive(Default)]`: `embed_llama` needs a different starting port
/// (8091) than `llama`'s (8090), which a derived `Default` (calling
/// `LlamaState::default()` for both) can't express. See the manual `impl
/// Default for AppState` below.
pub struct AppState {
    pub llama: std::sync::Mutex<llama::LlamaState>,
    /// The second, embeddings-only managed `llama-server` instance (port
    /// 8091, started with `--embeddings --pooling mean`) used by
    /// `stacks.rs`'s managed-llama embedding backend — a distinct
    /// `LlamaState` from `llama` above (not one process serving both
    /// roles), so a stack reindex never contends with the chat model for
    /// the same server slot. See `llama::embed_server_start`.
    pub embed_llama: std::sync::Mutex<llama::LlamaState>,
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
    /// Serializes the permission-granted mutation itself (checkpoint backup +
    /// the actual file write) in `tool_write_file`/`tool_edit_file` — same
    /// "two unsynchronized concurrent writers can silently clobber each
    /// other" reasoning as `memory_lock` above, now reachable from ordinary
    /// agent use since subagents (p3) let multiple `code`-profile `task`
    /// calls run genuinely concurrently in the same round
    /// (`agentLoop.ts::runToolCallsForRound`) and share the parent turn's
    /// checkpoint id. Without this, two concurrent calls that both resolve to
    /// the SAME workspace path can interleave past `request_permission`'s
    /// `.await` and race on `checkpoints::record_original` + the mutation
    /// itself, silently discarding whichever write lands first with no error
    /// surfaced to either caller. Acquired only around the synchronous
    /// backup+write critical section (after permission has already been
    /// granted) — never across an `.await` — so a plain `std::sync::Mutex` is
    /// enough, same as `mcp_config_lock`/`web_settings_lock`. A single
    /// workspace-wide lock (not keyed per-path) is deliberately simpler than a
    /// per-path lock map: file writes are fast, and correctness here matters
    /// far more than intra-turn write parallelism.
    pub file_write_lock: std::sync::Mutex<()>,
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
    /// Lazily-loaded (chunks + vectors) cache for `stacks_query`, keyed by
    /// stack id, invalidated (removed) whenever a stack is reindexed or
    /// deleted — see `stacks.rs::load_stack_cached`.
    pub stack_cache: std::sync::Mutex<std::collections::HashMap<String, std::sync::Arc<stacks::LoadedStack>>>,
    /// Cancellation handles for in-flight `stacks_reindex` calls, keyed by
    /// stack id — mirrors `stream_cancels`/`tool_cancel` above, but a
    /// `tokio_util::sync::CancellationToken` rather than a plain
    /// `tokio::sync::Notify`: a `CancellationToken`'s cancelled state is
    /// persisted, so a cancel request is never silently lost just because it
    /// arrived before anything happened to be awaiting it (`Notify`'s
    /// `notify_waiters()` has exactly that gap — see
    /// `stacks::reindex_impl`'s doc comment for the failure mode this
    /// avoids). See `stacks::stacks_cancel_index`.
    pub index_cancels: std::sync::Mutex<std::collections::HashMap<String, std::sync::Arc<tokio_util::sync::CancellationToken>>>,
}

impl Default for AppState {
    fn default() -> Self {
        AppState {
            llama: Default::default(),
            embed_llama: std::sync::Mutex::new(llama::LlamaState::for_embeddings()),
            ollama: Default::default(),
            workspace_roots: Default::default(),
            permissions: Default::default(),
            stream_cancels: Default::default(),
            checkpoints: Default::default(),
            checkpoint_locks: Default::default(),
            tool_cancel: Default::default(),
            memory_lock: Default::default(),
            mcp_config_lock: Default::default(),
            file_write_lock: Default::default(),
            mcp: Default::default(),
            web_settings_lock: Default::default(),
            api_server: Default::default(),
            api_server_config_lock: Default::default(),
            artifacts: Default::default(),
            stack_cache: Default::default(),
            index_cancels: Default::default(),
        }
    }
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
            // Best-effort, silent `monkey` PATH shim install — see
            // `cli_install.rs`'s module doc. Spawned on a blocking thread
            // (it does filesystem/registry I/O, no `.await` points) so
            // `setup` still returns promptly; failures are swallowed here on
            // purpose, matching every other autostart-style setup step in
            // this function — a missing/failed CLI shim is never worth
            // interrupting the GUI launching.
            tauri::async_runtime::spawn_blocking(|| {
                let _ = cli_install::install_if_needed();
            });

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
            cli_install::cli_install_status,
            cli_install::cli_install_set_enabled,
            llama::llama_start,
            llama::llama_stop,
            llama::llama_status,
            llama::embed_server_start,
            llama::embed_server_stop,
            llama::embed_server_status,
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
            permissions::set_permission_mode_for_turn,
            permissions::clear_permission_mode_for_turn,
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
            stacks::stacks_list,
            stacks::stacks_create,
            stacks::stacks_delete,
            stacks::stacks_rename,
            stacks::stacks_add_source,
            stacks::stacks_remove_source,
            stacks::stacks_reindex,
            stacks::stacks_cancel_index,
            stacks::stacks_query,
            stacks::stacks_is_stale,
            stacks::tool_search_docs,
            recipes::recipes_list,
            recipes::recipes_read,
            recipes::recipes_read_raw,
            recipes::recipes_render,
            recipes::recipes_save,
            recipes::recipes_delete,
            recipes::recipes_validate,
            automations::automations_load,
            automations::automations_save,
            automations::cron_validate,
            automations::cron_next,
            automations::cron_previous,
        ])
        .build(tauri::generate_context!())
        .expect("error while building tauri application");

    app.run(|app_handle, event| {
        // `App::run` never returns — once the event loop is done, the
        // underlying `tao` runtime calls `std::process::exit` directly
        // (see its own doc comment), which skips Rust's Drop-based cleanup
        // entirely. That means any live MCP stdio child process (held in
        // `AppState::mcp`, cleaned up only via `McpConnection::service`'s
        // `Drop`/`.cancel()`) — and either managed `llama-server` child
        // process (`AppState::llama`/`AppState::embed_llama`, neither of
        // which has a `Drop` impl either) — would otherwise be silently
        // orphaned on every normal app quit. `RunEvent::Exit` fires
        // synchronously on the main thread right before that happens, so
        // doing both kinds of cleanup here — `mcp::disconnect_all` (bounded
        // and best-effort — see its own doc comment) and
        // `llama::stop_all_blocking` (synchronous, no `AppHandle`/async
        // runtime required, since a plain `std::process::Child::kill` is all
        // either process needs) — is the only chance those child processes
        // get to actually be killed before the process itself exits.
        if let tauri::RunEvent::Exit = event {
            let state = app_handle.state::<AppState>();
            tauri::async_runtime::block_on(mcp::disconnect_all(state.inner()));
            llama::stop_all_blocking(state.inner());
        }
    });
}
