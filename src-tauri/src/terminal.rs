//! Workspace-scoped interactive terminal sessions.
//!
//! Each tab owns a real OS pseudoterminal. A tab can only be created for an
//! exact, currently attached canonical workspace root; start/restart and
//! every submitted command reuse the existing `run_shell` permission gate.
//! PTY output is retained in a bounded in-memory tail and mirrored to the
//! frontend over events. Command history is persisted per canonical root.

use std::collections::HashMap;
use std::fs;
use std::fs::OpenOptions;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};
use serde::{Deserialize, Serialize};
use tauri::Emitter;

use crate::{app_paths, permissions, workspace, AppState};

pub const MAX_OUTPUT_BYTES: usize = 256 * 1024;
const MAX_EVENT_CHUNK_BYTES: usize = 32 * 1024;
const MAX_COMMAND_BYTES: usize = 16 * 1024;
const MAX_HISTORY_FILE_BYTES: u64 = 1024 * 1024;
const MAX_HISTORY_ENTRIES: usize = 200;
const HISTORY_FILE: &str = "terminal_history.json";
const EXIT_POLL_INTERVAL: Duration = Duration::from_millis(100);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum TerminalStatus {
    Running,
    Exited,
    Killed,
    Error,
}

#[derive(Clone, Debug, Serialize)]
pub struct TerminalSessionView {
    pub id: String,
    pub workspace_id: String,
    pub workspace_path: String,
    pub shell: String,
    pub status: TerminalStatus,
    pub exit_code: Option<u32>,
    pub output: String,
    pub output_truncated: bool,
    pub started_at_ms: u64,
}

#[derive(Clone, Serialize)]
struct TerminalOutputEvent {
    session_id: String,
    chunk: String,
    output_truncated: bool,
}

#[derive(Clone, Serialize)]
struct TerminalStatusEvent {
    session: TerminalSessionView,
}

struct TerminalProcess {
    view: Mutex<TerminalSessionView>,
    writer: Mutex<Box<dyn Write + Send>>,
    child: Mutex<Box<dyn Child + Send + Sync>>,
    master: Mutex<Box<dyn MasterPty + Send>>,
}

impl TerminalProcess {
    fn view(&self) -> Result<TerminalSessionView, String> {
        self.view
            .lock()
            .map(|view| view.clone())
            .map_err(|_| "Terminal session lock poisoned".to_string())
    }

    fn kill(&self) -> Result<TerminalSessionView, String> {
        {
            let mut child = lock(&self.child, "Terminal child")?;
            // A process that exited between the frontend click and this lock
            // is already in the requested terminal state. portable-pty can
            // report an error for killing that stale handle; ignore it.
            let _ = child.kill();
        }
        let mut view = lock(&self.view, "Terminal session")?;
        if view.status == TerminalStatus::Running {
            view.status = TerminalStatus::Killed;
        }
        Ok(view.clone())
    }
}

/// In-memory PTY ownership. It is part of `AppState`, so commands in separate
/// WebViews/windows still address the same sessions and a workspace change
/// can authoritatively terminate sessions for roots it detaches.
#[derive(Default)]
pub struct TerminalManager {
    sessions: Mutex<HashMap<String, Arc<TerminalProcess>>>,
    history_lock: Mutex<()>,
}

impl TerminalManager {
    fn get(&self, id: &str) -> Result<Arc<TerminalProcess>, String> {
        lock(&self.sessions, "Terminal sessions")?
            .get(id)
            .cloned()
            .ok_or_else(|| format!("No terminal session with id '{id}'"))
    }

    fn insert(&self, id: String, process: Arc<TerminalProcess>) -> Result<(), String> {
        lock(&self.sessions, "Terminal sessions")?.insert(id, process);
        Ok(())
    }

    fn remove(&self, id: &str) -> Result<Option<Arc<TerminalProcess>>, String> {
        Ok(lock(&self.sessions, "Terminal sessions")?.remove(id))
    }

    fn list(&self) -> Result<Vec<TerminalSessionView>, String> {
        let sessions: Vec<_> = lock(&self.sessions, "Terminal sessions")?
            .values()
            .cloned()
            .collect();
        let mut views = sessions
            .into_iter()
            .map(|session| session.view())
            .collect::<Result<Vec<_>, _>>()?;
        views.sort_by_key(|view| view.started_at_ms);
        Ok(views)
    }

