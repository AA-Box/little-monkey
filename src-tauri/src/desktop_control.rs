//! Safe Desktop Control — the production-gated native Computer Use substrate.
//! Full threat model, platform boundaries, and recovery behavior:
//! `docs/computer-use.md` and `docs/safe-desktop-control-design.md`.
//!
//! All model-facing actions still require a human-created session grant with
//! explicit scope, capability flags, bounded lifetime, and approval policy.
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
//! alongside the real `enigo`-backed implementation. No test in this module
//! exercises anything other than `NullBackend`.
//!
//! Platform support for the real input path ([`EnigoBackend`], selected by
//! [`production_backend`]):
//! - **macOS** — real input (needs Accessibility permission). Runtime-verified.
//! - **Windows** — real input via `enigo` (`SendInput` under the hood).
//! - **Linux/X11** — real input via `enigo` (`x11rb`, `enigo`'s default
//!   feature).
//! - **Linux/Wayland** — deliberately *unsupported*: `production_backend`
//!   detects a Wayland session (see [`is_wayland_session`]) and returns
//!   [`UnsupportedBackend`] rather than constructing `enigo::Enigo`, since
//!   synthetic input on Wayland needs an xdg-desktop-portal/libei integration
//!   that is not built here. X11 sessions work today.
//! - Everything else (BSD, etc.) — [`UnsupportedBackend`], as before.
//!
//! CAUTION: the Windows and Linux code paths below are compiled only on their
//! own target_os, so they are NOT type-checked or runtime-verified in this
//! macOS development environment. All non-trivial platform logic (the Wayland
//! guard) is factored into pure, host-testable functions; the OS-gated blocks
//! themselves are kept to a bare `enigo` call. See each block's own note.

