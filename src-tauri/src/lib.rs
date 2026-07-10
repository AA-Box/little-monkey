pub mod checkpoints;
mod git;
mod llama;
mod models;
pub mod ollama;
pub mod providers;
mod sessions;
mod system;
mod tools;
mod permissions;
pub mod workspace;

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
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_shell::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_fs::init())
        .manage(AppState::default())
        .invoke_handler(tauri::generate_handler![
            llama::llama_start,
            llama::llama_stop,
            llama::llama_status,
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
            sessions::sessions_load,
            sessions::sessions_save,
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
            system::reveal_in_finder,
            system::open_in_terminal,
            system::open_in_editor,
            system::open_session_window,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