    pub(crate) fn kill_all<R: tauri::Runtime>(&self, app: Option<&tauri::AppHandle<R>>) {
        let sessions = self
            .sessions
            .lock()
            .map(|sessions| sessions.values().cloned().collect::<Vec<_>>())
            .unwrap_or_default();
        for process in sessions {
            if let Ok(view) = process.kill() {
                emit_status(app, view);
            }
        }
    }

    pub(crate) fn kill_workspace<R: tauri::Runtime>(
        &self,
        workspace_id: &str,
        app: Option<&tauri::AppHandle<R>>,
    ) {
        let sessions = self
            .sessions
            .lock()
            .map(|sessions| {
                sessions
                    .values()
                    .filter_map(|process| {
                        process
                            .view
                            .lock()
                            .ok()
                            .filter(|view| view.workspace_id == workspace_id)
                            .map(|_| process.clone())
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        for process in sessions {
            if let Ok(view) = process.kill() {
                emit_status(app, view);
            }
        }
    }
}

impl Drop for TerminalManager {
    fn drop(&mut self) {
        // App shutdown must not orphan user shells. No event is needed because
        // every WebView is shutting down with the manager.
        self.kill_all::<tauri::Wry>(None);
    }
}

fn lock<'a, T>(mutex: &'a Mutex<T>, label: &str) -> Result<MutexGuard<'a, T>, String> {
    mutex.lock().map_err(|_| format!("{label} lock poisoned"))
}

fn now_ms() -> Result<u64, String> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| "System clock is before Unix epoch".to_string())?
        .as_millis()
        .try_into()
        .map_err(|_| "System time exceeds supported range".to_string())
}

/// Append a UTF-8 chunk while retaining only the newest `max_bytes` bytes.
/// The cut advances to a character boundary, so a capped tail is always valid
/// UTF-8 even when the oldest retained character is multi-byte.
fn append_bounded(output: &mut String, chunk: &str, max_bytes: usize) -> bool {
    output.push_str(chunk);
    if output.len() <= max_bytes {
        return false;
    }

    let mut cut = output.len().saturating_sub(max_bytes);
    while cut < output.len() && !output.is_char_boundary(cut) {
        cut += 1;
    }
    output.drain(..cut);
    true
}

fn exact_workspace_root(state: &AppState, workspace_id: &str) -> Result<PathBuf, String> {
    let (resolved, root) = workspace::resolve_path_and_root(state, workspace_id)?;
    let canonical_id = root.to_string_lossy();
    if resolved != root || workspace_id != canonical_id {
        return Err("Terminal workspace must be an exact attached canonical root".to_string());
    }
    Ok(root)
}

#[cfg(windows)]
fn user_shell() -> PathBuf {
    std::env::var_os("COMSPEC")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute() && path.is_file())
        .unwrap_or_else(|| PathBuf::from(r"C:\Windows\System32\cmd.exe"))
}

#[cfg(not(windows))]
fn user_shell() -> PathBuf {
    std::env::var_os("SHELL")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute() && path.is_file())
        .unwrap_or_else(|| PathBuf::from("/bin/sh"))
}

fn bounded_size(rows: Option<u16>, cols: Option<u16>) -> PtySize {
    PtySize {
        rows: rows.unwrap_or(24).clamp(2, 500),
        cols: cols.unwrap_or(100).clamp(20, 500),
        pixel_width: 0,
        pixel_height: 0,
    }
}

fn emit_status<R: tauri::Runtime>(
    app: Option<&tauri::AppHandle<R>>,
    session: TerminalSessionView,
) {
    if let Some(app) = app {
        let _ = app.emit("terminal://status", TerminalStatusEvent { session });
    }
}