use std::collections::{BTreeMap, HashMap};
use std::io::{Read, Write as _};
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use serde::{de::Deserializer, Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tauri::{Emitter, Manager};
use tokio::sync::oneshot;
use uuid::Uuid;

/// Longest a control session may run before it must be restarted explicitly
/// — mirrors `m7_companion::MAX_GRANT_LIFETIME_MS`'s "bounded, not
/// indefinite" posture for the same reason: an unattended session left open
/// for hours is its own risk even with every other gate in place.
pub const MAX_SESSION_LIFETIME_MS: u64 = 30 * 60 * 1_000;

const DEFAULT_COMPUTER_USE_MAX_ACTIONS: u64 = 50;
const DEFAULT_COMPUTER_USE_MAX_SCREENSHOTS: u64 = 12;
const DEFAULT_COMPUTER_USE_MAX_RETRIES: u64 = 5;
const DEFAULT_COMPUTER_USE_MAX_MODEL_CALLS: u64 = 20;
const DEFAULT_COMPUTER_USE_DEADLINE_MS: u64 = 15 * 60 * 1_000;

/// Shared, atomic limits for one native Computer Use run. The frontend owns
/// model-call/retry charging; this same object owns the host-side action and
/// screenshot counters so concurrent dispatcher paths cannot overspend them.
#[derive(Debug)]
pub struct ComputerUseRunBudget {
    pub max_actions: u64,
    pub max_screenshots: u64,
    pub max_retries: u64,
    pub max_model_calls: u64,
    pub deadline_ms: u64,
    started_at: std::time::Instant,
    actions: AtomicU64,
    screenshots: AtomicU64,
}

impl Default for ComputerUseRunBudget {
    fn default() -> Self {
        Self {
            max_actions: DEFAULT_COMPUTER_USE_MAX_ACTIONS,
            max_screenshots: DEFAULT_COMPUTER_USE_MAX_SCREENSHOTS,
            max_retries: DEFAULT_COMPUTER_USE_MAX_RETRIES,
            max_model_calls: DEFAULT_COMPUTER_USE_MAX_MODEL_CALLS,
            deadline_ms: DEFAULT_COMPUTER_USE_DEADLINE_MS,
            started_at: std::time::Instant::now(),
            actions: AtomicU64::new(0),
            screenshots: AtomicU64::new(0),
        }
    }
}

impl ComputerUseRunBudget {
    fn consume(&self, counter: &str) -> Result<(), String> {
        if self.started_at.elapsed() >= Duration::from_millis(self.deadline_ms) {
            return Err("COMPUTER_USE_BUDGET_EXCEEDED: run deadline reached".to_string());
        }
        let (used, limit) = match counter {
            "actions" => (&self.actions, self.max_actions),
            "screenshots" => (&self.screenshots, self.max_screenshots),
            _ => return Err(format!("unknown Computer Use budget counter {counter}")),
        };
        let mut current = used.load(Ordering::Relaxed);
        loop {
            if current >= limit {
                return Err(format!(
                    "COMPUTER_USE_BUDGET_EXCEEDED: {counter} limit reached"
                ));
            }
            match used.compare_exchange_weak(
                current,
                current + 1,
                Ordering::AcqRel,
                Ordering::Relaxed,
            ) {
                Ok(_) => return Ok(()),
                Err(observed) => current = observed,
            }
        }
    }
}

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

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalPolicy {
    PerAction,
    ApprovedBatch,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalLevel {
    Low,
    Medium,
    High,
    Critical,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ComputerBounds {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ComputerTarget {
    pub target_id: String,
    pub application_id: String,
    pub application_name: String,
    pub window_id: String,
    /// Provider-specific identity retained across X11 window-id
    /// normalization. It is intentionally not exposed to model callers.
    #[serde(skip)]
    pub(crate) provider_window_id: Option<String>,
    pub window_title: String,
    pub bounds: ComputerBounds,
    pub focused: bool,
    pub sensitive: bool,
    #[serde(deserialize_with = "deserialize_vec_or_singleton")]
    pub supported_actions: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ComputerElement {
    pub id: String,
    pub role: String,
    pub label: String,
    pub value: Option<String>,
    pub bounds: ComputerBounds,
    pub enabled: bool,
    pub focused: bool,
    #[serde(deserialize_with = "deserialize_vec_or_singleton")]
    pub actions: Vec<String>,
    pub sensitive: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ComputerInspection {
    pub target: ComputerTarget,
    pub elements: Vec<ComputerElement>,
    pub truncated: bool,
    /// Count only; sensitive elements are never returned with labels or
    /// values, but callers can verify that the provider saw and redacted them.
    pub sensitive_element_count: usize,
    pub query: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ComputerScreenshot {
    pub artifact_id: String,
    pub audit_id: String,
    pub media_type: String,
    pub size_bytes: u64,
    pub content_base64: String,
    pub bounds: ComputerBounds,
    pub target: ComputerTarget,
}

/// A single input action a control session may request. Internally tagged
/// (`kind`) so the frontend's discriminated union matches this shape
/// exactly, and so a future variant can carry its own fields without a
/// serialization migration.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum ControlAction {
    MouseMove {
        x: i32,
        y: i32,
    },
    MouseClick {
        button: MouseButtonKind,
    },
    MouseClickAt {
        x: i32,
        y: i32,
        button: MouseButtonKind,
    },
    MouseDoubleClick {
        button: MouseButtonKind,
    },
    MouseDoubleClickAt {
        x: i32,
        y: i32,
        button: MouseButtonKind,
    },
    MouseDrag {
        from_x: i32,
        from_y: i32,
        to_x: i32,
        to_y: i32,
    },
    Scroll {
        delta_x: i32,
        delta_y: i32,
    },
    TypeText {
        text: String,
    },
    KeyPress {
        key: String,
    },
    Hotkey {
        keys: Vec<String>,
    },
    Focus,
    SemanticClick {
        element_id: String,
        button: MouseButtonKind,
        #[serde(default)]
        expected_value: Option<String>,
    },
    SemanticDoubleClick {
        element_id: String,
        button: MouseButtonKind,
        #[serde(default)]
        expected_value: Option<String>,
    },
    Select {
        element_id: String,
        value: String,
    },
    SetValue {
        element_id: String,
        value: String,
    },
    Wait {
        milliseconds: u64,
    },
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

    fn double_click(&self, button: MouseButtonKind) -> Result<(), String> {
        self.click(button)?;
        self.click(button)
    }

    fn drag(&self, from_x: i32, from_y: i32, to_x: i32, to_y: i32) -> Result<(), String> {
        self.move_mouse(from_x, from_y)?;
        self.click(MouseButtonKind::Left)?;
        self.move_mouse(to_x, to_y)?;
        self.click(MouseButtonKind::Left)
    }

    fn scroll(&self, delta_x: i32, delta_y: i32) -> Result<(), String> {
        if delta_x != 0 {
            self.key_press(&format!("scroll_x:{delta_x}"))?;
        }
        if delta_y != 0 {
            self.key_press(&format!("scroll_y:{delta_y}"))?;
        }
        Ok(())
    }

    fn type_text(&self, text: &str) -> Result<(), String> {
        for character in text.chars() {
            self.key_press(&character.to_string())?;
        }
        Ok(())
    }

    fn hotkey(&self, keys: &[String]) -> Result<(), String> {
        for key in keys {
            self.key_press(key)?;
        }
        Ok(())
    }
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

/// Semantic accessibility seam. The model never receives an unbounded raw OS
/// handle: adapters return this normalized, bounded representation and every
/// mutating operation re-resolves the target immediately before execution.
pub trait DesktopSemanticBackend: Send + Sync {
    fn list_targets(&self) -> Result<Vec<ComputerTarget>, String>;
    fn inspect(
        &self,
        application_id: &str,
        window_id: Option<&str>,
        query: Option<&str>,
    ) -> Result<ComputerInspection, String>;
    fn verify_target(
        &self,
        application_id: &str,
        window_id: Option<&str>,
        require_frontmost: bool,
    ) -> Result<ComputerTarget, String>;
    fn focus(&self, target: &ComputerTarget) -> Result<(), String>;
    fn click_element(
        &self,
        target: &ComputerTarget,
        element_id: &str,
        button: MouseButtonKind,
        double: bool,
    ) -> Result<(), String>;
    fn set_value(
        &self,
        target: &ComputerTarget,
        element_id: &str,
        value: &str,
        select: bool,
    ) -> Result<(), String>;
    fn screenshot(
        &self,
        target: &ComputerTarget,
        bounds: Option<ComputerBounds>,
    ) -> Result<(Vec<u8>, ComputerBounds), String>;
}

#[derive(Default)]
pub struct NullSemanticBackend;

impl DesktopSemanticBackend for NullSemanticBackend {
    fn list_targets(&self) -> Result<Vec<ComputerTarget>, String> {
        Ok(Vec::new())
    }

    fn inspect(
        &self,
        application_id: &str,
        window_id: Option<&str>,
        query: Option<&str>,
    ) -> Result<ComputerInspection, String> {
        let target = self.verify_target(application_id, window_id, false)?;
        Ok(ComputerInspection {
            target,
            elements: Vec::new(),
            truncated: false,
            sensitive_element_count: 0,
            query: query.map(str::to_string),
        })
    }

    fn verify_target(
        &self,
        application_id: &str,
        window_id: Option<&str>,
        _require_frontmost: bool,
    ) -> Result<ComputerTarget, String> {
        Ok(ComputerTarget {
            target_id: format!("{application_id}::{}", window_id.unwrap_or("window")),
            application_id: application_id.to_string(),
            application_name: application_id.to_string(),
            window_id: window_id.unwrap_or("window").to_string(),
            provider_window_id: None,
            window_title: application_id.to_string(),
            bounds: ComputerBounds::default(),
            focused: true,
            sensitive: false,
            supported_actions: vec![
                "inspect".to_string(),
                "focus".to_string(),
                "click".to_string(),
            ],
        })
    }

    fn focus(&self, _target: &ComputerTarget) -> Result<(), String> {
        Ok(())
    }

    fn click_element(
        &self,
        _target: &ComputerTarget,
        _element_id: &str,
        _button: MouseButtonKind,
        _double: bool,
    ) -> Result<(), String> {
        Ok(())
    }

    fn set_value(
        &self,
        _target: &ComputerTarget,
        _element_id: &str,
        _value: &str,
        _select: bool,
    ) -> Result<(), String> {
        Ok(())
    }

    fn screenshot(
        &self,
        target: &ComputerTarget,
        bounds: Option<ComputerBounds>,
    ) -> Result<(Vec<u8>, ComputerBounds), String> {
        Ok((Vec::new(), bounds.unwrap_or_else(|| target.bounds.clone())))
    }
}

/// Real input path (macOS, Windows, and Linux/X11). Not exercised by any test
/// in this module (see the module doc) — only ever constructed by
/// [`production_backend`]. The body is 100% `enigo`'s generic, cross-platform
/// API (`Mouse`/`Keyboard` traits): nothing here is OS-specific, so the same
/// struct/impl compiles unchanged on every supported target. `enigo` handles
/// the per-OS input synthesis internally, in its own crate.
#[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
struct EnigoBackend(Mutex<enigo::Enigo>);

#[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
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

    fn double_click(&self, button: MouseButtonKind) -> Result<(), String> {
        self.click(button)?;
        self.click(button)
    }

    fn drag(&self, from_x: i32, from_y: i32, to_x: i32, to_y: i32) -> Result<(), String> {
        use enigo::{Button, Coordinate, Direction, Mouse};
        let button = Button::Left;
        let mut engine = self
            .0
            .lock()
            .map_err(|_| "desktop input backend lock is poisoned".to_string())?;
        engine
            .move_mouse(from_x, from_y, Coordinate::Abs)
            .map_err(|error| error.to_string())?;
        engine
            .button(button, Direction::Press)
            .map_err(|error| error.to_string())?;
        engine
            .move_mouse(to_x, to_y, Coordinate::Abs)
            .map_err(|error| error.to_string())?;
        engine
            .button(button, Direction::Release)
            .map_err(|error| error.to_string())
    }

    fn scroll(&self, delta_x: i32, delta_y: i32) -> Result<(), String> {
        use enigo::{Axis, Mouse};
        let mut engine = self
            .0
            .lock()
            .map_err(|_| "desktop input backend lock is poisoned".to_string())?;
        if delta_y != 0 {
            engine
                .scroll(delta_y, Axis::Vertical)
                .map_err(|error| error.to_string())?;
        }
        if delta_x != 0 {
            engine
                .scroll(delta_x, Axis::Horizontal)
                .map_err(|error| error.to_string())?;
        }
        Ok(())
    }

    fn type_text(&self, text: &str) -> Result<(), String> {
        use enigo::Keyboard;
        self.0
            .lock()
            .map_err(|_| "desktop input backend lock is poisoned".to_string())?
            .text(text)
            .map_err(|error| error.to_string())
    }

    fn hotkey(&self, keys: &[String]) -> Result<(), String> {
        use enigo::{Direction, Keyboard};
        let parsed: Result<Vec<_>, _> = keys.iter().map(|key| parse_key(key)).collect();
        let parsed = parsed?;
        let mut engine = self
            .0
            .lock()
            .map_err(|_| "desktop input backend lock is poisoned".to_string())?;
        for key in &parsed {
            engine
                .key(*key, Direction::Press)
                .map_err(|error| error.to_string())?;
        }
        for key in parsed.iter().rev() {
            engine
                .key(*key, Direction::Release)
                .map_err(|error| error.to_string())?;
        }
        Ok(())
    }
}

/// A single Unicode character is sent as itself; a small set of named keys
/// covers the common non-printable ones. Anything else is rejected outright
/// — silently guessing at an unrecognized key name is exactly the kind of
/// "might do something other than what was approved" gap this spike avoids.
/// Every `enigo::Key` variant used here is ungated in `enigo`'s `Key` enum, so
/// this parses identically on macOS, Windows, and Linux.
#[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
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
        "ctrl" | "control" => Key::Control,
        "alt" | "option" => Key::Alt,
        "shift" => Key::Shift,
        "cmd" | "command" | "meta" | "super" | "windows" | "win" => Key::Meta,
        "delete" => Key::Delete,
        "up" => Key::UpArrow,
        "down" => Key::DownArrow,
        "left" => Key::LeftArrow,
        "right" => Key::RightArrow,
        _ => return Err(format!("Unsupported key name: {key}")),
    })
}

/// Message returned by [`production_backend`] when a Linux/Wayland session is
/// detected — kept as a named constant so the wording is asserted in tests.
/// Its only production use is Linux-gated, hence the non-Linux `allow`.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
const WAYLAND_UNSUPPORTED_MESSAGE: &str =
    "Wayland session detected — desktop control needs an xdg-desktop-portal/libei integration \
     that isn't built yet; X11 sessions work today.";

/// Pure Wayland-session detector. Takes the relevant environment values as
/// plain `Option<&str>` (it does *not* read the environment itself) so it is
/// fully unit-testable on any host, including this macOS build machine where
/// no `#[cfg(target_os = "linux")]` code is ever compiled.
///
/// Decision:
/// - an explicit, non-empty `XDG_SESSION_TYPE` is authoritative: only the
///   literal `"wayland"` (case-insensitive) counts as Wayland, so `"x11"`
///   (or any other value) is treated as not-Wayland;
/// - otherwise, a set, non-empty `WAYLAND_DISPLAY` is taken as Wayland;
/// - with neither signal present we assume X11/unknown and return `false` (do
///   not block — X11 is the supported Linux path).
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn is_wayland_session(xdg_session_type: Option<&str>, wayland_display: Option<&str>) -> bool {
    if let Some(session_type) = xdg_session_type {
        if !session_type.trim().is_empty() {
            return session_type.trim().eq_ignore_ascii_case("wayland");
        }
    }
    wayland_display.is_some_and(|value| !value.trim().is_empty())
}

/// Thin, Linux-only wrapper that reads the real `XDG_SESSION_TYPE` /
/// `WAYLAND_DISPLAY` env vars and defers the actual decision to the pure
/// [`is_wayland_session`] above.
#[cfg(target_os = "linux")]
fn is_wayland_session_from_env() -> bool {
    let session_type = std::env::var("XDG_SESSION_TYPE").ok();
    let wayland_display = std::env::var("WAYLAND_DISPLAY").ok();
    is_wayland_session(session_type.as_deref(), wayland_display.as_deref())
}

/// Selects the real [`EnigoBackend`] on macOS / Windows / Linux-X11, or a clear
/// [`UnsupportedBackend`] otherwise (Linux-Wayland, other OSes, or when the
/// real backend's own construction fails, e.g. missing Accessibility
/// permission on macOS) — never a silent no-op. Only ever called once, from
/// `DesktopControlState::production`; every test in this module constructs its
/// own [`NullBackend`] instead.
///
/// NOTE: the Windows and Linux arms below are compiled only on their own
/// target_os and were NOT compiled or runtime-verified in this macOS
/// development environment. Each arm is deliberately just a Wayland guard (a
/// pure, host-tested function) plus one generic `enigo::Enigo::new` call whose
/// API is identical across every target.
fn production_backend() -> Arc<dyn DesktopInputBackend> {
    // Linux/Wayland fails *closed and clearly* before any `enigo` construction:
    // building `enigo::Enigo` (x11rb backend) under Wayland would either fail
    // confusingly or behave unpredictably. X11 sessions fall through to enigo.
    #[cfg(target_os = "linux")]
    {
        if is_wayland_session_from_env() {
            return Arc::new(UnsupportedBackend(WAYLAND_UNSUPPORTED_MESSAGE.to_string()));
        }
    }
    #[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
    {
        match enigo::Enigo::new(&enigo::Settings::default()) {
            Ok(engine) => Arc::new(EnigoBackend(Mutex::new(engine))),
            Err(error) => Arc::new(UnsupportedBackend(backend_init_error_message(
                &error.to_string(),
            ))),
        }
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
    {
        Arc::new(UnsupportedBackend(
            "Safe Desktop Control input simulation is not implemented on this platform — a real \
             backend is wired only on macOS, Windows, and Linux/X11"
                .to_string(),
        ))
    }
}

/// Per-OS hint appended to an `enigo::Enigo::new` failure. Kept tiny and
/// cfg-selected (only the host target's arm is ever compiled); the surrounding
/// pure string formatting is what makes the message.
#[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
fn backend_init_error_message(error: &str) -> String {
    #[cfg(target_os = "macos")]
    let hint = "grant Accessibility access in System Settings > Privacy & Security > \
                Accessibility, then restart Little Monkey";
    #[cfg(target_os = "windows")]
    let hint = "the current desktop session may not permit synthetic input (e.g. no interactive \
                desktop, or a higher-integrity window has focus)";
    #[cfg(target_os = "linux")]
    let hint = "ensure an X11 display is reachable (DISPLAY set); Wayland sessions are not \
                supported yet";
    format!("Could not initialize desktop input simulation — {hint}: {error}")
}

const MAX_NATIVE_OUTPUT_BYTES: usize = 8 * 1024 * 1024;
const NATIVE_PROVIDER_TIMEOUT: Duration = Duration::from_secs(15);
const MAX_TARGETS: usize = 64;
const MAX_ELEMENTS: usize = 256;
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
const WAYLAND_PORTAL_MESSAGE: &str =
    "Wayland requires an approved xdg-desktop-portal RemoteDesktop/InputCapture/libei path; \
     Little Monkey will not bypass compositor security";

#[derive(Default, Deserialize)]
struct NativeSnapshot {
    #[serde(default, deserialize_with = "deserialize_vec_or_singleton")]
    targets: Vec<ComputerTarget>,
    #[serde(default, deserialize_with = "deserialize_element_map")]
    elements: HashMap<String, Vec<ComputerElement>>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum OneOrMany<T> {
    Many(Vec<T>),
    One(T),
    None(Option<T>),
}

fn deserialize_vec_or_singleton<'de, D, T>(deserializer: D) -> Result<Vec<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Ok(match OneOrMany::deserialize(deserializer)? {
        OneOrMany::Many(values) => values,
        OneOrMany::One(value) => vec![value],
        OneOrMany::None(_) => Vec::new(),
    })
}

fn deserialize_element_map<'de, D>(
    deserializer: D,
) -> Result<HashMap<String, Vec<ComputerElement>>, D::Error>
where
    D: Deserializer<'de>,
{
    let values: HashMap<String, OneOrMany<ComputerElement>> = HashMap::deserialize(deserializer)?;
    Ok(values
        .into_iter()
        .map(|(key, value)| {
            let elements = match value {
                OneOrMany::Many(elements) => elements,
                OneOrMany::One(element) => vec![element],
                OneOrMany::None(_) => Vec::new(),
            };
            (key, elements)
        })
        .collect())
}

struct NativeSemanticBackend {
    input: Arc<dyn DesktopInputBackend>,
}

fn production_semantic_backend(
    input: Arc<dyn DesktopInputBackend>,
) -> Arc<dyn DesktopSemanticBackend> {
    Arc::new(NativeSemanticBackend { input })
}

fn run_native_command(program: &str, args: &[&str]) -> Result<Vec<u8>, String> {
    run_native_command_with_env(program, args, &[])
}

fn run_native_command_with_env(
    program: &str,
    args: &[&str],
    environment: &[(&str, String)],
) -> Result<Vec<u8>, String> {
    let mut command = Command::new(program);
    command.args(args);
    for (key, value) in environment {
        command.env(key, value);
    }
    let mut child = command
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|error| format!("Could not start accessibility provider {program}: {error}"))?;

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| format!("Accessibility provider {program} did not expose stdout"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| format!("Accessibility provider {program} did not expose stderr"))?;
    let (stderr_sender, stderr_receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut stderr = stderr;
        let mut captured = Vec::new();
        let mut chunk = [0_u8; 4096];
        let result = loop {
            let count = match stderr.read(&mut chunk) {
                Ok(count) => count,
                Err(error) => break Err(error.to_string()),
            };
            if count == 0 {
                break Ok(captured);
            }
            let remaining = 64 * 1024 - captured.len();
            if remaining > 0 {
                captured.extend_from_slice(&chunk[..count.min(remaining)]);
            }
        };
        let _ = stderr_sender.send(result);
    });

    let (stdout_sender, stdout_receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut stdout = stdout;
        let mut stdout_bytes = Vec::new();
        let mut chunk = [0_u8; 16 * 1024];
        let result = loop {
            let count = match stdout.read(&mut chunk) {
                Ok(count) => count,
                Err(error) => break Err(error.to_string()),
            };
            if count == 0 {
                break Ok(stdout_bytes);
            }
            if stdout_bytes.len().saturating_add(count) > MAX_NATIVE_OUTPUT_BYTES {
                break Err(
                    "Accessibility provider returned more than the bounded output limit"
                        .to_string(),
                );
            }
            stdout_bytes.extend_from_slice(&chunk[..count]);
        };
        let _ = stdout_sender.send(result);
    });
    let deadline = std::time::Instant::now() + NATIVE_PROVIDER_TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if std::time::Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("Accessibility provider {program} timed out"));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(10)),
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!(
                    "Could not wait for accessibility provider {program}: {error}"
                ));
            }
        }
    };
    let stdout_bytes = stdout_receiver
        .recv_timeout(Duration::from_secs(1))
        .map_err(|_| format!("Accessibility provider {program} output timed out"))?
        .map_err(|error| format!("Could not read accessibility provider output: {error}"))?;
    let stderr = stderr_receiver
        .recv_timeout(Duration::from_secs(1))
        .map_err(|_| format!("Accessibility provider {program} error output timed out"))?
        .map_err(|error| format!("Could not read accessibility provider error output: {error}"))?;
    if !status.success() {
        let error = String::from_utf8_lossy(&stderr);
        return Err(if error.trim().is_empty() {
            format!("Accessibility provider {program} exited unsuccessfully")
        } else {
            format!("Accessibility provider {program} failed: {}", error.trim())
        });
    }
    Ok(stdout_bytes)
}

