//! Safe Desktop Control — a design-validation research spike (ROADMAP.md
//! Phase 5, "Trust, Sandboxing, and PC Control" → "Safe Desktop Control",
//! Status: Research). Full threat model and non-goals:
//! `docs/safe-desktop-control-design.md`. Read that first.
//!
//! This is real, working, and gated, not a mock — but it is intentionally
//! narrow: nothing here is offered to the model as an agent tool (unlike
//! `tools.rs`'s `TOOLS`), it is reachable only from a human explicitly
//! opening the Settings panel, exactly like `m7_companion`'s capture grants.
//!
//! Shape, deliberately mirrored from two existing modules rather than
//! invented fresh:
//! - [`ControlSession`] mirrors `m7_companion::CaptureGrant` (id, scope,
//!   `created_at_ms`/`expires_at_ms`, `active`), but the scope is a
//!   non-empty *allowlist* of application/window identifiers rather than a
//!   single optional one, and every action must name which allowlisted
//!   target it's aimed at.
//! - The pending-action approve/deny flow mirrors `permissions.rs`'s
//!   `PendingPermission`/oneshot resume mechanism exactly (insert a
//!   `oneshot::Sender<bool>` keyed by a generated id, await it with a
//!   timeout, a separate command resolves it) — copied for the mechanism,
//!   not the struct itself, since `PendingPermission` is tool-call-shaped
//!   and carries run-ledger fields this spike has no use for.
//!
//! [`DesktopInputBackend`] is the one seam that keeps every session/gating/
//! approval/emergency-stop code path testable without ever touching a real
//! OS cursor: [`NullBackend`] (test double, always succeeds) and
//! [`UnsupportedBackend`] (production fallback on platforms/environments
//! without a wired input path, always a clear error) both implement it
//! alongside the real macOS `enigo`-backed implementation. No test in this
//! module exercises anything other than `NullBackend`.

use std::collections::{BTreeMap, HashMap};
use std::io::Write as _;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tauri::{Emitter, Manager};
use tokio::sync::oneshot;
use uuid::Uuid;

/// Longest a control session may run before it must be restarted explicitly
/// — mirrors `m7_companion::MAX_GRANT_LIFETIME_MS`'s "bounded, not
/// indefinite" posture for the same reason: an unattended session left open
/// for hours is its own risk even with every other gate in place.
pub const MAX_SESSION_LIFETIME_MS: u64 = 30 * 60 * 1_000;

/// A held cross-process desktop-control lock is considered stale — and may be
/// reclaimed — once it is older than this bound, even if its owner pid still
/// happens to be alive. A single control session can never legitimately
/// outlive [`MAX_SESSION_LIFETIME_MS`], so a lock older than that can only be
/// a leak from a crashed or wedged controller.
const STALE_LOCK_MS: u64 = MAX_SESSION_LIFETIME_MS;

/// Longest a single pending action waits for a human decision before it is
/// treated as denied — mirrors `permissions::PERMISSION_TIMEOUT`'s "silence
/// is a denial, never a hang" posture, just shorter: an on-screen desktop
/// action prompt is meant to be answered in seconds, not minutes.
const ACTION_APPROVAL_TIMEOUT: Duration = Duration::from_secs(2 * 60);

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MouseButtonKind {
    Left,
    Right,
    Middle,
}

/// A single input action a control session may request. Internally tagged
/// (`kind`) so the frontend's discriminated union matches this shape
/// exactly, and so a future variant can carry its own fields without a
/// serialization migration.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum ControlAction {
    MouseMove { x: i32, y: i32 },
    MouseClick { button: MouseButtonKind },
    KeyPress { key: String },
}

/// Seam between session/gating logic and the real OS. Every method takes
/// `&self` (not `&mut self`) so a single `Arc<dyn DesktopInputBackend>` can
/// be shared across concurrent action dispatches without an outer lock
/// forcing them to serialize purely for borrow-checking reasons — real
/// implementations that need exclusive access (e.g. `enigo::Enigo`, which
/// isn't `Sync`) hold their own internal `Mutex`.
pub trait DesktopInputBackend: Send + Sync {
    fn move_mouse(&self, x: i32, y: i32) -> Result<(), String>;
    fn click(&self, button: MouseButtonKind) -> Result<(), String>;
    fn key_press(&self, key: &str) -> Result<(), String>;
}

/// Test double: every action always succeeds and touches nothing on the
/// real OS. Used by every test in this module and nowhere in the production
/// backend selection below.
#[derive(Default)]
pub struct NullBackend;

impl DesktopInputBackend for NullBackend {
    fn move_mouse(&self, _x: i32, _y: i32) -> Result<(), String> {
        Ok(())
    }
    fn click(&self, _button: MouseButtonKind) -> Result<(), String> {
        Ok(())
    }
    fn key_press(&self, _key: &str) -> Result<(), String> {
        Ok(())
    }
}

/// Production fallback for platforms (or macOS environments where backend
/// construction itself failed, e.g. no Accessibility permission yet) with no
/// wired real input path. Every action fails clearly rather than silently
/// no-op-ing — a caller must never be able to mistake "nothing happened" for
/// "the action ran".
pub struct UnsupportedBackend(pub String);

impl DesktopInputBackend for UnsupportedBackend {
    fn move_mouse(&self, _x: i32, _y: i32) -> Result<(), String> {
        Err(self.0.clone())
    }
    fn click(&self, _button: MouseButtonKind) -> Result<(), String> {
        Err(self.0.clone())
    }
    fn key_press(&self, _key: &str) -> Result<(), String> {
        Err(self.0.clone())
    }
}