fn spawn_reader<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
    process: Arc<TerminalProcess>,
    mut reader: Box<dyn Read + Send>,
) {
    std::thread::spawn(move || {
        let mut buffer = vec![0_u8; MAX_EVENT_CHUNK_BYTES];
        loop {
            match reader.read(&mut buffer) {
                Ok(0) => break,
                Ok(read) => {
                    let chunk = String::from_utf8_lossy(&buffer[..read]).into_owned();
                    let truncated = match process.view.lock() {
                        Ok(mut view) => {
                            let did_trim = append_bounded(&mut view.output, &chunk, MAX_OUTPUT_BYTES);
                            view.output_truncated |= did_trim;
                            view.output_truncated
                        }
                        Err(_) => break,
                    };
                    let session_id = process
                        .view
                        .lock()
                        .map(|view| view.id.clone())
                        .unwrap_or_default();
                    if session_id.is_empty() {
                        break;
                    }
                    let _ = app.emit(
                        "terminal://output",
                        TerminalOutputEvent {
                            session_id,
                            chunk,
                            output_truncated: truncated,
                        },
                    );
                }
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(error) => {
                    let next = process.view.lock().ok().map(|mut view| {
                        if view.status == TerminalStatus::Running {
                            view.status = TerminalStatus::Error;
                            append_bounded(
                                &mut view.output,
                                &format!("\r\n[terminal read failed: {error}]\r\n"),
                                MAX_OUTPUT_BYTES,
                            );
                        }
                        view.clone()
                    });
                    if let Some(view) = next {
                        emit_status(Some(&app), view);
                    }
                    break;
                }
            }
        }
    });
}

fn apply_exit(view: &mut TerminalSessionView, exit_code: u32) {
    // A user-requested kill is a distinct, useful state in the UI. The later
    // OS exit observation may fill its code, but must not rewrite it to the
    // less-informative generic `exited` state.
    if view.status == TerminalStatus::Running {
        view.status = TerminalStatus::Exited;
    }
    view.exit_code = Some(exit_code);
}

fn spawn_exit_watcher<R: tauri::Runtime>(
    app: tauri::AppHandle<R>,
    process: Arc<TerminalProcess>,
) {
    std::thread::spawn(move || loop {
        std::thread::sleep(EXIT_POLL_INTERVAL);
        let polled = process
            .child
            .lock()
            .map_err(|_| "Terminal child lock poisoned".to_string())
            .and_then(|mut child| {
                child
                    .try_wait()
                    .map_err(|error| format!("Failed to read terminal process status: {error}"))
            });

        match polled {
            Ok(None) => continue,
            Ok(Some(status)) => {
                if let Ok(mut view) = process.view.lock() {
                    apply_exit(&mut view, status.exit_code());
                    emit_status(Some(&app), view.clone());
                }
                break;
            }
            Err(error) => {
                if let Ok(mut view) = process.view.lock() {
                    if view.status == TerminalStatus::Running {
                        view.status = TerminalStatus::Error;
                        append_bounded(
                            &mut view.output,
                            &format!("\r\n[{error}]\r\n"),
                            MAX_OUTPUT_BYTES,
                        );
                    }
                    emit_status(Some(&app), view.clone());
                }
                break;
            }
        }
    });
}

fn spawn_session<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    state: &AppState,
    workspace_id: String,
    rows: Option<u16>,
    cols: Option<u16>,
) -> Result<TerminalSessionView, String> {
    let root = exact_workspace_root(state, &workspace_id)?;
    let shell = user_shell();
    if !shell.is_file() {
        return Err(format!("Configured shell '{}' does not exist", shell.display()));
    }

    let pair = native_pty_system()
        .openpty(bounded_size(rows, cols))
        .map_err(|error| format!("Failed to create terminal PTY: {error}"))?;
    let mut command = CommandBuilder::new(&shell);
    command.cwd(&root);
    command.env("TERM", "xterm-256color");
    command.env("COLORTERM", "truecolor");
    command.env("TERM_PROGRAM", "LittleMonkey");

    let child = pair
        .slave
        .spawn_command(command)
        .map_err(|error| format!("Failed to spawn shell '{}': {error}", shell.display()))?;
    drop(pair.slave);
    let reader = pair
        .master
        .try_clone_reader()
        .map_err(|error| format!("Failed to open terminal output: {error}"))?;
    let writer = pair
        .master
        .take_writer()
        .map_err(|error| format!("Failed to open terminal input: {error}"))?;

    let view = TerminalSessionView {
        id: uuid::Uuid::new_v4().to_string(),
        workspace_id,
        workspace_path: root.to_string_lossy().to_string(),
        shell: shell.to_string_lossy().to_string(),
        status: TerminalStatus::Running,
        exit_code: None,
        output: String::new(),
        output_truncated: false,
        started_at_ms: now_ms()?,
    };
    let process = Arc::new(TerminalProcess {
        view: Mutex::new(view.clone()),
        writer: Mutex::new(writer),
        child: Mutex::new(child),
        master: Mutex::new(pair.master),
    });
    state.terminal.insert(view.id.clone(), process.clone())?;
    spawn_reader(app.clone(), process.clone(), reader);
    spawn_exit_watcher(app.clone(), process);
    Ok(view)
}

