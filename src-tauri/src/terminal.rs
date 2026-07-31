//! Workspace-scoped interactive terminal sessions.
//!
//! Each tab owns a real OS pseudoterminal. A tab can only be created for an
//! exact, currently attached canonical workspace root. These commands are
//! user-initiated (typed/clicked in the terminal panel), so they carry no
//! `run_shell` permission gate — the user approving their own keystrokes
//! protects nothing; that gate exists for the *agent's* shell tool
//! (tools.rs), which remains fully gated. PTY output is retained in a
//! bounded in-memory tail and mirrored to the frontend over events. Command
//! history is persisted per canonical root.

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

use crate::{app_paths, workspace, AppState};

pub const MAX_OUTPUT_BYTES: usize = 256 * 1024;
const MAX_EVENT_CHUNK_BYTES: usize = 32 * 1024;
const MAX_COMMAND_BYTES: usize = 16 * 1024;
const MAX_HISTORY_FILE_BYTES: u64 = 1024 * 1024;
const MAX_HISTORY_ENTRIES: usize = 200;
const MAX_PROMPT_PROBE_BYTES: usize = 2 * 1024;
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

/// Local OS identity shown in the prompt line (`user@host`). Never carries
/// workspace or secret data — purely cosmetic, read once per app launch.
#[derive(Clone, Debug, Serialize)]
pub struct TerminalIdentity {
    pub user: String,
    pub host: String,
}

#[tauri::command]
pub fn terminal_identity() -> TerminalIdentity {
    let user = std::env::var("USER")
        .or_else(|_| std::env::var("USERNAME"))
        .unwrap_or_else(|_| "user".to_string());
    let host = std::env::var("COMPUTERNAME")
        .ok()
        .filter(|value| !value.is_empty())
        .or_else(|| {
            std::process::Command::new("hostname")
                .arg("-s")
                .output()
                .ok()
                .filter(|output| output.status.success())
                .and_then(|output| String::from_utf8(output.stdout).ok())
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty())
        })
        .unwrap_or_else(|| "localhost".to_string());
    TerminalIdentity { user, host }
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

/// Conservatively reconstructs commands typed through the raw PTY path.
///
/// This is intentionally not a generic keystroke logger. Input is eligible
/// only after output looks like a shell prompt, and only while the line can
/// be reconstructed exactly. Completion/history/cursor escape sequences,
/// multiline pastes, unknown control keys, and input entered while a command
/// or password prompt is active are dropped. The final persistence layer also
/// rejects secret-shaped command lines.
struct HistoryInputTracker {
    buffer: String,
    prompt_probe: String,
    ready_for_command: bool,
    reconstructable: bool,
}

impl Default for HistoryInputTracker {
    fn default() -> Self {
        Self {
            buffer: String::new(),
            prompt_probe: String::new(),
            ready_for_command: false,
            reconstructable: true,
        }
    }
}

impl HistoryInputTracker {
    fn observe_output(&mut self, chunk: &str) {
        append_bounded(&mut self.prompt_probe, chunk, MAX_PROMPT_PROBE_BYTES);
        if !self.ready_for_command && output_looks_like_shell_prompt(&self.prompt_probe) {
            self.ready_for_command = true;
            self.reconstructable = true;
            self.buffer.clear();
        }
    }