fn read_clipboard_native() -> Result<String, String> {
    #[cfg(target_os = "macos")]
    let bytes = run_native_command("pbpaste", &[])?;
    #[cfg(target_os = "windows")]
    let bytes = run_native_command(
        "powershell.exe",
        &[
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            "Get-Clipboard -Raw",
        ],
    )?;
    #[cfg(target_os = "linux")]
    let bytes = {
        if is_wayland_session_from_env() {
            return Err(WAYLAND_PORTAL_MESSAGE.to_string());
        }
        run_native_command("xclip", &["-selection", "clipboard", "-o"])
            .or_else(|_| run_native_command("xsel", &["--clipboard", "--output"]))?
    };
    #[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
    let bytes = return Err("Clipboard access is not implemented on this platform".to_string());
    if bytes.len() > 64 * 1024 {
        return Err("Clipboard content exceeds the 64 KiB Computer Use read bound".to_string());
    }
    String::from_utf8(bytes).map_err(|_| "Clipboard content is not valid UTF-8".to_string())
}

fn native_snapshot() -> Result<NativeSnapshot, String> {
    #[cfg(target_os = "macos")]
    {
        let bytes = run_native_command("osascript", &["-l", "JavaScript", "-e", MACOS_AX_SCRIPT])?;
        return serde_json::from_slice(&bytes)
            .map_err(|error| format!("macOS Accessibility returned invalid data: {error}"));
    }
    #[cfg(target_os = "windows")]
    {
        let bytes = run_native_command(
            "powershell.exe",
            &[
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                WINDOWS_UIA_SCRIPT,
            ],
        )?;
        return serde_json::from_slice(&bytes)
            .map_err(|error| format!("Windows UI Automation returned invalid data: {error}"));
    }
    #[cfg(target_os = "linux")]
    {
        if is_wayland_session_from_env() {
            return Err(WAYLAND_PORTAL_MESSAGE.to_string());
        }
        let bytes = run_native_command("python3", &["-c", LINUX_ATSPI_SCRIPT])?;
        let mut snapshot: NativeSnapshot = serde_json::from_slice(&bytes)
            .map_err(|error| format!("Linux AT-SPI returned invalid data: {error}"))?;
        normalize_linux_window_ids(&mut snapshot);
        return Ok(snapshot);
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
    {
        Err("Semantic accessibility is not implemented on this platform".to_string())
    }
}

#[cfg(target_os = "linux")]
fn normalize_linux_window_ids(snapshot: &mut NativeSnapshot) {
    let Ok(output) = Command::new("wmctrl").args(["-l"]).output() else {
        return;
    };
    let windows: Vec<(String, String)> = String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| {
            let mut fields = line.splitn(4, char::is_whitespace);
            let id = fields.next()?.trim().to_string();
            let _desktop = fields.next()?;
            let _host = fields.next()?;
            let title = fields.next()?.trim().to_string();
            Some((id, title))
        })
        .collect();
    for target in &mut snapshot.targets {
        target.provider_window_id = Some(target.window_id.clone());
        if let Some((window_id, _)) = windows.iter().find(|(_, title)| {
            !target.window_title.is_empty()
                && (title == &target.window_title || title.contains(&target.window_title))
        }) {
            target.window_id = window_id.clone();
        }
    }
}

fn target_is_sensitive(target: &ComputerTarget) -> bool {
    target.sensitive
        || sensitive_text(&format!(
            "{} {} {} {}",
            target.application_id, target.application_name, target.window_id, target.window_title
        ))
}

fn sensitive_text(value: &str) -> bool {
    let normalized = value.to_ascii_lowercase();
    [
        "1password",
        "lastpass",
        "bitwarden",
        "password manager",
        "keychain",
        "securityagent",
        "system settings",
        "system preferences",
        "terminal",
        "iterm",
        "powershell",
        "command prompt",
        "uac",
        "windows security",
        "credential ui",
        "#32770",
        "sudo",
        "authentication",
        "auth dialog",
        "biometric",
        "loginwindow",
        "full disk encryption",
        "filevault",
        "secure password",
    ]
    .iter()
    .any(|token| normalized.contains(token))
}

fn target_matches(target: &ComputerTarget, application_id: &str, window_id: Option<&str>) -> bool {
    let application_match = target.application_id == application_id
        || target.application_name == application_id
        || target.target_id == application_id;
    application_match && window_id.is_none_or(|id| target.window_id == id || target.target_id == id)
}

fn checked_target(
    snapshot: NativeSnapshot,
    application_id: &str,
    window_id: Option<&str>,
    require_frontmost: bool,
) -> Result<ComputerTarget, String> {
    let target = snapshot
        .targets
        .into_iter()
        .find(|target| target_matches(target, application_id, window_id))
        .ok_or_else(|| "Target application/window is stale or no longer visible".to_string())?;
    if target_is_sensitive(&target) {
        return Err("Sensitive application/window targets are blocked".to_string());
    }
    #[cfg(target_os = "windows")]
    verify_windows_target_integrity(&target.window_id)?;
    if require_frontmost && !target.focused {
        return Err("Target is not frontmost; focus it and retry".to_string());
    }
    Ok(target)
}

#[cfg(target_os = "windows")]
fn verify_windows_target_integrity(window_id: &str) -> Result<(), String> {
    let bytes = run_native_command_with_env(
        "powershell.exe",
        &[
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            WINDOWS_SECURITY_SCRIPT,
        ],
        &[("LM_WINDOW_HANDLE", window_id.to_string())],
    )?;
    let result: serde_json::Value = serde_json::from_slice(&bytes)
        .map_err(|error| format!("Windows integrity check returned invalid data: {error}"))?;
    if result.get("ok").and_then(serde_json::Value::as_bool) != Some(true) {
        return Err("Windows target integrity or per-monitor DPI check failed closed".to_string());
    }
    Ok(())
}