fn high_shell_risk(reason: &str) -> Option<permissions::RiskAssessment> {
    permissions::compute_risk(
        None,
        Some("high".to_string()),
        Some(reason.to_string()),
    )
}

#[tauri::command]
pub async fn terminal_create(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    workspace_id: String,
    rows: Option<u16>,
    cols: Option<u16>,
) -> Result<TerminalSessionView, String> {
    let root = exact_workspace_root(state.inner(), &workspace_id)?;
    permissions::request_permission(
        &app,
        state.inner(),
        "run_shell",
        format!(
            "Open an interactive terminal in '{}'.\n\nThe shell runs as your OS user and can access resources outside this workspace. Each command submitted through Little Monkey is separately approval-gated.",
            root.display()
        ),
        None,
        None,
        high_shell_risk("Interactive shells can run arbitrary programs with the current user's access"),
        None,
    )
    .await?;
    spawn_session(&app, state.inner(), workspace_id, rows, cols)
}

#[tauri::command]
pub fn terminal_list(
    state: tauri::State<'_, AppState>,
) -> Result<Vec<TerminalSessionView>, String> {
    state.terminal.list()
}

#[tauri::command]
pub async fn terminal_execute(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    session_id: String,
    command: String,
) -> Result<(), String> {
    let command = command.trim_end_matches(['\r', '\n']).to_string();
    if command.trim().is_empty() {
        return Err("Command cannot be empty".to_string());
    }
    if command.len() > MAX_COMMAND_BYTES {
        return Err(format!(
            "Command exceeds the {MAX_COMMAND_BYTES}-byte terminal limit"
        ));
    }
    if command.chars().any(|character| character == '\r' || character == '\n') {
        return Err("Submit one terminal command line at a time".to_string());
    }

    let process = state.terminal.get(&session_id)?;
    let before = process.view()?;
    if before.status != TerminalStatus::Running {
        return Err("Terminal is not running; restart it before sending a command".to_string());
    }
    exact_workspace_root(state.inner(), &before.workspace_id)?;

    permissions::request_permission(
        &app,
        state.inner(),
        "run_shell",
        command.clone(),
        None,
        None,
        high_shell_risk("Interactive terminal command with the current user's access"),
        None,
    )
    .await?;

    // Revalidate after the asynchronous approval: the workspace may have
    // changed or the user may have killed the tab while the modal was open.
    let after = process.view()?;
    if after.status != TerminalStatus::Running {
        return Err("Terminal stopped before the command was approved".to_string());
    }
    exact_workspace_root(state.inner(), &after.workspace_id)?;

    {
        let mut writer = lock(&process.writer, "Terminal input")?;
        writer
            .write_all(command.as_bytes())
            .and_then(|_| writer.write_all(b"\r"))
            .and_then(|_| writer.flush())
            .map_err(|error| format!("Failed to write terminal input: {error}"))?;
    }
    let history_path = history_path()?;
    let _history_guard = lock(&state.terminal.history_lock, "Terminal history")?;
    append_history(&history_path, &after.workspace_id, command)?;
    Ok(())
}

#[tauri::command]
pub fn terminal_interrupt(
    state: tauri::State<'_, AppState>,
    session_id: String,
) -> Result<(), String> {
    let process = state.terminal.get(&session_id)?;
    if process.view()?.status != TerminalStatus::Running {
        return Ok(());
    }
    let mut writer = lock(&process.writer, "Terminal input")?;
    writer
        .write_all(&[3])
        .and_then(|_| writer.flush())
        .map_err(|error| format!("Failed to interrupt terminal: {error}"))
}

#[tauri::command]
pub fn terminal_resize(
    state: tauri::State<'_, AppState>,
    session_id: String,
    rows: u16,
    cols: u16,
) -> Result<(), String> {
    let process = state.terminal.get(&session_id)?;
    let result = lock(&process.master, "Terminal PTY")?
        .resize(bounded_size(Some(rows), Some(cols)))
        .map_err(|error| format!("Failed to resize terminal: {error}"));
    result
}