    fn observe_input(&mut self, data: &str) -> Vec<String> {
        let contains_line_break = data
            .chars()
            .any(|character| matches!(character, '\r' | '\n'));
        let contains_text = data.chars().any(|character| !character.is_control());
        if contains_line_break && contains_text {
            // A terminal paste can contain complete lines in one event. Do
            // not treat those bytes as individually typed shell commands.
            self.mark_submitted();
            return Vec::new();
        }

        let mut completed = Vec::new();
        for character in data.chars() {
            match character {
                '\r' | '\n' => {
                    if self.ready_for_command
                        && self.reconstructable
                        && !self.buffer.trim().is_empty()
                        && self.buffer.len() <= MAX_COMMAND_BYTES
                    {
                        completed.push(self.buffer.clone());
                    }
                    self.mark_submitted();
                }
                // Backspace/Delete. Unicode is removed by scalar value, so a
                // multi-byte character never leaves invalid UTF-8 behind.
                '\u{0008}' | '\u{007f}' if self.ready_for_command && self.reconstructable => {
                    self.buffer.pop();
                }
                // readline/line-editor "kill whole line".
                '\u{0015}' if self.ready_for_command => {
                    self.buffer.clear();
                    self.reconstructable = true;
                }
                // Common readline "delete previous word". Shell-specific
                // WORDCHARS settings can differ, so this only handles the
                // unambiguous whitespace-delimited case.
                '\u{0017}' if self.ready_for_command && self.reconstructable => {
                    while self.buffer.ends_with(char::is_whitespace) {
                        self.buffer.pop();
                    }
                    while self
                        .buffer
                        .chars()
                        .last()
                        .is_some_and(|value| !value.is_whitespace())
                    {
                        self.buffer.pop();
                    }
                }
                // Ctrl+L redraws the screen without changing the edit buffer.
                '\u{000c}' if self.ready_for_command => {}
                // Ctrl+C abandons the current input. The next shell prompt
                // must be observed before capture can resume.
                '\u{0003}' => self.mark_submitted(),
                character if !character.is_control() && self.ready_for_command => {
                    if self.reconstructable
                        && self.buffer.len() + character.len_utf8() <= MAX_COMMAND_BYTES
                    {
                        self.buffer.push(character);
                    } else {
                        self.buffer.clear();
                        self.reconstructable = false;
                    }
                }
                // Tabs, arrows/escape sequences, cursor movement, bracketed
                // paste markers, and unfamiliar control input mean the final
                // shell line cannot be known from bytes alone.
                _ if self.ready_for_command => {
                    self.buffer.clear();
                    self.reconstructable = false;
                }
                _ => {}
            }
        }
        completed
    }

    fn mark_submitted(&mut self) {
        self.buffer.clear();
        self.prompt_probe.clear();
        self.ready_for_command = false;
        self.reconstructable = true;
    }
}