/// Real macOS input path. Not exercised by any test in this module (see the
/// module doc) — only ever constructed by [`production_backend`].
#[cfg(target_os = "macos")]
struct EnigoBackend(Mutex<enigo::Enigo>);

#[cfg(target_os = "macos")]
impl DesktopInputBackend for EnigoBackend {
    fn move_mouse(&self, x: i32, y: i32) -> Result<(), String> {
        use enigo::{Coordinate, Mouse};
        self.0
            .lock()
            .map_err(|_| "desktop input backend lock is poisoned".to_string())?
            .move_mouse(x, y, Coordinate::Abs)
            .map_err(|error| error.to_string())
    }

    fn click(&self, button: MouseButtonKind) -> Result<(), String> {
        use enigo::{Button, Direction, Mouse};
        let button = match button {
            MouseButtonKind::Left => Button::Left,
            MouseButtonKind::Right => Button::Right,
            MouseButtonKind::Middle => Button::Middle,
        };
        self.0
            .lock()
            .map_err(|_| "desktop input backend lock is poisoned".to_string())?
            .button(button, Direction::Click)
            .map_err(|error| error.to_string())
    }

    fn key_press(&self, key: &str) -> Result<(), String> {
        use enigo::{Direction, Keyboard};
        let parsed = parse_key(key)?;
        self.0
            .lock()
            .map_err(|_| "desktop input backend lock is poisoned".to_string())?
            .key(parsed, Direction::Click)
            .map_err(|error| error.to_string())
    }
}

/// A single Unicode character is sent as itself; a small set of named keys
/// covers the common non-printable ones. Anything else is rejected outright
/// — silently guessing at an unrecognized key name is exactly the kind of
/// "might do something other than what was approved" gap this spike avoids.
#[cfg(target_os = "macos")]
fn parse_key(key: &str) -> Result<enigo::Key, String> {
    use enigo::Key;
    let mut chars = key.chars();
    if let (Some(single), None) = (chars.next(), chars.next()) {
        return Ok(Key::Unicode(single));
    }
    Ok(match key.to_ascii_lowercase().as_str() {
        "enter" | "return" => Key::Return,
        "tab" => Key::Tab,
        "space" => Key::Space,
        "escape" | "esc" => Key::Escape,
        "backspace" => Key::Backspace,
        "delete" => Key::Delete,
        "up" => Key::UpArrow,
        "down" => Key::DownArrow,
        "left" => Key::LeftArrow,
        "right" => Key::RightArrow,
        _ => return Err(format!("Unsupported key name: {key}")),
    })
}

/// Selects the real backend on macOS, or a clear [`UnsupportedBackend`]
/// everywhere else (or if the real backend's own construction fails, e.g.
/// missing Accessibility permission) — never a silent no-op. Only ever
/// called once, from `DesktopControlState::production`; every test in this
/// module constructs its own [`NullBackend`] instead.
fn production_backend() -> Arc<dyn DesktopInputBackend> {
    #[cfg(target_os = "macos")]
    {
        match enigo::Enigo::new(&enigo::Settings::default()) {
            Ok(engine) => Arc::new(EnigoBackend(Mutex::new(engine))),
            Err(error) => Arc::new(UnsupportedBackend(format!(
                "Could not initialize macOS input simulation — grant Accessibility access in \
                 System Settings > Privacy & Security > Accessibility, then restart Little \
                 Monkey: {error}"
            ))),
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        Arc::new(UnsupportedBackend(
            "Safe Desktop Control input simulation is not implemented on this platform yet — \
             this research spike only wires a real backend on macOS"
                .to_string(),
        ))
    }
}

/// A single control session, scoped to an explicit, non-empty allowlist of
/// application/window identifiers — see the module doc's comparison to
/// `m7_companion::CaptureGrant`.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ControlSession {
    pub session_id: String,
    pub allowed_applications: Vec<String>,
    pub created_at_ms: u64,
    pub expires_at_ms: u64,
    pub active: bool,
    /// Always `true` while `active`: the visible on-screen indicator is not
    /// optional in this design (see the design doc's threat-model table).
    /// Kept as an explicit field rather than implied by `active` so the
    /// frontend has one clear source of truth for whether to render the
    /// "control is live" banner, and so a future mode that's active-but-
    /// hidden would be a visible, deliberate change to this struct rather
    /// than a silent behavior change.
    pub indicator_visible: bool,
    /// See the design doc's "What 'approved batch' mode is" section: skips
    /// the per-action approval prompt for this session only, never widens
    /// the allowlist, never disables emergency stop, never escapes the
    /// session's own expiry.
    pub approved_batch: bool,
}

/// An in-flight approval request for one [`ControlAction`], keyed by a
/// generated id in [`DesktopControlState::pending`]. Not `Clone`/`Serialize`
/// — the `oneshot::Sender` can't be, and nothing outside this module needs
/// the whole struct; [`PendingActionSummary`] is the serializable view sent
/// to the frontend.
struct PendingControlAction {
    session_id: String,
    sender: oneshot::Sender<bool>,
}

/// Serializable snapshot of a pending action, emitted to the frontend so it
/// can render an approve/deny prompt.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PendingActionSummary {
    pub action_id: String,
    pub session_id: String,
    pub action: ControlAction,
}

/// Result of a resolved (executed or denied) action, returned to the caller
/// of `desktop_control_request_action`.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ActionOutcome {
    pub action_id: String,
    pub executed: bool,
}

