//! Thin Tauri bridge for the data-only native `SKILL.md` runtime.
//!
//! Filesystem and Git work runs on Tauri's blocking pool. Workspace installs
//! are always rooted at the host-owned primary workspace, never at a frontend
//! supplied destination. Signed M4 skills are converted to inert descriptors
//! and collision-checked by the shared core on each discovery.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::Duration;

use crate::m4_commands::M4CommandState;
use crate::native_skills::{
    ExternalSignedSkill, GitBulkApproval, GitSkillPreviewOutcome, GitSkillRequest,
    NativeSkillManager, SkillDescriptor, SkillInstallPreview, SkillMutationResult, SkillScope,
};
use crate::AppState;
use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use tauri::{Emitter, Manager};

pub const NATIVE_SKILLS_CHANGED_EVENT: &str = "native-skills://changed";

pub struct NativeSkillsCommandState {
    pub manager: Arc<NativeSkillManager>,
    watcher: Arc<NativeSkillWatcher>,
}

struct NativeSkillWatchHandle {
    _watcher: RecommendedWatcher,
}

#[derive(Default)]
struct NativeSkillWatcher {
    handles: Mutex<BTreeMap<PathBuf, NativeSkillWatchHandle>>,
}

impl NativeSkillWatcher {
    fn sync(
        self: &Arc<Self>,
        app: &tauri::AppHandle,
        manager: &Arc<NativeSkillManager>,
        workspace: Option<&Path>,
    ) -> Result<(), String> {
        let desired = manager
            .watch_targets(workspace)
            .map_err(|error| error.to_string())?
            .into_iter()
            .collect::<BTreeMap<_, _>>();
        let mut handles = self
            .handles
            .lock()
            .map_err(|_| "Native skill watcher lock poisoned".to_string())?;
        handles.retain(|path, _| desired.contains_key(path));
        for (path, recursive) in desired {
            if handles.contains_key(&path) {
                continue;
            }
            let (tx, rx) = mpsc::channel::<()>();
            let mut watcher =
                notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
                    if event.is_ok() {
                        let _ = tx.send(());
                    }
                })
                .map_err(|error| format!("create native skill watcher: {error}"))?;
            watcher
                .watch(
                    &path,
                    if recursive {
                        RecursiveMode::Recursive
                    } else {
                        RecursiveMode::NonRecursive
                    },
                )
                .map_err(|error| format!("watch native skill path {}: {error}", path.display()))?;
            let app = app.clone();
            let watcher_state = Arc::clone(self);
            let manager = Arc::clone(manager);
            thread::spawn(move || {
                while rx.recv().is_ok() {
                    while rx.recv_timeout(Duration::from_millis(150)).is_ok() {}
                    let _ = watcher_state.sync(
                        &app,
                        &manager,
                        optional_primary_workspace(app.state::<AppState>().inner())
                            .ok()
                            .flatten()
                            .as_deref(),
                    );
                    let _ = app.emit(NATIVE_SKILLS_CHANGED_EVENT, "filesystem");
                }
            });
            handles.insert(path, NativeSkillWatchHandle { _watcher: watcher });
        }
        Ok(())
    }
}

impl NativeSkillsCommandState {
    pub fn production(app_data_dir: &Path) -> Result<Self, String> {
        Ok(Self {
            manager: Arc::new(NativeSkillManager::new(app_data_dir).map_err(command_error)?),
            watcher: Arc::new(NativeSkillWatcher::default()),
        })
    }
}

pub(crate) fn sync_native_skill_watchers(app: &tauri::AppHandle) {
    let native = app.state::<NativeSkillsCommandState>();
    let workspace = match optional_primary_workspace(app.state::<AppState>().inner()) {
        Ok(workspace) => workspace,
        Err(error) => {
            eprintln!("native skill watcher workspace lookup failed: {error}");
            None
        }
    };
    if let Err(error) = native
        .watcher
        .sync(app, &native.manager, workspace.as_deref())
    {
        eprintln!("native skill watcher setup failed: {error}");
    }
}

fn emit_native_skills_changed(window: &tauri::Window) {
    let _ = window
        .app_handle()
        .emit(NATIVE_SKILLS_CHANGED_EVENT, window.label());
}

fn notify_mutation<T>(window: &tauri::Window, result: Result<T, String>) -> Result<T, String> {
    if result.is_ok() {
        emit_native_skills_changed(window);
    }
    result
}