fn bounded_elements(
    snapshot: &NativeSnapshot,
    target: &ComputerTarget,
    query: Option<&str>,
) -> (Vec<ComputerElement>, bool, usize) {
    let query = query.map(str::to_ascii_lowercase);
    let mut seen = std::collections::HashSet::new();
    let mut elements = Vec::new();
    let mut truncated = false;
    let mut sensitive_element_count = 0;
    for element in snapshot
        .elements
        .get(&target.target_id)
        .into_iter()
        .flatten()
    {
        if !seen.insert(element.id.clone()) {
            continue;
        }
        if query.as_ref().is_some_and(|needle| {
            !format!(
                "{} {} {}",
                element.role,
                element.label,
                element.value.as_deref().unwrap_or_default()
            )
            .to_ascii_lowercase()
            .contains(needle)
        }) {
            continue;
        }
        if element.sensitive || sensitive_text(&format!("{} {}", element.role, element.label)) {
            sensitive_element_count += 1;
            continue;
        }
        if elements.len() == MAX_ELEMENTS {
            truncated = true;
            break;
        }
        elements.push(element.clone());
    }
    (elements, truncated, sensitive_element_count)
}

fn element_center(element: &ComputerElement) -> Result<(i32, i32), String> {
    if !element.bounds.x.is_finite()
        || !element.bounds.y.is_finite()
        || !element.bounds.width.is_finite()
        || !element.bounds.height.is_finite()
        || element.bounds.width <= 0.0
        || element.bounds.height <= 0.0
    {
        return Err("Accessibility element has no actionable bounds".to_string());
    }
    let x = (element.bounds.x + element.bounds.width / 2.0).round();
    let y = (element.bounds.y + element.bounds.height / 2.0).round();
    if x < f64::from(i32::MIN)
        || x > f64::from(i32::MAX)
        || y < f64::from(i32::MIN)
        || y > f64::from(i32::MAX)
    {
        return Err("Accessibility element bounds exceed native coordinate limits".to_string());
    }
    Ok((x as i32, y as i32))
}

fn find_element(target: &ComputerTarget, element_id: &str) -> Result<ComputerElement, String> {
    let snapshot = native_snapshot()?;
    let verified = checked_target(
        snapshot,
        &target.application_id,
        Some(&target.window_id),
        false,
    )?;
    let refreshed = native_snapshot()?;
    let verified = checked_target(
        refreshed,
        &verified.application_id,
        Some(&verified.window_id),
        false,
    )?;
    let snapshot = native_snapshot()?;
    let (elements, _, _) = bounded_elements(&snapshot, &verified, None);
    elements
        .into_iter()
        .find(|element| element.id == element_id)
        .ok_or_else(|| {
            "Accessibility element is stale or outside the bounded inspection".to_string()
        })
}

impl DesktopSemanticBackend for NativeSemanticBackend {
    fn list_targets(&self) -> Result<Vec<ComputerTarget>, String> {
        let mut targets = native_snapshot()?.targets;
        targets.retain(|target| !target_is_sensitive(target));
        targets.truncate(MAX_TARGETS);
        Ok(targets)
    }

    fn inspect(
        &self,
        application_id: &str,
        window_id: Option<&str>,
        query: Option<&str>,
    ) -> Result<ComputerInspection, String> {
        let snapshot = native_snapshot()?;
        let target = checked_target(snapshot, application_id, window_id, false)?;
        let refreshed = native_snapshot()?;
        let target = checked_target(
            refreshed,
            &target.application_id,
            Some(&target.window_id),
            false,
        )?;
        let snapshot = native_snapshot()?;
        let (elements, truncated, sensitive_element_count) =
            bounded_elements(&snapshot, &target, query);
        Ok(ComputerInspection {
            target,
            elements,
            truncated,
            sensitive_element_count,
            query: query.map(str::to_string),
        })
    }

    fn verify_target(
        &self,
        application_id: &str,
        window_id: Option<&str>,
        require_frontmost: bool,
    ) -> Result<ComputerTarget, String> {
        checked_target(
            native_snapshot()?,
            application_id,
            window_id,
            require_frontmost,
        )
    }

    fn focus(&self, target: &ComputerTarget) -> Result<(), String> {
        #[cfg(target_os = "macos")]
        {
            let index = window_index(&target.window_id)?;
            let bytes = run_native_command_with_env(
                "osascript",
                &["-l", "JavaScript", "-e", MACOS_FOCUS_SCRIPT],
                &[
                    ("LM_APP_ID", target.application_id.clone()),
                    ("LM_WINDOW_INDEX", index.to_string()),
                ],
            )?;
            if serde_json::from_slice::<serde_json::Value>(&bytes)
                .ok()
                .and_then(|json| json.get("focused").and_then(serde_json::Value::as_bool))
                == Some(true)
            {
                return Ok(());
            }
            return Err(
                "macOS Accessibility did not confirm the requested window focus".to_string(),
            );
        }
        #[cfg(target_os = "windows")]
        {
            let script = r#"Add-Type @'
using System;
using System.Runtime.InteropServices;
public static class LMWindow { [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr hWnd); }
'@; [LMWindow]::SetForegroundWindow([IntPtr]::new([int64]$env:LM_WINDOW_HANDLE)) | Out-Null"#;
            return Command::new("powershell.exe")
                .args(["-NoProfile", "-NonInteractive", "-Command", script])
                .env("LM_WINDOW_HANDLE", &target.window_id)
                .status()
                .map_err(|error| format!("Could not focus target: {error}"))
                .and_then(|status| {
                    if status.success() {
                        Ok(())
                    } else {
                        Err("Could not focus target".to_string())
                    }
                });
        }
        #[cfg(target_os = "linux")]
        {
            if is_wayland_session_from_env() {
                return Err(WAYLAND_PORTAL_MESSAGE.to_string());
            }
            return Command::new("wmctrl")
                .args(["-ia", &target.window_id])
                .status()
                .map_err(|error| format!("Could not focus X11 target: {error}"))
                .and_then(|status| {
                    if status.success() {
                        Ok(())
                    } else {
                        Err("Could not focus X11 target".to_string())
                    }
                });
        }
        #[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
        {
            let _ = target;
            Err("Target focus is not implemented on this platform".to_string())
        }
    }

    fn click_element(
        &self,
        target: &ComputerTarget,
        element_id: &str,
        button: MouseButtonKind,
        double: bool,
    ) -> Result<(), String> {
        let element = find_element(target, element_id)?;
        if element.sensitive || sensitive_text(&format!("{} {}", element.role, element.label)) {
            return Err("Sensitive accessibility elements are blocked".to_string());
        }
        if button == MouseButtonKind::Left {
            let action = if double { "double_click" } else { "click" };
            if native_semantic_action(target, element_id, action, None)? {
                return Ok(());
            }
        }
        let (x, y) = element_center(&element)?;
        self.input.move_mouse(x, y)?;
        if double {
            self.input.double_click(button)
        } else {
            self.input.click(button)
        }
    }

    fn set_value(
        &self,
        target: &ComputerTarget,
        element_id: &str,
        value: &str,
        select: bool,
    ) -> Result<(), String> {
        if value.len() > 16 * 1024 || sensitive_text(value) {
            return Err(
                "Sensitive or oversized values cannot be sent through Computer Use".to_string(),
            );
        }
        let element = find_element(target, element_id)?;
        if element.sensitive || sensitive_text(&format!("{} {}", element.role, element.label)) {
            return Err("Sensitive accessibility elements are blocked".to_string());
        }
        let semantic_action = if select { "select" } else { "set_value" };
        if native_semantic_action(target, element_id, semantic_action, Some(value))? {
            return Ok(());
        }
        let (x, y) = element_center(&element)?;
        self.input.move_mouse(x, y)?;
        self.input.click(MouseButtonKind::Left)?;
        if select {
            let modifier = if cfg!(target_os = "macos") {
                "CMD"
            } else {
                "CTRL"
            };
            self.input
                .hotkey(&[modifier.to_string(), "A".to_string()])?;
        }
        self.input.type_text(value)
    }

    fn screenshot(
        &self,
        target: &ComputerTarget,
        bounds: Option<ComputerBounds>,
    ) -> Result<(Vec<u8>, ComputerBounds), String> {
        let requested = bounds.unwrap_or_else(|| target.bounds.clone());
        if !requested.x.is_finite()
            || !requested.y.is_finite()
            || !requested.width.is_finite()
            || !requested.height.is_finite()
            || requested.width <= 0.0
            || requested.height <= 0.0
        {
            return Err("Target has no bounded screenshot region".to_string());
        }
        if target.bounds.width > 0.0
            && (requested.x < target.bounds.x
                || requested.y < target.bounds.y
                || requested.x + requested.width > target.bounds.x + target.bounds.width
                || requested.y + requested.height > target.bounds.y + target.bounds.height)
        {
            return Err("Screenshot region is outside the verified target bounds".to_string());
        }
        let path = std::env::temp_dir().join(format!("little-monkey-shot-{}.png", Uuid::new_v4()));
        let x = requested.x.round() as i32;
        let y = requested.y.round() as i32;
        let width = requested.width.round() as u32;
        let height = requested.height.round() as u32;
        if width == 0 || height == 0 || width > 8192 || height > 8192 {
            return Err("Screenshot region is outside bounded dimensions".to_string());
        }
        #[cfg(target_os = "macos")]
        let result = Command::new("screencapture")
            .args(["-x", "-R", &format!("{x},{y},{width},{height}")])
            .arg(&path)
            .status()
            .map_err(|error| format!("Could not capture macOS screenshot: {error}"));
        #[cfg(target_os = "windows")]
        let result = Command::new("powershell.exe")
            .args([
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                WINDOWS_SCREENSHOT_SCRIPT,
            ])
            .env("LM_SCREENSHOT_PATH", &path)
            .env("LM_SCREENSHOT_X", x.to_string())
            .env("LM_SCREENSHOT_Y", y.to_string())
            .env("LM_SCREENSHOT_W", width.to_string())
            .env("LM_SCREENSHOT_H", height.to_string())
            .status()
            .map_err(|error| format!("Could not capture Windows screenshot: {error}"));
        #[cfg(target_os = "linux")]
        let result = {
            if is_wayland_session_from_env() {
                return Err(WAYLAND_PORTAL_MESSAGE.to_string());
            }
            let geometry = format!("{x},{y} {width}x{height}");
            let scrot = Command::new("scrot")
                .args(["-a", &geometry])
                .arg(&path)
                .status();
            match scrot {
                Ok(status) if status.success() => Ok(status),
                _ => Command::new("import")
                    .args([
                        "-window",
                        "root",
                        "-crop",
                        &format!("{width}x{height}+{x}+{y}"),
                    ])
                    .arg(&path)
                    .status()
                    .map_err(|error| format!("Could not capture bounded X11 screenshot: {error}")),
            }
        };
        #[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
        let result: Result<std::process::ExitStatus, std::io::Error> =
            Err(std::io::Error::other("unsupported"));
        let status = result?;
        if !status.success() {
            return Err("Screenshot provider exited unsuccessfully".to_string());
        }
        let bytes = std::fs::read(&path)
            .map_err(|error| format!("Could not read screenshot artifact: {error}"));
        let _ = std::fs::remove_file(&path);
        let bytes = bytes?;
        if bytes.len() > MAX_NATIVE_OUTPUT_BYTES {
            return Err("Screenshot exceeds bounded artifact size".to_string());
        }
        Ok((bytes, requested))
    }
}