#[tauri::command]
pub fn terminal_kill(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    session_id: String,
) -> Result<TerminalSessionView, String> {
    let view = state.terminal.get(&session_id)?.kill()?;
    emit_status(Some(&app), view.clone());
    Ok(view)
}

#[tauri::command]
pub async fn terminal_restart(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    session_id: String,
    rows: Option<u16>,
    cols: Option<u16>,
) -> Result<TerminalSessionView, String> {
    let old = state.terminal.get(&session_id)?;
    let old_view = old.view()?;
    let root = exact_workspace_root(state.inner(), &old_view.workspace_id)?;
    permissions::request_permission(
        &app,
        state.inner(),
        "run_shell",
        format!("Restart the interactive terminal in '{}'", root.display()),
        None,
        None,
        high_shell_risk("Restarting an interactive shell runs its startup configuration"),
        None,
    )
    .await?;
    if let Some(process) = state.terminal.remove(&session_id)? {
        let view = process.kill()?;
        emit_status(Some(&app), view);
    }
    spawn_session(
        &app,
        state.inner(),
        old_view.workspace_id,
        rows,
        cols,
    )
}

#[tauri::command]
pub fn terminal_close(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    session_id: String,
) -> Result<(), String> {
    if let Some(process) = state.terminal.remove(&session_id)? {
        let view = process.kill()?;
        emit_status(Some(&app), view);
    }
    Ok(())
}

#[tauri::command]
pub fn terminal_history(
    state: tauri::State<'_, AppState>,
    workspace_id: String,
) -> Result<Vec<String>, String> {
    exact_workspace_root(state.inner(), &workspace_id)?;
    let path = history_path()?;
    let _guard = lock(&state.terminal.history_lock, "Terminal history")?;
    Ok(load_history(&path)?.remove(&workspace_id).unwrap_or_default())
}

fn history_path() -> Result<PathBuf, String> {
    app_paths::data_dir()
        .map(|path| path.join(HISTORY_FILE))
        .ok_or_else(|| "Could not resolve the Little Monkey data directory".to_string())
}

#[derive(Default, Deserialize, Serialize)]
struct TerminalHistoryFile {
    #[serde(default)]
    workspaces: HashMap<String, Vec<String>>,
}

fn load_history(path: &Path) -> Result<HashMap<String, Vec<String>>, String> {
    if !path.exists() {
        return Ok(HashMap::new());
    }
    let metadata = fs::metadata(path)
        .map_err(|error| format!("Failed to inspect terminal history: {error}"))?;
    if metadata.len() > MAX_HISTORY_FILE_BYTES {
        return Err("Terminal history file exceeds the safety limit".to_string());
    }
    let bytes = fs::read(path).map_err(|error| format!("Failed to read terminal history: {error}"))?;
    let parsed: TerminalHistoryFile = serde_json::from_slice(&bytes)
        .map_err(|error| format!("Failed to parse terminal history: {error}"))?;
    Ok(parsed.workspaces)
}

fn save_history(path: &Path, workspaces: HashMap<String, Vec<String>>) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| format!("Failed to create terminal history directory: {error}"))?;
    }
    let bytes = serde_json::to_vec_pretty(&TerminalHistoryFile { workspaces })
        .map_err(|error| format!("Failed to encode terminal history: {error}"))?;
    if bytes.len() as u64 > MAX_HISTORY_FILE_BYTES {
        return Err("Terminal history file exceeds the safety limit".to_string());
    }
    let temp = path.with_extension("json.tmp");
    let mut options = OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(&temp)
        .map_err(|error| format!("Failed to write terminal history: {error}"))?;
    file.write_all(&bytes)
        .and_then(|_| file.sync_all())
        .map_err(|error| format!("Failed to write terminal history: {error}"))?;
    if let Err(first) = fs::rename(&temp, path) {
        // Windows does not replace an existing destination with rename.
        if path.exists() {
            fs::remove_file(path)
                .map_err(|error| format!("Failed to replace terminal history: {error}"))?;
            fs::rename(&temp, path)
                .map_err(|error| format!("Failed to publish terminal history: {error}"))?;
        } else {
            return Err(format!("Failed to publish terminal history: {first}"));
        }
    }
    Ok(())
}

fn append_history(path: &Path, workspace_id: &str, command: String) -> Result<(), String> {
    if history_command_may_contain_secret(&command) {
        return Ok(());
    }
    let mut workspaces = load_history(path)?;
    let entries = workspaces.entry(workspace_id.to_string()).or_default();
    entries.push(command);
    if entries.len() > MAX_HISTORY_ENTRIES {
        entries.drain(..entries.len() - MAX_HISTORY_ENTRIES);
    }
    save_history(path, workspaces)
}