#[tauri::command]
pub async fn native_skills_discover(
    native: tauri::State<'_, NativeSkillsCommandState>,
    m4: tauri::State<'_, M4CommandState>,
    app: tauri::State<'_, AppState>,
) -> Result<Vec<SkillDescriptor>, String> {
    let workspace = optional_primary_workspace(&app)?;
    let packages = m4
        .packages
        .active_skills()
        .map_err(|error| error.to_string())?
        .into_iter()
        .map(|skill| ExternalSignedSkill {
            package_id: skill.package_id,
            name: skill.name,
            description: skill.description,
            command: skill.command,
            version: skill.version.to_string(),
            instructions: skill.instructions,
            sha256: skill.content_sha256,
            permissions: skill
                .permissions
                .into_iter()
                .map(|permission| permission.permission_id)
                .collect(),
        })
        .collect::<Vec<_>>();
    let manager = native.manager.clone();
    run_blocking(move || manager.discover(workspace.as_deref(), &packages)).await
}

#[tauri::command]
pub async fn native_skills_preview_local(
    window: tauri::Window,
    native: tauri::State<'_, NativeSkillsCommandState>,
    app: tauri::State<'_, AppState>,
    source_path: String,
    scope: SkillScope,
) -> Result<SkillInstallPreview, String> {
    require_main_window(&window)?;
    let workspace = workspace_for_scope(&app, scope)?;
    let manager = native.manager.clone();
    let source = PathBuf::from(source_path);
    run_blocking(move || manager.preview_local(&source, scope, workspace.as_deref())).await
}

#[tauri::command]
pub async fn native_skills_install_local(
    window: tauri::Window,
    native: tauri::State<'_, NativeSkillsCommandState>,
    app: tauri::State<'_, AppState>,
    source_path: String,
    scope: SkillScope,
    approval_digest: String,
    approved: bool,
) -> Result<SkillMutationResult, String> {
    require_main_window(&window)?;
    let workspace = workspace_for_scope(&app, scope)?;
    let manager = native.manager.clone();
    let source = PathBuf::from(source_path);
    notify_mutation(
        &window,
        run_blocking(move || {
            manager.install_local(
                &source,
                scope,
                workspace.as_deref(),
                &approval_digest,
                approved,
            )
        })
        .await,
    )
}

#[tauri::command]
pub async fn native_skills_preview_git(
    window: tauri::Window,
    native: tauri::State<'_, NativeSkillsCommandState>,
    app: tauri::State<'_, AppState>,
    request: GitSkillRequest,
    scope: SkillScope,
) -> Result<GitSkillPreviewOutcome, String> {
    require_main_window(&window)?;
    let workspace = workspace_for_scope(&app, scope)?;
    let manager = native.manager.clone();
    run_blocking(move || manager.preview_git(&request, scope, workspace.as_deref())).await
}

#[tauri::command]
pub async fn native_skills_install_git(
    window: tauri::Window,
    native: tauri::State<'_, NativeSkillsCommandState>,
    app: tauri::State<'_, AppState>,
    request: GitSkillRequest,
    scope: SkillScope,
    approval_digest: String,
    approved: bool,
) -> Result<SkillMutationResult, String> {
    require_main_window(&window)?;
    let workspace = workspace_for_scope(&app, scope)?;
    let manager = native.manager.clone();
    notify_mutation(
        &window,
        run_blocking(move || {
            manager.install_git(
                &request,
                scope,
                workspace.as_deref(),
                &approval_digest,
                approved,
            )
        })
        .await,
    )
}

#[tauri::command]
pub async fn native_skills_install_git_bulk(
    window: tauri::Window,
    native: tauri::State<'_, NativeSkillsCommandState>,
    app: tauri::State<'_, AppState>,
    request: GitSkillRequest,
    scope: SkillScope,
    approvals: Vec<GitBulkApproval>,
    approved: bool,
) -> Result<Vec<SkillMutationResult>, String> {
    require_main_window(&window)?;
    let workspace = workspace_for_scope(&app, scope)?;
    let manager = native.manager.clone();
    notify_mutation(
        &window,
        run_blocking(move || {
            manager.install_git_bulk(&request, scope, workspace.as_deref(), &approvals, approved)
        })
        .await,
    )
}

#[tauri::command]
pub async fn native_skills_set_enabled(
    window: tauri::Window,
    native: tauri::State<'_, NativeSkillsCommandState>,
    app: tauri::State<'_, AppState>,
    scope: SkillScope,
    command: String,
    enabled: bool,
) -> Result<SkillMutationResult, String> {
    require_main_window(&window)?;
    let workspace = workspace_for_scope(&app, scope)?;
    let manager = native.manager.clone();
    notify_mutation(
        &window,
        run_blocking(move || manager.set_enabled(scope, workspace.as_deref(), &command, enabled))
            .await,
    )
}

/// Same-repo group version of `native_skills_set_enabled` — the Settings
/// panel groups skills installed from the same Git repository into one
/// card and drives this for its Enable-all/Disable-all toggle.
#[tauri::command]
pub async fn native_skills_set_enabled_many(
    window: tauri::Window,
    native: tauri::State<'_, NativeSkillsCommandState>,
    app: tauri::State<'_, AppState>,
    scope: SkillScope,
    commands: Vec<String>,
    enabled: bool,
) -> Result<Vec<SkillMutationResult>, String> {
    require_main_window(&window)?;
    let workspace = workspace_for_scope(&app, scope)?;
    let manager = native.manager.clone();
    notify_mutation(
        &window,
        run_blocking(move || {
            manager.set_enabled_many(scope, workspace.as_deref(), &commands, enabled)
        })
        .await,
    )
}