/// Outcome of [`DesktopControlState::begin_action`]'s validation step —
/// factored out from the async command so it's directly testable without an
/// `AppHandle`/oneshot-await machinery (mirrors `permissions.rs`'s
/// `mode_short_circuit` being a pure, directly-testable decision function).
pub enum ActionGate {
    /// The session is in "approved batch" mode: the action already ran (or
    /// failed) against the backend, no approval needed.
    Executed(Result<(), String>),
    /// The session requires per-action approval: the caller must await
    /// `receiver`, then dispatch to the backend itself on `Ok(Ok(true))`.
    Pending {
        action_id: String,
        receiver: oneshot::Receiver<bool>,
    },
}

fn lock<'a, T>(mutex: &'a Mutex<T>, label: &str) -> Result<MutexGuard<'a, T>, String> {
    mutex
        .lock()
        .map_err(|_| format!("{label} lock is poisoned"))
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| u64::try_from(duration.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

fn validate_application_id(value: &str) -> Result<(), String> {
    if value.is_empty() || value.len() > 256 || value.chars().any(char::is_control) {
        Err(
            "Application/window identifier must be a non-empty, printable, bounded string"
                .to_string(),
        )
    } else {
        Ok(())
    }
}

/// Contents of the machine-wide desktop-control lock file. Persisted as JSON
/// so a stale lock left by a crashed process can be inspected and reclaimed by
/// a later controller (see [`STALE_LOCK_MS`] and `process_alive`).
#[derive(Serialize, Deserialize)]
struct LockContents {
    pid: u32,
    acquired_at_ms: u64,
}

/// RAII owner of the on-disk desktop-control lock file. Removing the file on
/// drop is the process-exit backstop the design requires: even if a controller
/// panics between `start_session_impl` and an explicit stop, dropping the
/// [`DesktopControlState`] releases the lock so the next process is not blocked
/// by a phantom owner.
struct DesktopControlLockGuard {
    path: PathBuf,
}

impl Drop for DesktopControlLockGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Best-effort liveness probe mirroring `daemon::service::process_alive` — a
/// lock whose owner pid is gone is always safe to reclaim regardless of age.
fn process_alive(pid: u32) -> bool {
    #[cfg(unix)]
    {
        std::process::Command::new("kill")
            .args(["-0", &pid.to_string()])
            .status()
            .map(|status| status.success())
            .unwrap_or(false)
    }
    #[cfg(windows)]
    {
        std::process::Command::new("tasklist")
            .args(["/FI", &format!("PID eq {pid}"), "/NH"])
            .output()
            .map(|output| {
                output.status.success()
                    && String::from_utf8_lossy(&output.stdout).contains(&pid.to_string())
            })
            .unwrap_or(false)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = pid;
        true
    }
}

pub struct DesktopControlState {
    backend: Arc<dyn DesktopInputBackend>,
    sessions: Mutex<BTreeMap<String, ControlSession>>,
    pending: Mutex<HashMap<String, PendingControlAction>>,
    /// Path of the machine-wide exclusive lock this state must hold while any
    /// session is active, or `None` to disable cross-process locking (the
    /// shape every in-module test and any pure in-process caller uses).
    lock_path: Option<PathBuf>,
    /// The currently-held lock guard, if this state owns an active session.
    held_lock: Mutex<Option<DesktopControlLockGuard>>,
}

impl DesktopControlState {
    pub fn production() -> Self {
        Self::with_backend_and_lock(production_backend(), None)
    }

    /// Production backend plus the machine-wide exclusive lock at
    /// `<app_data>/desktop_control.lock`, so the local Tauri app and the
    /// resident daemon can never drive real OS input at the same time even
    /// though each constructs its own `DesktopControlState`.
    pub fn production_with_lock(lock_path: PathBuf) -> Self {
        Self::with_backend_and_lock(production_backend(), Some(lock_path))
    }

    pub fn with_backend(backend: Arc<dyn DesktopInputBackend>) -> Self {
        Self::with_backend_and_lock(backend, None)
    }

    pub fn with_backend_and_lock(
        backend: Arc<dyn DesktopInputBackend>,
        lock_path: Option<PathBuf>,
    ) -> Self {
        Self {
            backend,
            sessions: Mutex::new(BTreeMap::new()),
            pending: Mutex::new(HashMap::new()),
            lock_path,
            held_lock: Mutex::new(None),
        }
    }

    /// Acquire the machine-wide lock before a session may be created. A no-op
    /// when cross-process locking is disabled (`lock_path` is `None`) or when
    /// this state already owns the lock (a second concurrent session in the
    /// same process is fine — the invariant is one *controller process* at a
    /// time). A lock held by a live, recent process refuses the start.
    fn acquire_lock(&self) -> Result<(), String> {
        let Some(path) = self.lock_path.as_ref() else {
            return Ok(());
        };
        let mut held = lock(&self.held_lock, "desktop control lock")?;
        if held.is_some() {
            return Ok(());
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|error| {
                format!("Could not prepare desktop-control lock directory: {error}")
            })?;
        }
        // One reclaim attempt: if the existing lock is stale (dead owner or
        // older than STALE_LOCK_MS) remove it and retry the create-new.
        for attempt in 0..2 {
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(path)
            {
                Ok(mut file) => {
                    let contents = LockContents {
                        pid: std::process::id(),
                        acquired_at_ms: now_ms(),
                    };
                    let bytes = serde_json::to_vec(&contents).map_err(|error| {
                        format!("Could not encode desktop-control lock: {error}")
                    })?;
                    file.write_all(&bytes).map_err(|error| {
                        format!("Could not write desktop-control lock: {error}")
                    })?;
                    file.sync_all().map_err(|error| {
                        format!("Could not persist desktop-control lock: {error}")
                    })?;
                    *held = Some(DesktopControlLockGuard { path: path.clone() });
                    return Ok(());
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    if attempt == 0 && self.reclaim_if_stale(path) {
                        continue;
                    }
                    return Err(
                        "Another control session is already active on this machine — stop it \
                         (or wait for it to expire) before starting a new one"
                            .to_string(),
                    );
                }
                Err(error) => {
                    return Err(format!("Could not create desktop-control lock: {error}"));
                }
            }
        }
        Err(
            "Another control session is already active on this machine — stop it (or wait for \
             it to expire) before starting a new one"
                .to_string(),
        )
    }

    /// Returns `true` (having removed the file) when the on-disk lock is stale
    /// — unreadable, owned by a dead pid, or older than [`STALE_LOCK_MS`].
    fn reclaim_if_stale(&self, path: &std::path::Path) -> bool {
        let stale = match std::fs::read(path) {
            Ok(bytes) => match serde_json::from_slice::<LockContents>(&bytes) {
                Ok(contents) => {
                    now_ms().saturating_sub(contents.acquired_at_ms) > STALE_LOCK_MS
                        || !process_alive(contents.pid)
                }
                // A corrupt/partial lock file cannot represent a live owner.
                Err(_) => true,
            },
            Err(_) => true,
        };
        if stale {
            let _ = std::fs::remove_file(path);
        }
        stale
    }

    /// Drop the held lock once no session in this state is active any more.
    fn release_lock_if_idle(&self) -> Result<(), String> {
        if self.lock_path.is_none() {
            return Ok(());
        }
        let any_active = lock(&self.sessions, "control sessions")?
            .values()
            .any(|session| session.active);
        if !any_active {
            *lock(&self.held_lock, "desktop control lock")? = None;
        }
        Ok(())
    }

    /// Core session-start logic, deliberately taking the caller's current
    /// permission mode as a plain `&str` rather than reaching for
    /// `tauri::State<AppState>` itself — this is the hard invariant
    /// ("never reachable from bypass, no exceptions") and it must be
    /// directly testable with a bare string, not only through a full Tauri
    /// command. The `#[tauri::command]` wrapper below is what actually
    /// resolves the live mode via `permissions::get_permission_mode`.
    pub fn start_session_impl(
        &self,
        permission_mode: &str,
        allowed_applications: Vec<String>,
        lifetime_ms: u64,
        approved_batch: bool,
    ) -> Result<ControlSession, String> {
        if permission_mode == "bypass" {
            return Err(
                "Safe Desktop Control can never be started while permission mode is bypass — \
                 switch to a gated mode (manual, acceptEdits, plan, auto, or smart) first"
                    .to_string(),
            );
        }
        if allowed_applications.is_empty() {
            return Err(
                "Safe Desktop Control requires at least one allowed application/window — an \
                 empty allowlist would mean the session could act anywhere"
                    .to_string(),
            );
        }
        if allowed_applications.len() > 64 {
            return Err("Safe Desktop Control allowlist is limited to 64 entries".to_string());
        }
        for application_id in &allowed_applications {
            validate_application_id(application_id)?;
        }
        if lifetime_ms == 0 || lifetime_ms > MAX_SESSION_LIFETIME_MS {
            return Err(format!(
                "Session lifetime must be between 1 ms and {MAX_SESSION_LIFETIME_MS} ms"
            ));
        }
        // Acquire the machine-wide exclusive lock before a session exists, so
        // a refused start never leaves a half-created session behind.
        self.acquire_lock()?;
        let created_at_ms = now_ms();
        let session = ControlSession {
            session_id: format!("desktop-control-{}", Uuid::new_v4()),
            allowed_applications,
            created_at_ms,
            expires_at_ms: created_at_ms.saturating_add(lifetime_ms),
            active: true,
            indicator_visible: true,
            approved_batch,
        };
        lock(&self.sessions, "control sessions")?
            .insert(session.session_id.clone(), session.clone());
        Ok(session)
    }

    /// Deactivates one session and denies any of its still-pending actions.
    /// Returns whether the session was active before this call (so a caller
    /// can tell "stopped something" from "was already stopped/unknown").
    pub fn stop_session(&self, session_id: &str) -> Result<bool, String> {
        let was_active = lock(&self.sessions, "control sessions")?
            .get_mut(session_id)
            .map(|session| {
                let was_active = session.active;
                session.active = false;
                was_active
            })
            .unwrap_or(false);
        self.deny_pending_for_session(session_id)?;
        self.release_lock_if_idle()?;
        Ok(was_active)
    }

    /// Read-only snapshot for the Settings panel, with lazily-expired
    /// sessions reflected in the returned copy — mirrors
    /// `m7_companion::M7CompanionState::security_grants`'s "reflect
    /// expiration without mutating on a read" behavior.
    pub fn sessions_snapshot(&self) -> Result<Vec<ControlSession>, String> {
        let now = now_ms();
        Ok(lock(&self.sessions, "control sessions")?
            .values()
            .cloned()
            .map(|mut session| {
                if session.expires_at_ms <= now {
                    session.active = false;
                }
                session
            })
            .collect())
    }

    /// Returns whether any session is still active — used to decide whether
    /// the visible indicator should keep showing.
    pub fn any_session_active(&self) -> Result<bool, String> {
        Ok(self
            .sessions_snapshot()?
            .iter()
            .any(|session| session.active))
    }

    fn require_active_session(
        &self,
        session_id: &str,
        target_application_id: &str,
    ) -> Result<ControlSession, String> {
        validate_application_id(target_application_id)?;
        let now = now_ms();
        let mut sessions = lock(&self.sessions, "control sessions")?;
        let session = sessions
            .get_mut(session_id)
            .ok_or_else(|| "Control session is missing or was stopped".to_string())?;
        if session.expires_at_ms <= now {
            session.active = false;
        }
        if !session.active {
            return Err("Control session is inactive or expired".to_string());
        }
        if !session
            .allowed_applications
            .iter()
            .any(|allowed| allowed == target_application_id)
        {
            return Err(
                "Target application/window is outside this session's allowlist".to_string(),
            );
        }
        Ok(session.clone())
    }

    fn execute(&self, action: &ControlAction) -> Result<(), String> {
        match action {
            ControlAction::MouseMove { x, y } => self.backend.move_mouse(*x, *y),
            ControlAction::MouseClick { button } => self.backend.click(*button),
            ControlAction::KeyPress { key } => self.backend.key_press(key),
        }
    }

    /// Validates the session/allowlist, then either executes immediately
    /// (approved-batch session) or registers a pending approval and returns
    /// the receiver half of its oneshot channel for the caller to await.
    /// Pure with respect to `AppHandle`/async runtime — the
    /// `#[tauri::command]` wrapper owns emitting the frontend event and
    /// awaiting with a timeout.
    pub fn begin_action(
        &self,
        session_id: &str,
        target_application_id: &str,
        action: ControlAction,
    ) -> Result<ActionGate, String> {
        let session = self.require_active_session(session_id, target_application_id)?;
        if session.approved_batch {
            return Ok(ActionGate::Executed(self.execute(&action)));
        }
        let (sender, receiver) = oneshot::channel::<bool>();
        let action_id = format!("control-action-{}", Uuid::new_v4());
        lock(&self.pending, "pending control actions")?.insert(
            action_id.clone(),
            PendingControlAction {
                session_id: session_id.to_string(),
                sender,
            },
        );
        Ok(ActionGate::Pending {
            action_id,
            receiver,
        })
    }

    /// Resolves a pending action by id, sending the decision through its
    /// oneshot channel. Returns `Ok(true)` if a pending action with that id
    /// existed, `Ok(false)` otherwise — mirrors `permissions.rs`'s
    /// `respond_if_pending` split between this pure lookup and the
    /// `#[tauri::command]` wrapper that turns "not found" into an `Err`.
    pub fn resolve_if_pending(&self, action_id: &str, approve: bool) -> Result<bool, String> {
        let Some(pending) = lock(&self.pending, "pending control actions")?.remove(action_id)
        else {
            return Ok(false);
        };
        // If the receiving end was already dropped (e.g. the request timed
        // out just before this call), there's nothing left to notify.
        let _ = pending.sender.send(approve);
        Ok(true)
    }

    /// Complete a pending per-action approval that a non-batch session
    /// produced via [`ActionGate::Pending`]. Resolves the pending entry (so
    /// its oneshot is consumed exactly once) and, only if it still existed and
    /// the decision was to allow, dispatches the action to the backend. Used
    /// by headless callers (the resident daemon's remote desktop-control
    /// routes) that decide the approval inline with a local prompt rather than
    /// through the async `#[tauri::command]` await/resolve split — and it keeps
    /// `execute` module-private. Returns whether the action actually ran.
    ///
    /// A `false` result with `approve == true` means the session was stopped
    /// (or its approval timed out) between `begin_action` and this call — the
    /// action is intentionally *not* executed in that race.
    pub fn finish_pending(
        &self,
        action_id: &str,
        action: &ControlAction,
        approve: bool,
    ) -> Result<bool, String> {
        if !self.resolve_if_pending(action_id, approve)? {
            return Ok(false);
        }
        if approve {
            self.execute(action)?;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    fn deny_pending_for_session(&self, session_id: &str) -> Result<(), String> {
        let mut pending = lock(&self.pending, "pending control actions")?;
        let matching: Vec<String> = pending
            .iter()
            .filter(|(_, action)| action.session_id == session_id)
            .map(|(id, _)| id.clone())
            .collect();
        for id in matching {
            if let Some(action) = pending.remove(&id) {
                let _ = action.sender.send(false);
            }
        }
        Ok(())
    }

    /// Removes one pending action without resolving it — used when the
    /// approval wait itself times out (see `desktop_control_request_action`),
    /// where the oneshot receiver has already observed the timeout and there
    /// is nothing left to notify.
    fn remove_pending(&self, action_id: &str) {
        if let Ok(mut pending) = self.pending.lock() {
            pending.remove(action_id);
        }
    }

    /// Deactivates every session and denies every pending action. Idempotent
    /// — calling this when nothing is active returns `(0, 0)` and is not an
    /// error, mirroring `m7_companion::M7CompanionState::emergency_stop`'s
    /// same guarantee (both are wired into the same app-exit shutdown path in
    /// `lib.rs`, so "already stopped" must never be treated as a failure).
    pub fn emergency_stop(&self) -> Result<(usize, usize), String> {
        let sessions_deactivated = {
            let mut sessions = lock(&self.sessions, "control sessions")?;
            let count = sessions.values().filter(|session| session.active).count();
            for session in sessions.values_mut() {
                session.active = false;
            }
            count
        };
        let actions_cancelled = {
            let mut pending = lock(&self.pending, "pending control actions")?;
            let count = pending.len();
            for (_, action) in pending.drain() {
                let _ = action.sender.send(false);
            }
            count
        };
        // Every session is now inactive, so the machine-wide lock is released
        // unconditionally — this is the app-exit / kill-switch / revoke path.
        self.release_lock_if_idle()?;
        Ok((sessions_deactivated, actions_cancelled))
    }
}

fn ensure_main_window(window: &tauri::Window) -> Result<(), String> {
    if window.label() == "main" {
        Ok(())
    } else {
        Err("Safe Desktop Control can only be driven from the main window".to_string())
    }
}

#[tauri::command]
pub fn desktop_control_start_session(
    app: tauri::AppHandle,
    window: tauri::Window,
    permissions_state: tauri::State<'_, crate::AppState>,
    state: tauri::State<'_, DesktopControlState>,
    allowed_applications: Vec<String>,
    lifetime_ms: u64,
    approved_batch: bool,
) -> Result<ControlSession, String> {
    ensure_main_window(&window)?;
    let mode = crate::permissions::get_permission_mode(permissions_state)?;
    let session =
        state.start_session_impl(&mode, allowed_applications, lifetime_ms, approved_batch)?;
    // Best-effort visible indicator — reuses the existing always-on-top
    // companion overlay window rather than building new window chrome (see
    // the design doc). A failure to show it never fails session start
    // itself: the session is still gated and stoppable either way, and the
    // Settings panel's own session list is a second, always-available
    // indicator.
    let _ = crate::m7_companion::show_overlay(&app);
    Ok(session)
}

#[tauri::command]
pub fn desktop_control_stop_session(
    app: tauri::AppHandle,
    window: tauri::Window,
    state: tauri::State<'_, DesktopControlState>,
    session_id: String,
) -> Result<bool, String> {
    ensure_main_window(&window)?;
    let stopped = state.stop_session(&session_id)?;
    if !state.any_session_active()? {
        if let Some(overlay) = app.get_webview_window("companion-overlay") {
            let _ = overlay.hide();
        }
    }
    Ok(stopped)
}

#[tauri::command]
pub fn desktop_control_sessions(
    state: tauri::State<'_, DesktopControlState>,
) -> Result<Vec<ControlSession>, String> {
    state.sessions_snapshot()
}

#[tauri::command]
pub async fn desktop_control_request_action(
    app: tauri::AppHandle,
    window: tauri::Window,
    state: tauri::State<'_, DesktopControlState>,
    session_id: String,
    target_application_id: String,
    action: ControlAction,
) -> Result<ActionOutcome, String> {
    ensure_main_window(&window)?;
    match state.begin_action(&session_id, &target_application_id, action.clone())? {
        ActionGate::Executed(result) => {
            result?;
            Ok(ActionOutcome {
                action_id: format!("batch-{}", Uuid::new_v4()),
                executed: true,
            })
        }
        ActionGate::Pending {
            action_id,
            receiver,
        } => {
            let _ = app.emit(
                "desktop-control://action-pending",
                PendingActionSummary {
                    action_id: action_id.clone(),
                    session_id,
                    action: action.clone(),
                },
            );
            match tokio::time::timeout(ACTION_APPROVAL_TIMEOUT, receiver).await {
                Ok(Ok(true)) => {
                    state.execute(&action)?;
                    Ok(ActionOutcome {
                        action_id,
                        executed: true,
                    })
                }
                Ok(Ok(false)) => Err("Control action was denied".to_string()),
                // Timed out, or the sender was dropped without a response.
                Ok(Err(_)) | Err(_) => {
                    state.remove_pending(&action_id);
                    Err("Control action approval timed out".to_string())
                }
            }
        }
    }
}

#[tauri::command]
pub fn desktop_control_respond_action(
    window: tauri::Window,
    state: tauri::State<'_, DesktopControlState>,
    action_id: String,
    approve: bool,
) -> Result<(), String> {
    ensure_main_window(&window)?;
    if state.resolve_if_pending(&action_id, approve)? {
        Ok(())
    } else {
        Err(format!("No pending control action with id {action_id}"))
    }
}

#[tauri::command]
pub fn desktop_control_emergency_stop(
    app: tauri::AppHandle,
    state: tauri::State<'_, DesktopControlState>,
) -> Result<serde_json::Value, String> {
    let (sessions_deactivated, actions_cancelled) = state.emergency_stop()?;
    if let Some(overlay) = app.get_webview_window("companion-overlay") {
        let _ = overlay.hide();
    }
    let payload = serde_json::json!({
        "sessionsDeactivated": sessions_deactivated,
        "actionsCancelled": actions_cancelled,
    });
    let _ = app.emit("desktop-control://emergency-stop", payload.clone());
    Ok(payload)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state() -> DesktopControlState {
        DesktopControlState::with_backend(Arc::new(NullBackend))
    }

    fn allow(apps: &[&str]) -> Vec<String> {
        apps.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn bypass_mode_is_always_refused() {
        let state = state();
        let err = state
            .start_session_impl("bypass", allow(&["Notes"]), 60_000, false)
            .unwrap_err();
        assert!(err.contains("bypass"));
        assert!(state.sessions_snapshot().unwrap().is_empty());
    }

    #[test]
    fn every_gated_mode_can_start_a_session() {
        let state = state();
        for mode in ["manual", "acceptEdits", "plan", "auto", "smart"] {
            let session = state
                .start_session_impl(mode, allow(&["Notes"]), 60_000, false)
                .unwrap_or_else(|error| panic!("mode {mode} should be allowed to start: {error}"));
            assert!(session.active);
            assert!(session.indicator_visible);
        }
    }

    #[test]
    fn empty_allowlist_is_refused() {
        let state = state();
        let err = state
            .start_session_impl("manual", Vec::new(), 60_000, false)
            .unwrap_err();
        assert!(err.contains("allowed application"));
    }

    #[test]
    fn lifetime_bounds_are_enforced() {
        let state = state();
        assert!(state
            .start_session_impl("manual", allow(&["Notes"]), 0, false)
            .is_err());
        assert!(state
            .start_session_impl(
                "manual",
                allow(&["Notes"]),
                MAX_SESSION_LIFETIME_MS + 1,
                false
            )
            .is_err());
        assert!(state
            .start_session_impl("manual", allow(&["Notes"]), MAX_SESSION_LIFETIME_MS, false)
            .is_ok());
    }

    #[test]
    fn allowlist_is_enforced_per_action() {
        let state = state();
        let session = state
            .start_session_impl("manual", allow(&["Notes"]), 60_000, false)
            .unwrap();

        let outside = state.begin_action(
            &session.session_id,
            "Safari",
            ControlAction::MouseMove { x: 1, y: 1 },
        );
        match outside {
            Err(error) => assert!(error.contains("allowlist")),
            Ok(_) => panic!("action against a non-allowlisted target must be rejected"),
        }

        let inside = state.begin_action(
            &session.session_id,
            "Notes",
            ControlAction::MouseMove { x: 1, y: 1 },
        );
        assert!(matches!(inside, Ok(ActionGate::Pending { .. })));
    }

    #[test]
    fn unknown_or_stopped_session_rejects_actions() {
        let state = state();
        assert!(state
            .begin_action(
                "does-not-exist",
                "Notes",
                ControlAction::KeyPress {
                    key: "a".to_string()
                }
            )
            .is_err());

        let session = state
            .start_session_impl("manual", allow(&["Notes"]), 60_000, false)
            .unwrap();
        assert!(state.stop_session(&session.session_id).unwrap());
        assert!(state
            .begin_action(
                &session.session_id,
                "Notes",
                ControlAction::KeyPress {
                    key: "a".to_string()
                }
            )
            .is_err());
    }

    #[test]
    fn approved_batch_session_executes_immediately_without_a_pending_entry() {
        let state = state();
        let session = state
            .start_session_impl("manual", allow(&["Notes"]), 60_000, true)
            .unwrap();

        let gate = state
            .begin_action(
                &session.session_id,
                "Notes",
                ControlAction::MouseClick {
                    button: MouseButtonKind::Left,
                },
            )
            .unwrap();
        match gate {
            ActionGate::Executed(result) => assert!(result.is_ok()),
            ActionGate::Pending { .. } => {
                panic!("approved-batch session must not create a pending approval")
            }
        }
    }

    #[test]
    fn pending_action_approval_resumes_via_the_oneshot_channel() {
        let state = state();
        let session = state
            .start_session_impl("manual", allow(&["Notes"]), 60_000, false)
            .unwrap();

        let gate = state
            .begin_action(
                &session.session_id,
                "Notes",
                ControlAction::KeyPress {
                    key: "a".to_string(),
                },
            )
            .unwrap();
        let ActionGate::Pending {
            action_id,
            receiver,
        } = gate
        else {
            panic!("non-batch session must produce a pending approval");
        };

        assert!(state.resolve_if_pending(&action_id, true).unwrap());
        assert_eq!(receiver.blocking_recv(), Ok(true));
    }

    #[test]
    fn pending_action_denial_resumes_as_false() {
        let state = state();
        let session = state
            .start_session_impl("manual", allow(&["Notes"]), 60_000, false)
            .unwrap();

        let gate = state
            .begin_action(
                &session.session_id,
                "Notes",
                ControlAction::KeyPress {
                    key: "a".to_string(),
                },
            )
            .unwrap();
        let ActionGate::Pending {
            action_id,
            receiver,
        } = gate
        else {
            panic!("non-batch session must produce a pending approval");
        };

        assert!(state.resolve_if_pending(&action_id, false).unwrap());
        assert_eq!(receiver.blocking_recv(), Ok(false));
    }

    #[test]
    fn resolving_an_unknown_pending_action_id_is_reported_as_not_found() {
        let state = state();
        assert!(!state.resolve_if_pending("does-not-exist", true).unwrap());
    }

    #[test]
    fn stopping_a_session_denies_its_pending_actions() {
        let state = state();
        let session = state
            .start_session_impl("manual", allow(&["Notes"]), 60_000, false)
            .unwrap();
        let gate = state
            .begin_action(
                &session.session_id,
                "Notes",
                ControlAction::KeyPress {
                    key: "a".to_string(),
                },
            )
            .unwrap();
        let ActionGate::Pending {
            action_id,
            receiver,
        } = gate
        else {
            panic!("non-batch session must produce a pending approval");
        };

        assert!(state.stop_session(&session.session_id).unwrap());

        assert_eq!(receiver.blocking_recv(), Ok(false));
        // The pending entry was consumed by the denial, not left dangling.
        assert!(!state.resolve_if_pending(&action_id, true).unwrap());
    }

    #[test]
    fn emergency_stop_deactivates_sessions_and_cancels_pending_actions_and_is_idempotent() {
        let state = state();
        let session = state
            .start_session_impl("manual", allow(&["Notes"]), 60_000, false)
            .unwrap();
        let gate = state
            .begin_action(
                &session.session_id,
                "Notes",
                ControlAction::KeyPress {
                    key: "a".to_string(),
                },
            )
            .unwrap();
        let ActionGate::Pending { receiver, .. } = gate else {
            panic!("non-batch session must produce a pending approval");
        };

        assert_eq!(state.emergency_stop().unwrap(), (1, 1));
        assert_eq!(receiver.blocking_recv(), Ok(false));
        assert!(!state.sessions_snapshot().unwrap()[0].active);

        // Calling it again with nothing left active/pending must not error
        // and must report zero, not "already stopped".
        assert_eq!(state.emergency_stop().unwrap(), (0, 0));
    }

    #[test]
    fn expired_session_is_reported_inactive_and_rejects_new_actions() {
        let state = state();
        let session = state
            .start_session_impl("manual", allow(&["Notes"]), 1, false)
            .unwrap();
        std::thread::sleep(Duration::from_millis(5));

        let snapshot = state.sessions_snapshot().unwrap();
        assert!(
            !snapshot
                .iter()
                .find(|s| s.session_id == session.session_id)
                .unwrap()
                .active
        );

        assert!(state
            .begin_action(
                &session.session_id,
                "Notes",
                ControlAction::KeyPress {
                    key: "a".to_string()
                }
            )
            .is_err());
    }

    fn temp_lock_path() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("lm-desktop-control-lock-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("desktop_control.lock")
    }

    #[test]
    fn cross_process_lock_refuses_a_second_controller_until_released() {
        let lock_path = temp_lock_path();
        // Two independent controllers (the local app and the resident daemon
        // in production) pointed at the same machine-wide lock file.
        let first = DesktopControlState::with_backend_and_lock(
            Arc::new(NullBackend),
            Some(lock_path.clone()),
        );
        let second = DesktopControlState::with_backend_and_lock(
            Arc::new(NullBackend),
            Some(lock_path.clone()),
        );

        let session = first
            .start_session_impl("manual", allow(&["Notes"]), 60_000, false)
            .expect("first controller should acquire the lock and start");

        // While the first controller holds a live session, the second is
        // refused — not silently allowed to also drive real input.
        let refused = second
            .start_session_impl("manual", allow(&["Notes"]), 60_000, false)
            .unwrap_err();
        assert!(
            refused.contains("Another control session is already active"),
            "unexpected refusal message: {refused}"
        );

        // Releasing via stop_session hands the lock to the second controller.
        assert!(first.stop_session(&session.session_id).unwrap());
        let after_stop = second
            .start_session_impl("manual", allow(&["Notes"]), 60_000, false)
            .expect("second controller should start once the first stops");
        assert!(after_stop.active);

        // Releasing via emergency_stop hands it back to the first controller.
        second.emergency_stop().unwrap();
        let after_emergency = first
            .start_session_impl("manual", allow(&["Notes"]), 60_000, false)
            .expect("first controller should start once the second emergency-stops");
        assert!(after_emergency.active);

        first.emergency_stop().unwrap();
        let _ = std::fs::remove_dir_all(lock_path.parent().unwrap());
    }

    #[test]
    fn a_stale_lock_older_than_the_bound_is_reclaimed() {
        let lock_path = temp_lock_path();
        // A leaked lock file older than any legitimate session lifetime must
        // not permanently wedge desktop control, even if its recorded pid
        // happens to still be a live process.
        let contents = LockContents {
            pid: std::process::id(),
            acquired_at_ms: now_ms().saturating_sub(STALE_LOCK_MS + 1_000),
        };
        std::fs::write(&lock_path, serde_json::to_vec(&contents).unwrap()).unwrap();

        let state = DesktopControlState::with_backend_and_lock(
            Arc::new(NullBackend),
            Some(lock_path.clone()),
        );
        let session = state
            .start_session_impl("manual", allow(&["Notes"]), 60_000, false)
            .expect("a lock older than STALE_LOCK_MS must be reclaimable");
        assert!(session.active);

        state.emergency_stop().unwrap();
        let _ = std::fs::remove_dir_all(lock_path.parent().unwrap());
    }

    #[test]
    fn a_corrupt_lock_file_is_reclaimed() {
        let lock_path = temp_lock_path();
        // A truncated/garbage lock file cannot describe a live owner.
        std::fs::write(&lock_path, b"not json").unwrap();
        let state = DesktopControlState::with_backend_and_lock(
            Arc::new(NullBackend),
            Some(lock_path.clone()),
        );
        let session = state
            .start_session_impl("manual", allow(&["Notes"]), 60_000, false)
            .expect("a corrupt lock must be reclaimable");
        assert!(session.active);
        state.emergency_stop().unwrap();
        let _ = std::fs::remove_dir_all(lock_path.parent().unwrap());
    }

    #[test]
    fn null_backend_always_succeeds_and_unsupported_backend_always_fails_clearly() {
        let null = NullBackend;
        assert!(null.move_mouse(0, 0).is_ok());
        assert!(null.click(MouseButtonKind::Left).is_ok());
        assert!(null.key_press("a").is_ok());

        let unsupported = UnsupportedBackend("no backend wired".to_string());
        assert_eq!(
            unsupported.move_mouse(0, 0).unwrap_err(),
            "no backend wired"
        );
        assert_eq!(
            unsupported.click(MouseButtonKind::Left).unwrap_err(),
            "no backend wired"
        );
        assert_eq!(unsupported.key_press("a").unwrap_err(), "no backend wired");
    }
}