#[cfg(target_os = "macos")]
const MACOS_AX_SCRIPT: &str = r#"
ObjC.import('AppKit');
const se = Application('System Events');
const providerEnv = $.NSProcessInfo.processInfo.environment;
const onlyPid = Number(ObjC.unwrap(providerEnv.objectForKey('COMPUTER_USE_FIXTURE_PID')) || 0);
const onlyName = String(ObjC.unwrap(providerEnv.objectForKey('COMPUTER_USE_FIXTURE_APP_NAME')) || '');
const safe = (f, d) => { try { const v = f(); return v === undefined ? d : v; } catch (_) { return d; } };
const text = (...fs) => { for (const f of fs) { const value = safe(f, ''); if (value !== null && value !== undefined && String(value).trim() !== '') return String(value); } return ''; };
const rect = o => { const p = safe(() => o.position(), [0,0]); const s = safe(() => o.size(), [0,0]); return {x:Number(p[0])||0,y:Number(p[1])||0,width:Number(s[0])||0,height:Number(s[1])||0}; };
const targets = [], elements = {};
let processList = [];
if (onlyName) {
  const selected = safe(() => se.processes.byName(onlyName), null);
  if (selected) processList = [selected];
} else if (onlyPid) {
  for (const candidate of safe(() => se.processes(), [])) {
    if (Number(safe(() => candidate.unixId(), 0)) === onlyPid) { processList = [candidate]; break; }
  }
} else {
  processList = safe(() => se.processes(), []);
}
for (const p of processList) {
  try {
    if (onlyPid && Number(safe(() => p.unixId(), 0)) !== onlyPid) continue;
    if (!safe(() => p.visible(), false)) continue;
    const name = String(safe(() => p.name(), '')); const bundle = String(safe(() => p.bundleIdentifier(), '')); const app = bundle === 'null' || bundle === 'undefined' || !bundle ? name : bundle;
    const workspaceFrontPid = Number(safe(() => $.NSWorkspace.sharedWorkspace.frontmostApplication.processIdentifier, 0));
    const front = onlyPid ? workspaceFrontPid === onlyPid : Boolean(safe(() => p.frontmost(), false)); let wi = 0;
    for (const w of safe(() => p.windows(), [])) {
      if (wi >= 32) break;
      const title = String(safe(() => w.name(), '')); const id = app + '::window-' + wi; const target = {targetId:id,applicationId:app,applicationName:name,windowId:id,windowTitle:title,bounds:rect(w),focused:front && wi===0,sensitive:false,supportedActions:['inspect','focus','click','double_click','scroll','type','key','hotkey','screenshot']}; targets.push(target);
      const out=[]; let ei=0;
      for (const e of safe(() => w.entireContents(), [])) { if (ei++ >= 256) break; const role=String(safe(() => e.role(),'')); const subrole=String(safe(() => e.attribute('AXSubrole'),'')); const label=text(() => e.attribute('AXTitle'), () => e.description(), () => e.name()); const value=safe(() => e.value(), null); const native=String(safe(() => e.attribute('AXIdentifier'), '')); const stable=native.replace(/[^A-Za-z0-9._-]/g,'_'); const eb=rect(e); out.push({id:id+'::element-'+(ei-1)+'::native-'+stable,role,label,value:value===null?null:String(value),bounds:eb,enabled:Boolean(safe(() => e.enabled(),true)),focused:Boolean(safe(() => e.focused(),false)),actions:['click','double_click','set_value','select'],sensitive:/AXSecureTextField|securetextfield|password|secure|auth|credential/i.test(role+' '+subrole+' '+label)}); }
      elements[id]=out; wi++;
    }
  } catch (_) {}
}
JSON.stringify({targets,elements});
"#;

#[cfg(target_os = "windows")]
const WINDOWS_SECURITY_SCRIPT: &str = r#"
Add-Type @'
using System;
using System.Runtime.InteropServices;
public static class LMComputerUseSecurity {
  [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr hWnd, out uint pid);
  [DllImport("user32.dll")] public static extern IntPtr SetThreadDpiAwarenessContext(IntPtr context);
  [DllImport("user32.dll")] public static extern uint GetDpiForWindow(IntPtr hWnd);
  [DllImport("kernel32.dll")] public static extern uint GetCurrentProcessId();
  [DllImport("kernel32.dll")] public static extern IntPtr OpenProcess(uint access, bool inherit, uint pid);
  [DllImport("kernel32.dll")] public static extern bool CloseHandle(IntPtr handle);
  [DllImport("advapi32.dll", SetLastError=true)] public static extern bool OpenProcessToken(IntPtr process, uint access, out IntPtr token);
  [DllImport("advapi32.dll", SetLastError=true)] public static extern bool GetTokenInformation(IntPtr token, int kind, IntPtr data, uint length, out uint returned);
  public static int Integrity(uint pid) {
    var process=OpenProcess(0x1000, false, pid); if(process==IntPtr.Zero) return -1;
    IntPtr token; if(!OpenProcessToken(process, 0x0008, out token)){CloseHandle(process);return -1;}
    uint length; GetTokenInformation(token, 25, IntPtr.Zero, 0, out length);
    var buffer=Marshal.AllocHGlobal((int)length); int level=-1;
    if(GetTokenInformation(token, 25, buffer, length, out length)) {
      var sid=Marshal.ReadIntPtr(buffer); var count=Marshal.ReadByte(sid, 1);
      level=Marshal.ReadInt32(sid, 8 + 4 * (count - 1));
    }
    Marshal.FreeHGlobal(buffer); CloseHandle(token); CloseHandle(process); return level;
  }
  public static int TargetIntegrity(IntPtr hwnd) { uint pid; GetWindowThreadProcessId(hwnd, out pid); return Integrity(pid); }
}
'@
$handle=[IntPtr]::new([int64]$env:LM_WINDOW_HANDLE)
[LMComputerUseSecurity]::SetThreadDpiAwarenessContext([IntPtr]::new(-4)) | Out-Null
$target=[LMComputerUseSecurity]::TargetIntegrity($handle)
$current=[LMComputerUseSecurity]::Integrity([LMComputerUseSecurity]::GetCurrentProcessId())
$dpi=[LMComputerUseSecurity]::GetDpiForWindow($handle)
if($target -lt 0 -or $current -lt 0 -or $dpi -le 0){throw 'Could not determine Windows target integrity or per-monitor DPI'}
if($target -gt $current){throw "Refusing higher-integrity target ($target > $current)"}
[ordered]@{ok=$true;target_integrity=$target;current_integrity=$current;dpi=$dpi}|ConvertTo-Json -Compress
"#;