#[tauri::command]
pub async fn native_skills_uninstall(
    window: tauri::Window,
    native: tauri::State<'_, NativeSkillsCommandState>,
    app: tauri::State<'_, AppState>,
    scope: SkillScope,
    command: String,
) -> Result<SkillMutationResult, String> {
    require_main_window(&window)?;
    let workspace = workspace_for_scope(&app, scope)?;
    let manager = native.manager.clone();
    notify_mutation(
        &window,
        run_blocking(move || manager.uninstall(scope, workspace.as_deref(), &command)).await,
    )
}

/// Same-repo group version of `native_skills_uninstall` — see
/// `native_skills_set_enabled_many`.
#[tauri::command]
pub async fn native_skills_uninstall_many(
    window: tauri::Window,
    native: tauri::State<'_, NativeSkillsCommandState>,
    app: tauri::State<'_, AppState>,
    scope: SkillScope,
    commands: Vec<String>,
) -> Result<Vec<SkillMutationResult>, String> {
    require_main_window(&window)?;
    let workspace = workspace_for_scope(&app, scope)?;
    let manager = native.manager.clone();
    notify_mutation(
        &window,
        run_blocking(move || manager.uninstall_many(scope, workspace.as_deref(), &commands)).await,
    )
}

#[tauri::command]
pub async fn native_skills_rollback(
    window: tauri::Window,
    native: tauri::State<'_, NativeSkillsCommandState>,
    app: tauri::State<'_, AppState>,
    scope: SkillScope,
    command: String,
) -> Result<SkillMutationResult, String> {
    require_main_window(&window)?;
    let workspace = workspace_for_scope(&app, scope)?;
    let manager = native.manager.clone();
    notify_mutation(
        &window,
        run_blocking(move || manager.rollback(scope, workspace.as_deref(), &command)).await,
    )
}

/// Same-repo group version of `native_skills_rollback` — see
/// `native_skills_set_enabled_many`.
#[tauri::command]
pub async fn native_skills_rollback_many(
    window: tauri::Window,
    native: tauri::State<'_, NativeSkillsCommandState>,
    app: tauri::State<'_, AppState>,
    scope: SkillScope,
    commands: Vec<String>,
) -> Result<Vec<SkillMutationResult>, String> {
    require_main_window(&window)?;
    let workspace = workspace_for_scope(&app, scope)?;
    let manager = native.manager.clone();
    notify_mutation(
        &window,
        run_blocking(move || manager.rollback_many(scope, workspace.as_deref(), &commands)).await,
    )
}

/// `pub(crate)` (unlike the other private helpers in this module) so
/// `tools.rs`'s `tool_read_skill_resource` can resolve the same primary
/// workspace path this module's own commands do, instead of duplicating
/// the `AppState.workspace_roots` lock/canonicalize dance.
pub(crate) fn optional_primary_workspace(state: &AppState) -> Result<Option<PathBuf>, String> {
    let roots = state
        .workspace_roots
        .lock()
        .map_err(|_| "Workspace roots lock poisoned".to_string())?;
    let Some(primary) = roots.first() else {
        return Ok(None);
    };
    primary.path.canonicalize().map(Some).map_err(|error| {
        format!(
            "Primary workspace '{}' is no longer valid: {error}",
            primary.path.display()
        )
    })
}

fn require_main_window(window: &tauri::Window) -> Result<(), String> {
    if window.label() == "main" {
        Ok(())
    } else {
        Err("Native skill management is only available from the main window".to_string())
    }
}

fn workspace_for_scope(state: &AppState, scope: SkillScope) -> Result<Option<PathBuf>, String> {
    match scope {
        SkillScope::Global => Ok(None),
        SkillScope::Workspace => optional_primary_workspace(state)?.map(Some).ok_or_else(|| {
            "No workspace folder is open. Open a folder before managing workspace skills."
                .to_string()
        }),
    }
}

/// `pub(crate)` for the same reason as `optional_primary_workspace` above —
/// `tools.rs`'s `tool_read_skill_resource` reuses this rather than a second
/// `spawn_blocking`/`SkillError`-to-`String` wrapper.
pub(crate) async fn run_blocking<T, F>(operation: F) -> Result<T, String>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T, crate::native_skills::SkillError> + Send + 'static,
{
    tauri::async_runtime::spawn_blocking(operation)
        .await
        .map_err(|error| format!("Native skill worker failed: {error}"))?
        .map_err(command_error)
}

fn command_error(error: crate::native_skills::SkillError) -> String {
    error.to_string()
}