struct TerminalProcess {
    view: Mutex<TerminalSessionView>,
    writer: Mutex<Box<dyn Write + Send>>,
    child: Mutex<Box<dyn Child + Send + Sync>>,
    master: Mutex<Box<dyn MasterPty + Send>>,
    shell_process_id: Option<u32>,
    history_input: Mutex<HistoryInputTracker>,
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

fn visible_terminal_text(output: &str) -> String {
    #[derive(Clone, Copy)]
    enum EscapeState {
        Text,
        Escape,
        Csi,
        Osc,
        OscEscape,
    }

    let mut state = EscapeState::Text;
    let mut visible = String::with_capacity(output.len());
    for character in output.chars() {
        state = match state {
            EscapeState::Text if character == '\u{001b}' => EscapeState::Escape,
            EscapeState::Text => {
                match character {
                    '\r' => visible.push('\n'),
                    '\n' | '\t' => visible.push(character),
                    value if !value.is_control() => visible.push(value),
                    _ => {}
                }
                EscapeState::Text
            }
            EscapeState::Escape if character == '[' => EscapeState::Csi,
            EscapeState::Escape if character == ']' => EscapeState::Osc,
            EscapeState::Escape => EscapeState::Text,
            EscapeState::Csi if ('@'..='~').contains(&character) => EscapeState::Text,
            EscapeState::Csi => EscapeState::Csi,
            EscapeState::Osc if character == '\u{0007}' => EscapeState::Text,
            EscapeState::Osc if character == '\u{001b}' => EscapeState::OscEscape,
            EscapeState::Osc => EscapeState::Osc,
            EscapeState::OscEscape if character == '\\' => EscapeState::Text,
            EscapeState::OscEscape => EscapeState::Osc,
        };
    }
    visible
}

fn output_looks_like_shell_prompt(output: &str) -> bool {
    let visible = visible_terminal_text(output);
    let line = visible
        .rsplit(['\r', '\n'])
        .next()
        .unwrap_or_default()
        .trim_end();
    if line.is_empty() {
        return false;
    }

    let lower = line.to_ascii_lowercase();
    if [
        "password",
        "passphrase",
        "verification code",
        "one-time code",
        "otp",
        "token:",
        "secret:",
        "api key",
    ]
    .iter()
    .any(|marker| lower.contains(marker))
    {
        return false;
    }

    if ['$', '%', '#', '❯', '➜', 'λ', '»', '›', '❱']
        .iter()
        .any(|suffix| line.ends_with(*suffix))
        || line.starts_with('➜')
    {
        return true;
    }

    if !line.ends_with('>') {
        return false;
    }

    // Avoid mistaking ordinary REPL prompts such as `>>>` or `node>` for a
    // shell. These patterns cover PowerShell, cmd.exe, and common fish/custom
    // prompts while deliberately missing ambiguous prompt themes.
    line.starts_with("PS ")
        || line.contains('@')
        || line.starts_with('~')
        || line.contains('/')
        || line
            .as_bytes()
            .get(1)
            .is_some_and(|character| *character == b':')
}

#[cfg(unix)]
fn shell_is_accepting_input(process: &TerminalProcess) -> bool {
    let Ok(master) = process.master.lock() else {
        return false;
    };
    let (Some(shell_pid), Some(foreground_pid), Some(fd)) = (
        process.shell_process_id,
        master.process_group_leader(),
        master.as_raw_fd(),
    ) else {
        return false;
    };
    if foreground_pid as u32 != shell_pid {
        return false;
    }

    let mut attributes = std::mem::MaybeUninit::<libc::termios>::uninit();
    // SAFETY: `fd` is the live PTY master owned by `master`, and tcgetattr
    // initializes the supplied termios value when it returns success.
    if unsafe { libc::tcgetattr(fd, attributes.as_mut_ptr()) } != 0 {
        return false;
    }
    let attributes = unsafe { attributes.assume_init() };
    attributes.c_lflag & libc::ECHO != 0 && attributes.c_lflag & libc::ICANON != 0
}

#[cfg(not(unix))]
fn shell_is_accepting_input(_process: &TerminalProcess) -> bool {
    // ConPTY does not expose a foreground process group or termios. The
    // prompt/reconstruction guards still apply on Windows; ambiguous prompts
    // are deliberately rejected by `output_looks_like_shell_prompt`.
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

fn emit_status<R: tauri::Runtime>(app: Option<&tauri::AppHandle<R>>, session: TerminalSessionView) {
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
                    if let Ok(mut history_input) = process.history_input.lock() {
                        history_input.observe_output(&chunk);
                    }
                    let truncated = match process.view.lock() {
                        Ok(mut view) => {
                            let did_trim =
                                append_bounded(&mut view.output, &chunk, MAX_OUTPUT_BYTES);
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

fn spawn_exit_watcher<R: tauri::Runtime>(app: tauri::AppHandle<R>, process: Arc<TerminalProcess>) {
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
        return Err(format!(
            "Configured shell '{}' does not exist",
            shell.display()
        ));
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
    let shell_process_id = child.process_id();
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
        shell_process_id,
        history_input: Mutex::new(HistoryInputTracker::default()),
    });
    state.terminal.insert(view.id.clone(), process.clone())?;
    spawn_reader(app.clone(), process.clone(), reader);
    spawn_exit_watcher(app.clone(), process);
    Ok(view)
}

#[tauri::command]
pub async fn terminal_create(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    workspace_id: String,
    rows: Option<u16>,
    cols: Option<u16>,
) -> Result<TerminalSessionView, String> {
    exact_workspace_root(state.inner(), &workspace_id)?;
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
    if command
        .chars()
        .any(|character| character == '\r' || character == '\n')
    {
        return Err("Submit one terminal command line at a time".to_string());
    }

    let process = state.terminal.get(&session_id)?;
    let before = process.view()?;
    if before.status != TerminalStatus::Running {
        return Err("Terminal is not running; restart it before sending a command".to_string());
    }
    exact_workspace_root(state.inner(), &before.workspace_id)?;
    let after = before;

    {
        // Hold the tracker across the PTY write so a very fast command cannot
        // emit its next prompt and re-arm history before this submission is
        // marked complete.
        let mut history_input = lock(&process.history_input, "Terminal history input")?;
        let mut writer = lock(&process.writer, "Terminal input")?;
        writer
            .write_all(command.as_bytes())
            .and_then(|_| writer.write_all(b"\r"))
            .and_then(|_| writer.flush())
            .map_err(|error| format!("Failed to write terminal input: {error}"))?;
        history_input.mark_submitted();
    }
    let history_path = history_path()?;
    let _history_guard = lock(&state.terminal.history_lock, "Terminal history")?;
    append_history(&history_path, &after.workspace_id, command)?;
    Ok(())
}

/// Upper bound for one raw input write — far above any human keystroke burst
/// or paste the UI should forward in a single IPC call, low enough to keep a
/// misbehaving caller from queueing unbounded PTY input.
const MAX_WRITE_BYTES: usize = 64 * 1024;

/// Raw keystroke path for the embedded terminal emulator: bytes go to the
/// PTY exactly as typed (arrows, tab, control characters, bracketed paste),
/// so the user's real shell provides its own line editing, history, and
/// completions. User-initiated like every command in this module — no
/// permission gate (see the module doc).
#[tauri::command]
pub fn terminal_write(
    state: tauri::State<'_, AppState>,
    session_id: String,
    data: String,
) -> Result<(), String> {
    if data.is_empty() {
        return Ok(());
    }
    if data.len() > MAX_WRITE_BYTES {
        return Err(format!(
            "Input exceeds the {MAX_WRITE_BYTES}-byte terminal write limit"
        ));
    }
    let process = state.terminal.get(&session_id)?;
    if process.view()?.status != TerminalStatus::Running {
        return Err("Terminal is not running; restart it before typing".to_string());
    }
    let submits_line = data
        .chars()
        .any(|character| matches!(character, '\r' | '\n'));
    let shell_accepts_submission = !submits_line || shell_is_accepting_input(&process);
    let completed = {
        // The writer and conservative input tracker share one critical
        // section so concurrent IPC writes are reconstructed in the same
        // order the PTY actually receives them.
        let mut history_input = lock(&process.history_input, "Terminal history input")?;
        let mut writer = lock(&process.writer, "Terminal input")?;
        writer
            .write_all(data.as_bytes())
            .and_then(|_| writer.flush())
            .map_err(|error| format!("Failed to write terminal input: {error}"))?;
        if shell_accepts_submission {
            history_input.observe_input(&data)
        } else {
            history_input.mark_submitted();
            Vec::new()
        }
    };

    if completed.is_empty() {
        return Ok(());
    }
    let workspace_id = process.view()?.workspace_id;
    let history_path = history_path()?;
    let _history_guard = lock(&state.terminal.history_lock, "Terminal history")?;
    for command in completed {
        append_history(&history_path, &workspace_id, command)?;
    }
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
    let mut history_input = lock(&process.history_input, "Terminal history input")?;
    let mut writer = lock(&process.writer, "Terminal input")?;
    writer
        .write_all(&[3])
        .and_then(|_| writer.flush())
        .map_err(|error| format!("Failed to interrupt terminal: {error}"))?;
    history_input.mark_submitted();
    Ok(())
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
    exact_workspace_root(state.inner(), &old_view.workspace_id)?;
    if let Some(process) = state.terminal.remove(&session_id)? {
        let view = process.kill()?;
        emit_status(Some(&app), view);
    }
    spawn_session(&app, state.inner(), old_view.workspace_id, rows, cols)
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
    Ok(load_history(&path)?
        .remove(&workspace_id)
        .unwrap_or_default())
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
    let bytes =
        fs::read(path).map_err(|error| format!("Failed to read terminal history: {error}"))?;
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
    // Leading-space commands are a long-standing opt-out convention used by
    // bash/zsh history settings. Respect it even if the user's shell is not
    // configured to do so, since this is a separate app-owned history.
    if command.starts_with(char::is_whitespace)
        || command
            .chars()
            .any(|character| matches!(character, '\r' | '\n') || character.is_control())
    {
        return true;
    }

    let lower = command.to_ascii_lowercase();
    [
        "password",
        "passwd",
        "passphrase",
        "token",
        "api_key",
        "apikey",
        "secret",
        "credential",
        "private_key",
        "access_key",
        "authorization:",
        "bearer ",
        "--with-token",
        "ghp_",
        "github_pat_",
        "sk-",
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
            static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
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
    fn terminal_identity_never_returns_empty_fields() {
        let identity = terminal_identity();
        assert!(!identity.user.is_empty());
        assert!(!identity.host.is_empty());
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
        assert!(
            exact_workspace_root(&state, root.path.join("nested").to_string_lossy().as_ref())
                .is_err()
        );
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
    fn raw_history_tracks_only_reconstructable_input_at_a_shell_prompt() {
        let mut tracker = HistoryInputTracker::default();
        tracker.observe_output("\u{001b}[32muser@host\u{001b}[0m:/workspace$ ");
        assert!(tracker.observe_input("echo hellp").is_empty());
        assert!(tracker.observe_input("\u{007f}o").is_empty());
        assert_eq!(tracker.observe_input("\r"), vec!["echo hello"]);

        // Output must reach another recognizable prompt before a new line is
        // eligible. Bytes typed while a command is active are ignored.
        assert!(tracker.observe_input("not-a-shell-command").is_empty());
        assert!(tracker.observe_input("\r").is_empty());
        tracker.observe_output("\r\nfinished\r\n/workspace$ ");
        assert!(tracker.observe_input("pwd").is_empty());
        assert_eq!(tracker.observe_input("\r"), vec!["pwd"]);

        tracker.observe_output("/workspace$ ");
        assert!(tracker.observe_input("echo $").is_empty());
        tracker.observe_output("echo $");
        assert!(tracker.observe_input("HOME").is_empty());
        assert_eq!(tracker.observe_input("\r"), vec!["echo $HOME"]);
    }

    #[test]
    fn raw_history_drops_shell_edits_pastes_and_prompted_secret_input() {
        let mut tracker = HistoryInputTracker::default();
        tracker.observe_output("/workspace$ ");
        assert!(tracker.observe_input("ec").is_empty());
        assert!(tracker.observe_input("\u{001b}[A").is_empty());
        assert!(tracker.observe_input("\r").is_empty());

        tracker.observe_output("/workspace$ ");
        assert!(tracker.observe_input("echo one\recho two\r").is_empty());

        tracker.observe_output("Password: ");
        assert!(tracker
            .observe_input("correct horse battery staple")
            .is_empty());
        assert!(tracker.observe_input("\r").is_empty());
    }

    #[test]
    fn shell_prompt_detection_rejects_password_and_repl_prompts() {
        assert!(output_looks_like_shell_prompt(
            "\u{001b}]0;workspace\u{0007}\u{001b}[36m~/code\u{001b}[0m ❯ "
        ));
        assert!(output_looks_like_shell_prompt("➜  newApp git:(main) ✗ "));
        assert!(!output_looks_like_shell_prompt("Password: "));
        assert!(!output_looks_like_shell_prompt(">>> "));
        assert!(!output_looks_like_shell_prompt("node> "));
        assert!(!output_looks_like_shell_prompt(
            "child output ending in $\r\n"
        ));
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
        append_history(
            &path,
            "/a",
            "curl -H 'Authorization: Bearer secret'".to_string(),
        )
        .unwrap();
        append_history(&path, "/a", " hidden-from-app-history".to_string()).unwrap();
        assert_eq!(load_history(&path).unwrap()["/a"], vec!["echo safe"]);
    }
}