#[cfg(target_os = "windows")]
const WINDOWS_UIA_SCRIPT: &str = r#"
Add-Type -AssemblyName UIAutomationClient
Add-Type -AssemblyName UIAutomationTypes
Add-Type -AssemblyName System.Windows.Forms
Add-Type @'
using System;
using System.Runtime.InteropServices;
public static class LMComputerUseDpi { [DllImport("user32.dll")] public static extern IntPtr SetThreadDpiAwarenessContext(IntPtr context); }
'@
[LMComputerUseDpi]::SetThreadDpiAwarenessContext([IntPtr]::new(-4)) | Out-Null
$root=[System.Windows.Automation.AutomationElement]::RootElement
$targets=@();$elements=@{}
$windows=$root.FindAll([System.Windows.Automation.TreeScope]::Children,[System.Windows.Automation.Condition]::TrueCondition)
$onlyPid=0
try { if($env:COMPUTER_USE_FIXTURE_PID){$onlyPid=[int]$env:COMPUTER_USE_FIXTURE_PID} } catch {}
$walker=[System.Windows.Automation.TreeWalker]::ControlViewWalker
$fixtureFallback=$false
if($onlyPid){
  $windows=@($windows | Where-Object {$_.Current.ProcessId -eq $onlyPid})
  if($windows.Count -eq 0){
    $windows=@($root.FindAll([System.Windows.Automation.TreeScope]::Children,[System.Windows.Automation.Condition]::TrueCondition) | Where-Object {$_.Current.Name -like 'Little Monkey TestApp*'})
    $fixtureFallback=$windows.Count -gt 0
  }
}
function ParentOf($e) { try { return $walker.GetParent($e) } catch { return $null } }
function ValueOf($e) {
  try { $value=$e.GetCurrentPattern([System.Windows.Automation.ValuePattern]::Pattern).Current.Value; if($null -ne $value){return $value} } catch {}
  try { $value=[string]$e.GetCurrentPattern([System.Windows.Automation.TogglePattern]::Pattern).Current.ToggleState; if(-not [string]::IsNullOrWhiteSpace($value)){return $value} } catch {}
  try { $value=[string]$e.Current.HelpText; if(-not [string]::IsNullOrWhiteSpace($value)){return $value} } catch {}
  $current=$e
  for($k=0;$k -lt 4;$k++) {
    $current=ParentOf $current
    if($null -eq $current){break}
    try { $value=[string]$current.GetCurrentPattern([System.Windows.Automation.TogglePattern]::Pattern).Current.ToggleState; if(-not [string]::IsNullOrWhiteSpace($value)){return $value} } catch {}
    try { $value=[string]$current.Current.HelpText; if(-not [string]::IsNullOrWhiteSpace($value)){return $value} } catch {}
  }
  return $null
}
function ActionsOf($e) {
  $a=@()
  try { $e.GetCurrentPattern([System.Windows.Automation.InvokePattern]::Pattern) | Out-Null; $a+='click'; $a+='double_click' } catch {}
  try { $e.GetCurrentPattern([System.Windows.Automation.TogglePattern]::Pattern) | Out-Null; $a+='click' } catch {}
  try { $e.GetCurrentPattern([System.Windows.Automation.ValuePattern]::Pattern) | Out-Null; $a+='set_value' } catch {}
  try { $e.GetCurrentPattern([System.Windows.Automation.SelectionItemPattern]::Pattern) | Out-Null; $a+='select' } catch {}
  try { $e.GetCurrentPattern([System.Windows.Automation.LegacyIAccessiblePattern]::Pattern) | Out-Null; $a+='click' } catch {}
  if(([string]$e.Current.ControlType.ProgrammaticName -match 'Edit') -and -not ($a -contains 'set_value')){$a+='set_value'}
  if($a.Count -eq 0) {
    $current=$e
    for($k=0;$k -lt 4;$k++) {
      $current=ParentOf $current
      if($null -eq $current){break}
      try { $current.GetCurrentPattern([System.Windows.Automation.TogglePattern]::Pattern) | Out-Null; $a+='click'; break } catch {}
      try { $current.GetCurrentPattern([System.Windows.Automation.InvokePattern]::Pattern) | Out-Null; $a+='click'; $a+='double_click'; break } catch {}
      try { $current.GetCurrentPattern([System.Windows.Automation.LegacyIAccessiblePattern]::Pattern) | Out-Null; $a+='click'; break } catch {}
    }
  }
  return @($a | Select-Object -Unique)
}
function RectOf($rect) { $x=0.0;$y=0.0;$width=0.0;$height=0.0;try{$x=[double]$rect.X;$y=[double]$rect.Y;$width=[double]$rect.Width;$height=[double]$rect.Height}catch{};[ordered]@{x=$x;y=$y;width=$width;height=$height} }
function AncestorText($e) { $parts=@();$current=$e;for($k=0;$k -lt 8;$k++){try{$current=$walker.GetParent($current);if($null -eq $current){break};$parts+=[string]$current.Current.Name}catch{break}};return ($parts -join ' ') }
for($i=0;$i -lt $windows.Count -and $i -lt 64;$i++){
  $w=$windows.Item($i);$p=$w.Current.ProcessId;$id=if($fixtureFallback){"process:$onlyPid"}else{"process:$p"};$name=[string]$w.Current.Name;$windowId=[string]$w.Current.NativeWindowHandle;$targetId="$id::window-$i";
  $t=[ordered]@{targetId=$targetId;applicationId=$id;applicationName=$name;windowId=$windowId;windowTitle=$name;bounds=(RectOf $w.Current.BoundingRectangle);focused=([bool]$w.Current.HasKeyboardFocus);sensitive=($name -match 'UAC|Windows Security|credential|password');supportedActions=@('inspect','focus','click','double_click','scroll','type','key','hotkey','screenshot')};$targets+=$t;$list=@();$desc=$w.FindAll([System.Windows.Automation.TreeScope]::Descendants,[System.Windows.Automation.Condition]::TrueCondition);
  for($j=0;$j -lt $desc.Count -and $j -lt 256;$j++){
    $e=$desc.Item($j);$label=[string]$e.Current.Name;$help=[string]$e.Current.HelpText;$role=[string]$e.Current.ControlType.ProgrammaticName;$automation=[string]$e.Current.AutomationId;if([string]::IsNullOrWhiteSpace($automation)){try{$automation=($e.GetRuntimeId() -join '-')}catch{$automation=''}};$stable=($automation -replace '[^A-Za-z0-9._-]','_');$value=ValueOf $e;if($null -ne $value){$value=[string]$value};$actions=@(ActionsOf $e);$context="$role $label $help $automation $(AncestorText $e)";$enabled=([bool]$e.Current.IsEnabled);try{$legacy=$e.GetCurrentPattern([System.Windows.Automation.LegacyIAccessiblePattern]::Pattern).Current;if(([int]$legacy.State -band 1) -ne 0){$enabled=$false}}catch{};$list+=[ordered]@{id="$targetId::element-$j::native-$stable";role=$role;label=$label;value=$value;bounds=(RectOf $e.Current.BoundingRectangle);enabled=$enabled;focused=([bool]$e.Current.HasKeyboardFocus);actions=$actions;sensitive=([bool]$e.Current.IsPassword -or ($context -match 'password|credential|secret'))};
  }
  $elements[$targetId]=@($list)
}
[ordered]@{targets=$targets;elements=$elements}|ConvertTo-Json -Compress -Depth 8
"#;

#[cfg(target_os = "windows")]
const WINDOWS_SCREENSHOT_SCRIPT: &str = r#"
Add-Type -AssemblyName System.Drawing
Add-Type @'
using System; using System.Drawing; using System.Drawing.Imaging; using System.Windows.Forms;
'@
Add-Type @'
using System; using System.Runtime.InteropServices;
public static class LMComputerUseDpi { [DllImport("user32.dll")] public static extern IntPtr SetThreadDpiAwarenessContext(IntPtr context); }
'@
[LMComputerUseDpi]::SetThreadDpiAwarenessContext([IntPtr]::new(-4)) | Out-Null
$x=[int]$env:LM_SCREENSHOT_X;$y=[int]$env:LM_SCREENSHOT_Y;$w=[int]$env:LM_SCREENSHOT_W;$h=[int]$env:LM_SCREENSHOT_H
$bmp=New-Object Drawing.Bitmap $w,$h;$g=[Drawing.Graphics]::FromImage($bmp);$g.CopyFromScreen($x,$y,0,0,$bmp.Size);$bmp.Save($env:LM_SCREENSHOT_PATH,[Drawing.Imaging.ImageFormat]::Png);$g.Dispose();$bmp.Dispose()
"#;

#[cfg(target_os = "linux")]
const LINUX_ATSPI_SCRIPT: &str = r#"
import json
try:
 import pyatspi
except Exception as e:
 raise SystemExit('AT-SPI unavailable: '+str(e))
def rect(o):
 try:
  b=o.queryComponent().getExtents(pyatspi.DESKTOP_COORDS)
  return {'x':b.x,'y':b.y,'width':b.width,'height':b.height}
 except Exception: return {'x':0,'y':0,'width':0,'height':0}
def provider_part(node, path):
 role = str(node.getRoleName() or '')
 name = str(getattr(node, 'name', '') or '')
 return (':'.join(str(index) for index in path) + ':' + role + ':' + name).replace(' ', '_')[:160]
def walk(node, path=()):
 for index, child in enumerate(list(node)):
  child_path = path + (index,)
  yield child, child_path
  yield from walk(child, child_path)
targets=[]; elements={}; desktop=pyatspi.Registry.getDesktop(0)
for app in list(desktop)[:64]:
 raw_name=str(getattr(app,'name','') or '')
 name=raw_name or 'Python'
 aid='atspi:'+name
 for wi,w in enumerate(list(app)[:32]):
  title=str(getattr(w,'name','')); tid=aid+'::window-'+str(wi); st=w.getState(); target={'targetId':tid,'applicationId':aid,'applicationName':name,'windowId':tid,'windowTitle':title,'bounds':rect(w),'focused':bool(st.contains(pyatspi.STATE_ACTIVE)),'sensitive':False,'supportedActions':['inspect','focus','click','double_click','scroll','type','key','hotkey','screenshot']};targets.append(target); out=[]
  for ei,(e,path) in enumerate(list(walk(w))[:256]):
   role=str(e.getRoleName()); label=str(getattr(e,'name','')); value=None
   try: value=str(e.queryValue().getCurrentValue())
   except Exception: pass
   try:
    editable=e.queryEditableText()
    count=getattr(editable,'characterCount',0)
    if callable(count): count=count()
    value=str(editable.getText(0,int(count)))
   except Exception:
    try:
     text_iface=e.queryText()
     count=getattr(text_iface,'characterCount',0)
     if callable(count): count=count()
     value=str(text_iface.getText(0,int(count)))
    except Exception: pass
   try:
    state=e.getState()
    if 'check' in role.lower() or 'toggle' in role.lower(): value='on' if state.contains(pyatspi.STATE_CHECKED) else 'off'
   except Exception: pass
   actions=[]
   try:
    qa=e.queryAction()
    for ai in range(qa.nActions):
     name=(qa.getName(ai) or '').lower()
     if name in ('click','press','activate','select'): actions.append(name)
   except Exception: pass
   try: e.queryEditableText(); actions.append('set_value')
   except Exception: pass
   stable=provider_part(e,path)
   try: enabled=bool(e.getState().contains(pyatspi.STATE_ENABLED))
   except Exception: enabled=True
   try: focused=bool(e.getState().contains(pyatspi.STATE_FOCUSED))
   except Exception: focused=False
   out.append({'id':tid+'::element-'+str(ei)+'::native-'+stable,'role':role,'label':label,'value':value,'bounds':rect(e),'enabled':enabled,'focused':focused,'actions':list(dict.fromkeys(actions)),'sensitive':any(token in (role+' '+label).lower() for token in ('password','secure','credential','authentication'))})
  elements[tid]=out
print(json.dumps({'targets':targets,'elements':elements},separators=(',',':')))
"#;

#[cfg(target_os = "macos")]
const MACOS_AX_ACTION_SCRIPT: &str = r#"
ObjC.import('Foundation');
const se = Application('System Events');
const env = $.NSProcessInfo.processInfo.environment;
const get = key => ObjC.unwrap(env.objectForKey(key));
const safe = (f, d) => { try { const v = f(); return v === undefined ? d : v; } catch (_) { return d; } };
const text = (...fs) => { for (const f of fs) { const value = safe(f, ''); if (value !== null && value !== undefined && String(value).trim() !== '') return String(value); } return ''; };
const appId = get('LM_APP_ID');
const windowIndex = Number(get('LM_WINDOW_INDEX'));
const elementIndex = Number(get('LM_ELEMENT_INDEX'));
const stable = get('LM_ELEMENT_STABLE');
const action = get('LM_ACTION');
const value = get('LM_VALUE');
const process = /^(com|org|net|io)\./.test(appId) ? se.processes.byBundleIdentifier(appId) : se.processes.byName(appId);
const window = process.windows[windowIndex];
const contents = window.entireContents();
let element = null;
if (stable) {
  element = contents.find(e => String(safe(() => e.attribute('AXIdentifier'), '')).replace(/[^A-Za-z0-9._-]/g, '_') === stable) || null;
  if (!element) throw new Error('macOS Accessibility element is stale');
} else {
  element = contents[elementIndex];
}
if (!element) throw new Error('macOS Accessibility element is stale');
if (action === 'set_value') element.value = value;
else if (action === 'select') { try { element.performAction('AXPress'); } catch (_) { element.click(); } }
else if (action === 'click') { try { element.performAction('AXPress'); } catch (_) { element.click(); } }
else if (action === 'double_click') { try { element.performAction('AXPress'); element.performAction('AXPress'); } catch (_) { element.click(); element.click(); } }
else throw new Error('unsupported semantic action');
JSON.stringify({semantic:true});
"#;