fn history_command_may_contain_secret(command: &str) -> bool {
    let lower = command.to_ascii_lowercase();
    [
        "password=",
        "passwd=",
        "token=",
        "api_key=",
        "apikey=",
        "secret=",
        "authorization:",
        "--password",
        "--token",
        "--with-token",
    ]
    .iter()
    .any(|marker| lower.contains(marker))
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TempTree {
        path: PathBuf,
    }

    impl TempTree {
        fn new() -> Self {
            static COUNTER: std::sync::atomic::AtomicU64 =
                std::sync::atomic::AtomicU64::new(0);
            let count = COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let path = std::env::temp_dir().join(format!(
                "little_monkey_terminal_test_{}_{}",
                std::process::id(),
                count
            ));
            fs::create_dir_all(&path).unwrap();
            Self { path }
        }
    }

    impl Drop for TempTree {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    fn state_with_root(path: &Path) -> AppState {
        let state = AppState::default();
        let canonical = path.canonicalize().unwrap();
        state
            .workspace_roots
            .lock()
            .unwrap()
            .push(workspace::WorkspaceRoot {
                id: canonical.to_string_lossy().to_string(),
                path: canonical,
                label: "terminal-test".to_string(),
            });
        state
    }

    #[test]
    fn terminal_workspace_requires_the_exact_attached_canonical_root() {
        let root = TempTree::new();
        fs::create_dir_all(root.path.join("nested")).unwrap();
        let outside = TempTree::new();
        let state = state_with_root(&root.path);
        let canonical = root.path.canonicalize().unwrap();

        assert_eq!(
            exact_workspace_root(&state, canonical.to_string_lossy().as_ref()).unwrap(),
            canonical
        );
        assert!(exact_workspace_root(
            &state,
            root.path.join("nested").to_string_lossy().as_ref()
        )
        .is_err());
        assert!(exact_workspace_root(&state, outside.path.to_string_lossy().as_ref()).is_err());
    }

    #[test]
    fn bounded_output_keeps_a_valid_utf8_tail_and_marks_truncation() {
        let mut output = "prefix".to_string();
        assert!(append_bounded(&mut output, "🙂suffix", 10));
        assert!(output.len() <= 10);
        assert!(output.ends_with("suffix"));
        assert!(std::str::from_utf8(output.as_bytes()).is_ok());
    }

    #[test]
    fn killed_state_is_not_overwritten_when_the_os_exit_arrives() {
        let mut view = TerminalSessionView {
            id: "terminal".to_string(),
            workspace_id: "/workspace".to_string(),
            workspace_path: "/workspace".to_string(),
            shell: "/bin/sh".to_string(),
            status: TerminalStatus::Killed,
            exit_code: None,
            output: String::new(),
            output_truncated: false,
            started_at_ms: 1,
        };
        apply_exit(&mut view, 137);
        assert_eq!(view.status, TerminalStatus::Killed);
        assert_eq!(view.exit_code, Some(137));
    }

    #[test]
    fn command_history_is_persisted_per_workspace_and_bounded() {
        let tree = TempTree::new();
        let path = tree.path.join("history.json");
        for index in 0..(MAX_HISTORY_ENTRIES + 5) {
            append_history(&path, "/a", format!("command-{index}")).unwrap();
        }
        append_history(&path, "/b", "other".to_string()).unwrap();

        let history = load_history(&path).unwrap();
        assert_eq!(history["/a"].len(), MAX_HISTORY_ENTRIES);
        assert_eq!(history["/a"].first().unwrap(), "command-5");
        assert_eq!(history["/b"], vec!["other"]);
    }

    #[test]
    fn likely_secret_commands_are_not_written_to_history() {
        let tree = TempTree::new();
        let path = tree.path.join("history.json");
        append_history(&path, "/a", "echo safe".to_string()).unwrap();
        append_history(&path, "/a", "export API_KEY=super-secret".to_string()).unwrap();
        append_history(&path, "/a", "curl -H 'Authorization: Bearer secret'".to_string()).unwrap();
        assert_eq!(load_history(&path).unwrap()["/a"], vec!["echo safe"]);
    }
}