#[cfg(target_os = "macos")]
const MACOS_FOCUS_SCRIPT: &str = r#"
ObjC.import('Foundation');
ObjC.import('AppKit');
const se = Application('System Events');
const env = $.NSProcessInfo.processInfo.environment;
const get = key => ObjC.unwrap(env.objectForKey(key));
const safe = (f, d) => { try { const v = f(); return v === undefined ? d : v; } catch (_) { return d; } };
const appId = get('LM_APP_ID');
const fixturePid = Number(get('COMPUTER_USE_FIXTURE_PID') || 0);
const process = /^(com|org|net|io)\./.test(appId) ? se.processes.byBundleIdentifier(appId) : se.processes.byName(appId);
if (fixturePid) safe(() => { const app = $.NSRunningApplication.runningApplicationWithProcessIdentifier(fixturePid); app.activateWithOptions(2); return true; }, false);
process.frontmost = true;
const frontPid = Number(safe(() => $.NSWorkspace.sharedWorkspace.frontmostApplication.processIdentifier, 0));
JSON.stringify({focused:fixturePid ? frontPid === fixturePid : Boolean(process.frontmost())});
"#;

#[cfg(target_os = "windows")]
const WINDOWS_UIA_ACTION_SCRIPT: &str = r#"
Add-Type -AssemblyName UIAutomationClient
Add-Type -AssemblyName UIAutomationTypes
Add-Type @'
using System; using System.Runtime.InteropServices;
public static class LMComputerUseDpi { [DllImport("user32.dll")] public static extern IntPtr SetThreadDpiAwarenessContext(IntPtr context); }
'@
[LMComputerUseDpi]::SetThreadDpiAwarenessContext([IntPtr]::new(-4)) | Out-Null
Add-Type @'
using System;
using System.Runtime.InteropServices;
public static class LMComputerUseNative {
  [DllImport("user32.dll")] public static extern IntPtr SendMessage(IntPtr hWnd, uint message, IntPtr wParam, IntPtr lParam);
}
'@
$root=[System.Windows.Automation.AutomationElement]::FromHandle([IntPtr]::new([int64]$env:LM_WINDOW_HANDLE))
$desc=$root.FindAll([System.Windows.Automation.TreeScope]::Descendants,[System.Windows.Automation.Condition]::TrueCondition)
$stable=$env:LM_ELEMENT_STABLE
$walker=[System.Windows.Automation.TreeWalker]::ControlViewWalker
function ResolveElement {
  $candidates=$root.FindAll([System.Windows.Automation.TreeScope]::Descendants,[System.Windows.Automation.Condition]::TrueCondition)
  if(-not [string]::IsNullOrWhiteSpace($stable)) {
    for($i=0;$i -lt $candidates.Count;$i++) {
      $candidate=$candidates.Item($i)
      $automation=[string]$candidate.Current.AutomationId
      if([string]::IsNullOrWhiteSpace($automation)){try{$automation=($candidate.GetRuntimeId() -join '-')}catch{$automation=''}}
      $candidateStable=($automation -replace '[^A-Za-z0-9._-]','_')
      if($candidateStable -eq $stable){return $candidate}
    }
    throw 'UIAutomation element is stale'
  }
  return $candidates.Item([int]$env:LM_ELEMENT_INDEX)
}
$action=$env:LM_ACTION
function ResolveActionElement {
  $current=ResolveElement
  for($k=0;$k -lt 5;$k++) {
    if($action -eq 'set_value') { try { $current.GetCurrentPattern([System.Windows.Automation.ValuePattern]::Pattern) | Out-Null; return $current } catch {} }
    else { try { $current.GetCurrentPattern([System.Windows.Automation.TogglePattern]::Pattern) | Out-Null; return $current } catch {}; try { $current.GetCurrentPattern([System.Windows.Automation.InvokePattern]::Pattern) | Out-Null; return $current } catch {}; try { $current.GetCurrentPattern([System.Windows.Automation.LegacyIAccessiblePattern]::Pattern) | Out-Null; return $current } catch {} }
    try { $current=$walker.GetParent($current) } catch { break }
    if($null -eq $current){break}
  }
  return $current
}
$e=ResolveActionElement
$performed=$false
if($action -eq 'set_value') {
  try { $p=$e.GetCurrentPattern([System.Windows.Automation.ValuePattern]::Pattern); $p.SetValue($env:LM_VALUE); $performed=$true } catch {}
} elseif($action -eq 'select') {
  try { $p=$e.GetCurrentPattern([System.Windows.Automation.SelectionItemPattern]::Pattern); $p.Select(); $performed=$true } catch {}
} elseif($action -eq 'click' -or $action -eq 'double_click') {
  $toggleSupported=$false
  try {
    $p=$e.GetCurrentPattern([System.Windows.Automation.TogglePattern]::Pattern)
    $toggleSupported=$true
    $before=$p.Current.ToggleState
    $p.Toggle()
    for($wait=0;$wait -lt 10 -and -not $performed;$wait++) {
      Start-Sleep -Milliseconds 100
      try {
        $fresh=ResolveActionElement
        $after=$fresh.GetCurrentPattern([System.Windows.Automation.TogglePattern]::Pattern).Current.ToggleState
        if($after -ne $before){$performed=$true}
      } catch {}
    }
    if(-not $performed) {
      try {
        $fresh=ResolveActionElement
        $p=$fresh.GetCurrentPattern([System.Windows.Automation.InvokePattern]::Pattern)
        $p.Invoke()
        for($wait=0;$wait -lt 10 -and -not $performed;$wait++) {
          Start-Sleep -Milliseconds 100
          try {
            $fresh=ResolveActionElement
            $after=$fresh.GetCurrentPattern([System.Windows.Automation.TogglePattern]::Pattern).Current.ToggleState
            if($after -ne $before){$performed=$true}
          } catch {}
        }
      } catch {}
    }
    if(-not $performed) {
      try {
        $fresh=ResolveActionElement
        $handle=[int64]$fresh.Current.NativeWindowHandle
        if($handle -ne 0) {
          [LMComputerUseNative]::SendMessage([IntPtr]::new($handle), 0x00F5, [IntPtr]::Zero, [IntPtr]::Zero) | Out-Null
          for($wait=0;$wait -lt 10 -and -not $performed;$wait++) {
            Start-Sleep -Milliseconds 100
            try {
              $fresh=ResolveActionElement
              $after=$fresh.GetCurrentPattern([System.Windows.Automation.TogglePattern]::Pattern).Current.ToggleState
              if($after -ne $before){$performed=$true}
            } catch {}
          }
        }
      } catch {}
    }
    if(-not $performed) {
      try {
        $fresh=ResolveActionElement
        $fresh.SetFocus()
        [System.Windows.Forms.SendKeys]::SendWait(' ')
        for($wait=0;$wait -lt 10 -and -not $performed;$wait++) {
          Start-Sleep -Milliseconds 100
          try {
            $fresh=ResolveActionElement
            $after=$fresh.GetCurrentPattern([System.Windows.Automation.TogglePattern]::Pattern).Current.ToggleState
            if($after -ne $before){$performed=$true}
          } catch {}
        }
      } catch {}
    }
  } catch {}
  if(-not $performed -and -not $toggleSupported) {
    try { $p=$e.GetCurrentPattern([System.Windows.Automation.InvokePattern]::Pattern); $p.Invoke(); $performed=$true } catch {}
    if(-not $performed) { try { $p=$e.GetCurrentPattern([System.Windows.Automation.LegacyIAccessiblePattern]::Pattern); $p.DoDefaultAction(); $performed=$true } catch {} }
  }
  if($performed -and $action -eq 'double_click') { try { $fresh=ResolveActionElement; if($toggleSupported) { $fresh.GetCurrentPattern([System.Windows.Automation.TogglePattern]::Pattern).Toggle() } else { $fresh.GetCurrentPattern([System.Windows.Automation.InvokePattern]::Pattern).Invoke() } } catch {} }
}
if($performed) { [ordered]@{semantic=$true}|ConvertTo-Json -Compress } else { [ordered]@{semantic=$false}|ConvertTo-Json -Compress }
"#;

#[cfg(target_os = "linux")]
const LINUX_ATSPI_ACTION_SCRIPT: &str = r#"
import os, json
import pyatspi
def provider_part(node, path):
 role=str(node.getRoleName() or '')
 name=str(getattr(node, 'name', '') or '')
 return (':'.join(str(index) for index in path)+':'+role+':'+name).replace(' ','_')[:160]
def walk(node, path=()):
 for index, child in enumerate(list(node)):
  child_path=path+(index,)
  yield child, child_path
  yield from walk(child, child_path)
app_name=os.environ['LM_APP_NAME']; wi=int(os.environ['LM_WINDOW_INDEX']); ei=int(os.environ['LM_ELEMENT_INDEX']); stable=os.environ.get('LM_ELEMENT_STABLE','')
a=None
for candidate in list(pyatspi.Registry.getDesktop(0)):
 if str(getattr(candidate,'name','')) == app_name: a=candidate; break
if a is None: raise SystemExit('AT-SPI application is stale')
w=list(a)[wi]; entries=list(walk(w)); e=None
if stable:
 for candidate,path in entries:
  if provider_part(candidate,path)==stable: e=candidate; break
 if e is None: raise SystemExit('AT-SPI element is stale')
else:
 e=entries[ei][0]
if e is None: raise SystemExit('AT-SPI element is stale')
action=os.environ['LM_ACTION']
if action in ('click','double_click','select'):
 actions=e.queryAction(); done=False
 for i in range(actions.nActions):
  name=(actions.getName(i) or '').lower()
  if name in ('click','press','activate','select'):
   actions.doAction(i)
   if action == 'double_click': actions.doAction(i)
   done=True; break
 if not done:
  print(json.dumps({'semantic':False},separators=(',',':'))); raise SystemExit(0)
elif action == 'set_value':
 editable=e.queryEditableText(); editable.setTextContents(os.environ['LM_VALUE'])
else: raise SystemExit('unsupported semantic action')
print(json.dumps({'semantic':True},separators=(',',':')))
"#;

fn element_index(element_id: &str) -> Result<usize, String> {
    element_id
        .rsplit_once("::element-")
        .and_then(|(_, index)| index.split("::").next())
        .and_then(|index| index.parse::<usize>().ok())
        .ok_or_else(|| "Accessibility element id has no stable provider index".to_string())
}

#[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
fn element_native_stable(element_id: &str) -> Option<&str> {
    element_id
        .rsplit_once("::native-")
        .map(|(_, stable)| stable)
}

fn window_index(window_id: &str) -> Result<usize, String> {
    window_id
        .rsplit_once("::window-")
        .and_then(|(_, index)| index.parse::<usize>().ok())
        .ok_or_else(|| "Accessibility window id has no stable provider index".to_string())
}

fn native_semantic_action(
    target: &ComputerTarget,
    element_id: &str,
    action: &str,
    value: Option<&str>,
) -> Result<bool, String> {
    let element_index = element_index(element_id)?;
    #[cfg(target_os = "macos")]
    {
        let window_index = window_index(&target.window_id)?;
        let bytes = run_native_command_with_env(
            "osascript",
            &["-l", "JavaScript", "-e", MACOS_AX_ACTION_SCRIPT],
            &[
                ("LM_APP_ID", target.application_id.clone()),
                ("LM_WINDOW_INDEX", window_index.to_string()),
                ("LM_ELEMENT_INDEX", element_index.to_string()),
                (
                    "LM_ELEMENT_STABLE",
                    element_native_stable(element_id)
                        .unwrap_or_default()
                        .to_string(),
                ),
                (
                    "LM_ELEMENT_STABLE",
                    element_native_stable(element_id)
                        .unwrap_or_default()
                        .to_string(),
                ),
                ("LM_ACTION", action.to_string()),
                ("LM_VALUE", value.unwrap_or_default().to_string()),
            ],
        )?;
        return serde_json::from_slice::<serde_json::Value>(&bytes)
            .ok()
            .and_then(|json| json.get("semantic").and_then(serde_json::Value::as_bool))
            .ok_or_else(|| "macOS Accessibility action returned invalid data".to_string());
    }
    #[cfg(target_os = "windows")]
    {
        let bytes = run_native_command_with_env(
            "powershell.exe",
            &[
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                WINDOWS_UIA_ACTION_SCRIPT,
            ],
            &[
                ("LM_WINDOW_HANDLE", target.window_id.clone()),
                ("LM_ELEMENT_INDEX", element_index.to_string()),
                (
                    "LM_ELEMENT_STABLE",
                    element_native_stable(element_id)
                        .unwrap_or_default()
                        .to_string(),
                ),
                ("LM_ACTION", action.to_string()),
                ("LM_VALUE", value.unwrap_or_default().to_string()),
            ],
        )?;
        return serde_json::from_slice::<serde_json::Value>(&bytes)
            .ok()
            .and_then(|json| json.get("semantic").and_then(serde_json::Value::as_bool))
            .ok_or_else(|| "Windows UI Automation action returned invalid data".to_string());
    }
    #[cfg(target_os = "linux")]
    {
        if is_wayland_session_from_env() {
            return Err(WAYLAND_PORTAL_MESSAGE.to_string());
        }
        let provider_window_id = target
            .provider_window_id
            .as_deref()
            .unwrap_or(&target.window_id);
        let window_index = window_index(provider_window_id)?;
        let bytes = run_native_command_with_env(
            "python3",
            &["-c", LINUX_ATSPI_ACTION_SCRIPT],
            &[
                ("LM_APP_NAME", target.application_name.clone()),
                ("LM_WINDOW_INDEX", window_index.to_string()),
                ("LM_ELEMENT_INDEX", element_index.to_string()),
                ("LM_ACTION", action.to_string()),
                ("LM_VALUE", value.unwrap_or_default().to_string()),
            ],
        )?;
        return serde_json::from_slice::<serde_json::Value>(&bytes)
            .ok()
            .and_then(|json| json.get("semantic").and_then(serde_json::Value::as_bool))
            .ok_or_else(|| "Linux AT-SPI action returned invalid data".to_string());
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
    {
        let _ = (target, element_id, action, value, element_index);
        Ok(false)
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
    pub allowed_windows: Vec<String>,
    pub created_at_ms: u64,
    pub expires_at_ms: u64,
    pub active: bool,
    pub paused: bool,
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
    pub approval_policy: ApprovalPolicy,
    pub allow_screenshots: bool,
    pub allow_keyboard_input: bool,
    pub allow_clipboard_read: bool,
}

#[derive(Debug, Default)]
pub struct SessionGrantOptions {
    pub allowed_windows: Vec<String>,
    pub allow_screenshots: bool,
    pub allow_keyboard_input: bool,
    pub allow_clipboard_read: bool,
    pub approval_policy: Option<ApprovalPolicy>,
    pub budget: Option<ComputerUseRunBudget>,
}

impl SessionGrantOptions {
    fn for_legacy(approved_batch: bool) -> Self {
        Self {
            allow_screenshots: true,
            allow_keyboard_input: true,
            approval_policy: Some(if approved_batch {
                ApprovalPolicy::ApprovedBatch
            } else {
                ApprovalPolicy::PerAction
            }),
            ..Self::default()
        }
    }
}

/// An in-flight approval request for one [`ControlAction`], keyed by a
/// generated id in [`DesktopControlState::pending`]. Not `Clone`/`Serialize`
/// — the `oneshot::Sender` can't be, and nothing outside this module needs
/// the whole struct; [`PendingActionSummary`] is the serializable view sent
/// to the frontend.
#[derive(Clone, Debug, Default)]
struct AuditContext {
    run_id: Option<String>,
    tool_call_id: Option<String>,
}

struct PendingControlAction {
    session_id: String,
    target_application_id: String,
    target_window_id: Option<String>,
    action: ControlAction,
    context: AuditContext,
    approval_level: ApprovalLevel,
    approval_digest: String,
    description: String,
    sender: Option<oneshot::Sender<bool>>,
}

/// Serializable snapshot of a pending action, emitted to the frontend so it
/// can render an approve/deny prompt.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PendingActionSummary {
    pub action_id: String,
    pub session_id: String,
    pub target_application_id: String,
    pub target_window_id: Option<String>,
    pub approval_level: ApprovalLevel,
    pub description: String,
    pub action: ControlAction,
}

#[derive(Clone, Debug, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct VerificationEvidence {
    pub kind: String,
    pub element_id: Option<String>,
    pub expected_value: Option<String>,
    pub observed_value: Option<String>,
    pub matched: bool,
    pub detail: String,
}

impl VerificationEvidence {
    fn redacted_for_audit(&self) -> Self {
        let mut redacted = self.clone();
        if self.kind == "element_value" {
            redacted.expected_value = None;
            redacted.observed_value = None;
            redacted.detail = format!("{}; values redacted from durable audit", self.detail);
        }
        redacted
    }
}

/// Result of a resolved (executed or denied) action, returned to the caller
/// of `desktop_control_request_action`.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ActionOutcome {
    pub action_id: String,
    pub executed: bool,
    pub input_sent: bool,
    pub state_verified: bool,
    pub verification: Option<String>,
    pub verification_evidence: Option<VerificationEvidence>,
    pub audit_id: String,
    pub approval_level: ApprovalLevel,
}

impl ActionOutcome {
    pub fn from_execution(action_id: String, result: ExecutionResult) -> Self {
        Self {
            action_id,
            executed: true,
            input_sent: result.input_sent,
            state_verified: result.state_verified,
            verification: result.verification,
            verification_evidence: result.verification_evidence,
            audit_id: result.audit_id,
            approval_level: result.approval_level,
        }
    }
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ComputerAuditRecord {
    pub audit_id: String,
    pub run_id: Option<String>,
    pub tool_call_id: Option<String>,
    pub session_id: String,
    pub target_application_id: String,
    pub target_window_id: Option<String>,
    pub action: String,
    pub approval_level: ApprovalLevel,
    pub result: String,
    pub approval: String,
    pub input_sent: bool,
    pub state_verified: bool,
    pub verification: Option<String>,
    pub verification_evidence: Option<VerificationEvidence>,
    pub screenshot_ref: Option<String>,
    pub created_at_ms: u64,
}

/// Outcome of [`DesktopControlState::begin_action`]'s validation step —
/// factored out from the async command so it's directly testable without an
/// `AppHandle`/oneshot-await machinery (mirrors `permissions.rs`'s
/// `mode_short_circuit` being a pure, directly-testable decision function).
pub enum ActionGate {
    /// The session is in "approved batch" mode: the action already ran (or
    /// failed) against the backend, no approval needed.
    Executed(Result<ExecutionResult, String>),
    /// The session requires per-action approval: the caller must await
    /// `receiver`, then dispatch to the backend itself on `Ok(Ok(true))`.
    Pending {
        action_id: String,
        receiver: oneshot::Receiver<bool>,
    },
}

pub struct ExecutionResult {
    input_sent: bool,
    state_verified: bool,
    verification: Option<String>,
    verification_evidence: Option<VerificationEvidence>,
    audit_id: String,
    approval_level: ApprovalLevel,
}

fn validate_coordinates(target: &ComputerTarget, x: i32, y: i32) -> Result<(), String> {
    if !target.bounds.x.is_finite()
        || !target.bounds.y.is_finite()
        || !target.bounds.width.is_finite()
        || !target.bounds.height.is_finite()
        || target.bounds.width <= 0.0
        || target.bounds.height <= 0.0
    {
        return Err("Target has no valid bounded coordinate region".to_string());
    }
    let inside_x =
        f64::from(x) >= target.bounds.x && f64::from(x) <= target.bounds.x + target.bounds.width;
    let inside_y =
        f64::from(y) >= target.bounds.y && f64::from(y) <= target.bounds.y + target.bounds.height;
    if inside_x && inside_y {
        Ok(())
    } else {
        Err("Coordinate is outside the verified target bounds".to_string())
    }
}

fn action_summary(action: &ControlAction) -> String {
    match action {
        ControlAction::TypeText { .. } => "type_text (content redacted)".to_string(),
        ControlAction::SetValue { .. } | ControlAction::Select { .. } => {
            "set_value (content redacted)".to_string()
        }
        ControlAction::SemanticClick {
            element_id,
            button,
            expected_value,
        } => format!(
            "semantic_click element={element_id} button={button:?} expected_value={}",
            if expected_value.is_some() {
                "(redacted)"
            } else {
                "unspecified"
            }
        ),
        ControlAction::SemanticDoubleClick {
            element_id,
            button,
            expected_value,
        } => format!(
            "semantic_double_click element={element_id} button={button:?} expected_value={}",
            if expected_value.is_some() {
                "(redacted)"
            } else {
                "unspecified"
