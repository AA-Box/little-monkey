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
    if require_frontmost && !target.focused {
        return Err("Target is not frontmost; focus it and retry".to_string());
    }
    Ok(target)
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
    const front = Boolean(safe(() => p.frontmost(), false)); let wi = 0;
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
const WINDOWS_UIA_SCRIPT: &str = r#"
Add-Type -AssemblyName UIAutomationClient
Add-Type -AssemblyName UIAutomationTypes
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
    $windows=@($root.FindAll([System.Windows.Automation.TreeScope]::Children,[System.Windows.Automation.Condition]::TrueCondition) | Where-Object {$_.Current.Name -eq 'Little Monkey TestApp'})
    $fixtureFallback=$windows.Count -gt 0
  }
}
function ValueOf($e) { try { return $e.GetCurrentPattern([System.Windows.Automation.ValuePattern]::Pattern).Current.Value } catch { try { return [string]$e.GetCurrentPattern([System.Windows.Automation.TogglePattern]::Pattern).Current.ToggleState } catch { return $null } } }
function ActionsOf($e) { $a=@(); try { $e.GetCurrentPattern([System.Windows.Automation.InvokePattern]::Pattern) | Out-Null; $a+='click'; $a+='double_click' } catch {}; try { $e.GetCurrentPattern([System.Windows.Automation.TogglePattern]::Pattern) | Out-Null; $a+='click' } catch {}; try { $e.GetCurrentPattern([System.Windows.Automation.ValuePattern]::Pattern) | Out-Null; $a+='set_value' } catch {}; try { $e.GetCurrentPattern([System.Windows.Automation.SelectionItemPattern]::Pattern) | Out-Null; $a+='select' } catch {}; if(([string]$e.Current.ControlType.ProgrammaticName -match 'Edit') -and -not ($a -contains 'set_value')){$a+='set_value'}; return @($a | Select-Object -Unique) }
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
def walk(node):
 for child in list(node):
  yield child
  yield from walk(child)
targets=[]; elements={}; desktop=pyatspi.Registry.getDesktop(0)
for app in list(desktop)[:64]:
 name=str(getattr(app,'name','')); aid='atspi:'+name
 for wi,w in enumerate(list(app)[:32]):
  title=str(getattr(w,'name','')); tid=aid+'::window-'+str(wi); st=w.getState(); target={'targetId':tid,'applicationId':aid,'applicationName':name,'windowId':tid,'windowTitle':title,'bounds':rect(w),'focused':bool(st.contains(pyatspi.STATE_ACTIVE)),'sensitive':False,'supportedActions':['inspect','focus','click','double_click','scroll','type','key','hotkey','screenshot']};targets.append(target); out=[]
  for ei,e in enumerate(list(walk(w))[:256]):
   role=str(e.getRoleName()); label=str(getattr(e,'name','')); value=None
   try: value=str(e.queryValue().getCurrentValue())
   except Exception: pass
   actions=[]
   try:
    qa=e.queryAction()
    for ai in range(qa.nActions):
     name=(qa.getActionName(ai) or '').lower()
     if name in ('click','press','activate','select'): actions.append(name)
   except Exception: pass
   try: e.queryEditableText(); actions.append('set_value')
   except Exception: pass
   stable=(role+'-'+label).replace(' ','_')
   out.append({'id':tid+'::element-'+str(ei)+'::native-'+stable[:80],'role':role,'label':label,'value':value,'bounds':rect(e),'enabled':True,'focused':False,'actions':list(dict.fromkeys(actions)),'sensitive':any(token in (role+' '+label).lower() for token in ('password','secure','credential','authentication'))})
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
const action = get('LM_ACTION');
const value = get('LM_VALUE');
const process = /^(com|org|net|io)\./.test(appId) ? se.processes.byBundleIdentifier(appId) : se.processes.byName(appId);
const window = process.windows[windowIndex];
const element = window.entireContents()[elementIndex];
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
const se = Application('System Events');
const env = $.NSProcessInfo.processInfo.environment;
const get = key => ObjC.unwrap(env.objectForKey(key));
const appId = get('LM_APP_ID');
const process = /^(com|org|net|io)\./.test(appId) ? se.processes.byBundleIdentifier(appId) : se.processes.byName(appId);
process.frontmost = true;
JSON.stringify({focused:Boolean(process.frontmost())});
"#;

#[cfg(target_os = "windows")]
const WINDOWS_UIA_ACTION_SCRIPT: &str = r#"
Add-Type -AssemblyName UIAutomationClient
Add-Type -AssemblyName UIAutomationTypes
$root=[System.Windows.Automation.AutomationElement]::FromHandle([IntPtr]::new([int64]$env:LM_WINDOW_HANDLE))
$desc=$root.FindAll([System.Windows.Automation.TreeScope]::Descendants,[System.Windows.Automation.Condition]::TrueCondition)
$e=$null
$stable=$env:LM_ELEMENT_STABLE
if(-not [string]::IsNullOrWhiteSpace($stable)) {
  for($i=0;$i -lt $desc.Count;$i++) {
    $candidate=$desc.Item($i)
    $automation=[string]$candidate.Current.AutomationId
    if([string]::IsNullOrWhiteSpace($automation)){try{$automation=($candidate.GetRuntimeId() -join '-')}catch{$automation=''}}
    $candidateStable=($automation -replace '[^A-Za-z0-9._-]','_')
    if($candidateStable -eq $stable){$e=$candidate;break}
  }
}
if($null -eq $e){$e=$desc.Item([int]$env:LM_ELEMENT_INDEX)}
$action=$env:LM_ACTION
$performed=$false
if($action -eq 'set_value') {
  try { $p=$e.GetCurrentPattern([System.Windows.Automation.ValuePattern]::Pattern); $p.SetValue($env:LM_VALUE); $performed=$true } catch {}
} elseif($action -eq 'select') {
  try { $p=$e.GetCurrentPattern([System.Windows.Automation.SelectionItemPattern]::Pattern); $p.Select(); $performed=$true } catch {}
} elseif($action -eq 'click' -or $action -eq 'double_click') {
  $toggleSupported=$false
  try { $p=$e.GetCurrentPattern([System.Windows.Automation.TogglePattern]::Pattern); $toggleSupported=$true; $p.Toggle(); $performed=$true } catch {}
  if(-not $performed -and -not $toggleSupported) { try { $p=$e.GetCurrentPattern([System.Windows.Automation.InvokePattern]::Pattern); $p.Invoke(); $performed=$true } catch {} }
  if($performed -and $action -eq 'double_click') { try { if($toggleSupported) { $p.Toggle() } else { $p.Invoke() } } catch {} }
}
if($performed) { [ordered]@{semantic=$true}|ConvertTo-Json -Compress } else { [ordered]@{semantic=$false}|ConvertTo-Json -Compress }
"#;

#[cfg(target_os = "linux")]
const LINUX_ATSPI_ACTION_SCRIPT: &str = r#"
import os, json
import pyatspi
def walk(node):
 for child in list(node):
  yield child
  yield from walk(child)
app_name=os.environ['LM_APP_NAME']; wi=int(os.environ['LM_WINDOW_INDEX']); ei=int(os.environ['LM_ELEMENT_INDEX'])
a=None
for candidate in list(pyatspi.Registry.getDesktop(0)):
 if str(getattr(candidate,'name','')) == app_name: a=candidate; break
if a is None: raise SystemExit('AT-SPI application is stale')
w=list(a)[wi]; e=list(walk(w))[ei]; action=os.environ['LM_ACTION']
if action in ('click','double_click','select'):
 actions=e.queryAction(); done=False
 for i in range(actions.nActions):
  name=(actions.getActionName(i) or '').lower()
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

#[cfg(target_os = "windows")]
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

#[derive(Clone, Debug, Default)]
pub struct SessionGrantOptions {
    pub allowed_windows: Vec<String>,
    pub allow_screenshots: bool,
    pub allow_keyboard_input: bool,
    pub allow_clipboard_read: bool,
    pub approval_policy: Option<ApprovalPolicy>,
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
            }
        ),
        _ => serde_json::to_string(action).unwrap_or_else(|_| "unserializable_action".to_string()),
    }
}

fn approval_digest(
    session_id: &str,
    target_application_id: &str,
    target_window_id: Option<&str>,
    action: &ControlAction,
    approval: ApprovalLevel,
    description: &str,
) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"little-monkey-computer-approval-v1\0");
    let action_json = serde_json::to_string(action).unwrap_or_default();
    for value in [
        session_id,
        target_application_id,
        target_window_id.unwrap_or(""),
        &format!("{approval:?}"),
        description,
        &action_json,
    ] {
        hasher.update((value.len() as u64).to_le_bytes());
        hasher.update(value.as_bytes());
    }
    format!("{:x}", hasher.finalize())
}

fn approval_level(action: &ControlAction) -> ApprovalLevel {
    approval_level_for_name(&action_summary(action))
}

fn approval_level_for_name(action_name: &str) -> ApprovalLevel {
    let summary = action_name.to_ascii_lowercase();
    if [
        "delete", "destroy", "remove", "purchase", "payment", "confirm", "send", "submit",
        "publish", "revoke", "shutdown", "format", "erase",
    ]
    .iter()
    .any(|token| summary.contains(token))
    {
        return ApprovalLevel::Critical;
    }
    if summary.contains("screenshot")
        || summary.contains("inspect")
        || summary.contains("clipboard")
    {
        ApprovalLevel::Low
    } else if summary.contains("focus") || summary.contains("scroll") {
        ApprovalLevel::Medium
    } else {
        ApprovalLevel::High
    }
}

fn redacted_action_for_ui(action: &ControlAction) -> ControlAction {
    match action {
        ControlAction::TypeText { text } => ControlAction::TypeText {
            text: format!("[redacted typed text: {} characters]", text.chars().count()),
        },
        ControlAction::SetValue { element_id, value } => ControlAction::SetValue {
            element_id: element_id.clone(),
            value: format!("[redacted value: {} characters]", value.chars().count()),
        },
        ControlAction::Select { element_id, value } => ControlAction::Select {
            element_id: element_id.clone(),
            value: format!("[redacted value: {} characters]", value.chars().count()),
        },
        other => other.clone(),
    }
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
    semantic: Arc<dyn DesktopSemanticBackend>,
    sessions: Mutex<BTreeMap<String, ControlSession>>,
    pending: Mutex<HashMap<String, PendingControlAction>>,
    audit: Mutex<Vec<ComputerAuditRecord>>,
    /// Path of the machine-wide exclusive lock this state must hold while any
    /// session is active, or `None` to disable cross-process locking (the
    /// shape every in-module test and any pure in-process caller uses).
    lock_path: Option<PathBuf>,
    /// The currently-held lock guard, if this state owns an active session.
    held_lock: Mutex<Option<DesktopControlLockGuard>>,
}

impl DesktopControlState {
    pub fn production() -> Self {
        let backend = production_backend();
        Self::with_backends_and_lock(backend.clone(), production_semantic_backend(backend), None)
    }

    /// Production backend plus the machine-wide exclusive lock at
    /// `<app_data>/desktop_control.lock`, so the local Tauri app and the
    /// resident daemon can never drive real OS input at the same time even
    /// though each constructs its own `DesktopControlState`.
    pub fn production_with_lock(lock_path: PathBuf) -> Self {
        let backend = production_backend();
        Self::with_backends_and_lock(
            backend.clone(),
            production_semantic_backend(backend),
            Some(lock_path),
        )
    }

    pub fn with_backend(backend: Arc<dyn DesktopInputBackend>) -> Self {
        Self::with_backend_and_lock(backend, None)
    }

    pub fn with_backend_and_lock(
        backend: Arc<dyn DesktopInputBackend>,
        lock_path: Option<PathBuf>,
    ) -> Self {
        Self::with_backends_and_lock(backend, Arc::new(NullSemanticBackend), lock_path)
    }

    pub fn with_backends_and_lock(
        backend: Arc<dyn DesktopInputBackend>,
        semantic: Arc<dyn DesktopSemanticBackend>,
        lock_path: Option<PathBuf>,
    ) -> Self {
        Self {
            backend,
            semantic,
            sessions: Mutex::new(BTreeMap::new()),
            pending: Mutex::new(HashMap::new()),
            audit: Mutex::new(Vec::new()),
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
        self.start_session_with_options(
            permission_mode,
            allowed_applications,
            lifetime_ms,
            SessionGrantOptions::for_legacy(approved_batch),
        )
    }

    pub fn start_session_with_options(
        &self,
        permission_mode: &str,
        allowed_applications: Vec<String>,
        lifetime_ms: u64,
        options: SessionGrantOptions,
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
        if options.allowed_windows.len() > 64 {
            return Err(
                "Safe Desktop Control window allowlist is limited to 64 entries".to_string(),
            );
        }
        for window_id in &options.allowed_windows {
            validate_application_id(window_id)?;
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
        let approval_policy = options.approval_policy.unwrap_or(ApprovalPolicy::PerAction);
        let session = ControlSession {
            session_id: format!("desktop-control-{}", Uuid::new_v4()),
            allowed_applications,
            allowed_windows: options.allowed_windows,
            created_at_ms,
            expires_at_ms: created_at_ms.saturating_add(lifetime_ms),
            active: true,
            paused: false,
            indicator_visible: true,
            approved_batch: matches!(approval_policy, ApprovalPolicy::ApprovedBatch),
            approval_policy,
            allow_screenshots: options.allow_screenshots,
            allow_keyboard_input: options.allow_keyboard_input,
            allow_clipboard_read: options.allow_clipboard_read,
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

    pub fn pause_session(&self, session_id: &str, paused: bool) -> Result<bool, String> {
        let changed = lock(&self.sessions, "control sessions")?
            .get_mut(session_id)
            .map(|session| {
                let changed = session.active && session.paused != paused;
                if changed {
                    session.paused = paused;
                }
                changed
            })
            .unwrap_or(false);
        if paused {
            self.deny_pending_for_session(session_id)?;
        }
        Ok(changed)
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

    fn active_session(&self, session_id: &str) -> Result<ControlSession, String> {
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
        if session.paused {
            return Err("Control session is paused".to_string());
        }
        Ok(session.clone())
    }

    pub fn list_targets_for_session(
        &self,
        session_id: &str,
    ) -> Result<Vec<ComputerTarget>, String> {
        let session = self.active_session(session_id)?;
        let mut targets = self.semantic.list_targets()?;
        targets.retain(|target| {
            !target_is_sensitive(target)
                && session.allowed_applications.iter().any(|allowed| {
                    allowed == &target.application_id
                        || allowed == &target.application_name
                        || allowed == &target.target_id
                })
                && (session.allowed_windows.is_empty()
                    || session.allowed_windows.iter().any(|allowed| {
                        allowed == &target.window_id || allowed == &target.target_id
                    }))
        });
        targets.truncate(MAX_TARGETS);
        Ok(targets)
    }

    pub fn inspect_for_session(
        &self,
        session_id: &str,
        target_application_id: &str,
        target_window_id: Option<&str>,
        query: Option<&str>,
    ) -> Result<ComputerInspection, String> {
        let _ = self.require_active_session_for_target(
            session_id,
            target_application_id,
            target_window_id,
            false,
        )?;
        self.semantic
            .inspect(target_application_id, target_window_id, query)
    }

    pub fn screenshot_for_session(
        &self,
        session_id: &str,
        target_application_id: &str,
        target_window_id: Option<&str>,
        bounds: Option<ComputerBounds>,
    ) -> Result<(ComputerTarget, Vec<u8>, ComputerBounds), String> {
        let session = self.active_session(session_id)?;
        if !session.allow_screenshots {
            return Err("This session grant does not allow screenshots".to_string());
        }
        let (_, target) = self.require_active_session_for_target(
            session_id,
            target_application_id,
            target_window_id,
            false,
        )?;
        let (bytes, captured_bounds) = self.semantic.screenshot(&target, bounds)?;
        Ok((target, bytes, captured_bounds))
    }

    fn clipboard_for_session(
        &self,
        session_id: &str,
        target_application_id: &str,
        target_window_id: Option<&str>,
        context: &AuditContext,
    ) -> Result<(String, String), String> {
        let session = self.active_session(session_id)?;
        if !session.allow_clipboard_read {
            return Err("This session grant does not allow clipboard reads".to_string());
        }
        let _ = self.require_active_session_for_target(
            session_id,
            target_application_id,
            target_window_id,
            false,
        )?;
        let text = read_clipboard_native()?;
        let audit_id = self.record_named_audit_with_context(
            session_id,
            target_application_id,
            target_window_id,
            "clipboard_read (content redacted)".to_string(),
            "executed",
            "granted",
            false,
            true,
            Some("clipboard content returned to the model; content omitted from audit".to_string()),
            context,
        )?;
        Ok((text, audit_id))
    }

    /// Reads clipboard content only for a session that explicitly granted the
    /// clipboard capability. Remote callers use this wrapper so they share
    /// the same target validation and redacted audit record as local callers.
    pub fn clipboard_for_remote(
        &self,
        session_id: &str,
        target_application_id: &str,
        target_window_id: Option<&str>,
    ) -> Result<(String, String), String> {
        self.clipboard_for_session(
            session_id,
            target_application_id,
            target_window_id,
            &AuditContext::default(),
        )
    }

    fn require_active_session_for_target(
        &self,
        session_id: &str,
        target_application_id: &str,
        target_window_id: Option<&str>,
        require_frontmost: bool,
    ) -> Result<(ControlSession, ComputerTarget), String> {
        validate_application_id(target_application_id)?;
        if let Some(window_id) = target_window_id {
            validate_application_id(window_id)?;
        }
        if sensitive_text(target_application_id) || target_window_id.is_some_and(sensitive_text) {
            return Err("Sensitive application/window targets are blocked".to_string());
        }
        let now = now_ms();
        let session = {
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
            if session.paused {
                return Err("Control session is paused".to_string());
            }
            session.clone()
        };
        let target = self.semantic.verify_target(
            target_application_id,
            target_window_id,
            require_frontmost,
        )?;
        if target_is_sensitive(&target) {
            return Err("Sensitive application/window targets are blocked".to_string());
        }
        let app_allowed = session.allowed_applications.iter().any(|allowed| {
            allowed == target_application_id
                || allowed == &target.application_id
                || allowed == &target.application_name
                || allowed == &target.target_id
        });
        let window_allowed = session.allowed_windows.is_empty()
            || session
                .allowed_windows
                .iter()
                .any(|allowed| allowed == &target.window_id || allowed == &target.target_id);
        if !app_allowed || !window_allowed {
            return Err(
                "Target application/window is outside this session's allowlist".to_string(),
            );
        }
        Ok((session, target))
    }

    fn action_requires_frontmost(action: &ControlAction) -> bool {
        !matches!(action, ControlAction::Focus | ControlAction::Wait { .. })
    }

    fn action_targets_sensitive_focus(
        &self,
        target_application_id: &str,
        target_window_id: Option<&str>,
        action: &ControlAction,
    ) -> Result<bool, String> {
        if !matches!(
            action,
            ControlAction::TypeText { .. }
                | ControlAction::KeyPress { .. }
                | ControlAction::Hotkey { .. }
        ) {
            return Ok(false);
        }
        Ok(self
            .semantic
            .inspect(target_application_id, target_window_id, None)?
            .elements
            .iter()
            .any(|element| element.focused && element.sensitive))
    }

    fn action_approval(
        &self,
        target_application_id: &str,
        target_window_id: Option<&str>,
        action: &ControlAction,
    ) -> (ApprovalLevel, String) {
        let mut description = action_summary(action);
        let element_id = match action {
            ControlAction::SemanticClick { element_id, .. }
            | ControlAction::SemanticDoubleClick { element_id, .. }
            | ControlAction::Select { element_id, .. }
            | ControlAction::SetValue { element_id, .. } => Some(element_id.as_str()),
            _ => None,
        };
        let mut element_unverified = false;
        if let Some(element_id) = element_id {
            if let Ok(inspection) =
                self.semantic
                    .inspect(target_application_id, target_window_id, None)
            {
                if let Some(element) = inspection
                    .elements
                    .iter()
                    .find(|element| element.id == element_id)
                {
                    description.push_str(&format!(
                        " role={} label={}",
                        element.role,
                        if element.label.is_empty() {
                            "(unlabelled)"
                        } else {
                            &element.label
                        }
                    ));
                } else {
                    element_unverified = true;
                }
            } else {
                element_unverified = true;
            }
        }
        let level = if element_id.is_some() {
            if element_unverified {
                ApprovalLevel::Critical
            } else {
                approval_level_for_name(&description)
            }
        } else {
            approval_level(action)
        };
        (level, description)
    }

    fn verify_postcondition(
        &self,
        target_application_id: &str,
        target_window_id: Option<&str>,
        action: &ControlAction,
        before_value: Option<String>,
    ) -> (bool, Option<VerificationEvidence>, Option<String>) {
        let element_id = match action {
            ControlAction::SemanticClick { element_id, .. }
            | ControlAction::SemanticDoubleClick { element_id, .. }
            | ControlAction::Select { element_id, .. }
            | ControlAction::SetValue { element_id, .. } => Some(element_id.as_str()),
            _ => None,
        };
        if matches!(action, ControlAction::Focus) {
            return match self
                .semantic
                .verify_target(target_application_id, target_window_id, true)
            {
                Ok(target) if target.focused => (
                    true,
                    Some(VerificationEvidence {
                        kind: "target_focus".to_string(),
                        element_id: None,
                        expected_value: None,
                        observed_value: Some("focused".to_string()),
                        matched: true,
                        detail: "the requested target is focused after the action".to_string(),
                    }),
                    Some("target focus verified after action".to_string()),
                ),
                Ok(_) => (
                    false,
                    Some(VerificationEvidence {
                        kind: "target_focus".to_string(),
                        element_id: None,
                        expected_value: Some("focused".to_string()),
                        observed_value: Some("not_focused".to_string()),
                        matched: false,
                        detail: "the target remained reachable but is not focused".to_string(),
                    }),
                    Some("target remained reachable but focus was not verified".to_string()),
                ),
                Err(error) => (
                    false,
                    Some(VerificationEvidence {
                        kind: "target_focus".to_string(),
                        element_id: None,
                        expected_value: Some("focused".to_string()),
                        observed_value: None,
                        matched: false,
                        detail: error.clone(),
                    }),
                    Some(format!("target focus could not be verified: {error}")),
                ),
            };
        }
        if let Some(element_id) = element_id {
            let expected = match action {
                ControlAction::SemanticClick { expected_value, .. }
                | ControlAction::SemanticDoubleClick { expected_value, .. } => {
                    expected_value.clone()
                }
                ControlAction::Select { value, .. } | ControlAction::SetValue { value, .. } => {
                    Some(value.clone())
                }
                _ => None,
            };
            let inspected = self
                .semantic
                .inspect(target_application_id, target_window_id, None)
                .ok()
                .and_then(|inspection| {
                    inspection
                        .elements
                        .into_iter()
                        .find(|element| element.id == element_id)
                });
            let observed = inspected.as_ref().and_then(|element| element.value.clone());
            let matched = if let Some(expected) = expected.as_deref() {
                observed
                    .as_deref()
                    .is_some_and(|value| value.trim() == expected.trim())
            } else {
                before_value
                    .as_deref()
                    .zip(observed.as_deref())
                    .is_some_and(|(before, after)| before != after)
            };
            let detail = if expected.is_some() {
                "the inspected element value was compared with the requested postcondition"
            } else {
                "the inspected element value was compared before and after the semantic action"
            };
            return (
                matched,
                Some(VerificationEvidence {
                    kind: "element_value".to_string(),
                    element_id: Some(element_id.to_string()),
                    expected_value: expected,
                    observed_value: observed,
                    matched,
                    detail: detail.to_string(),
                }),
                Some(if matched {
                    "element state verified after action".to_string()
                } else {
                    "input was sent; the requested element state was not verified".to_string()
                }),
            );
        }
        let detail = if matches!(action, ControlAction::Wait { .. }) {
            "the target was revalidated after the wait"
        } else {
            "input delivery was confirmed, but no element postcondition was supplied"
        };
        let verified = matches!(action, ControlAction::Wait { .. })
            && self
                .semantic
                .verify_target(target_application_id, target_window_id, false)
                .is_ok();
        (
            verified,
            Some(VerificationEvidence {
                kind: "target_revalidation".to_string(),
                element_id: None,
                expected_value: None,
                observed_value: None,
                matched: verified,
                detail: detail.to_string(),
            }),
            Some(detail.to_string()),
        )
    }

    fn set_audit_verification_evidence(
        &self,
        audit_id: &str,
        evidence: Option<VerificationEvidence>,
    ) -> Result<(), String> {
        if let Some(record) = lock(&self.audit, "desktop control audit")?
            .iter_mut()
            .find(|record| record.audit_id == audit_id)
        {
            record.verification_evidence = evidence.map(|value| value.redacted_for_audit());
        }
        Ok(())
    }

    fn validate_action(
        session: &ControlSession,
        target: &ComputerTarget,
        action: &ControlAction,
    ) -> Result<(), String> {
        let keyboard_action = matches!(
            action,
            ControlAction::TypeText { .. }
                | ControlAction::KeyPress { .. }
                | ControlAction::Hotkey { .. }
                | ControlAction::Select { .. }
                | ControlAction::SetValue { .. }
        );
        if keyboard_action && !session.allow_keyboard_input {
            return Err("This session grant does not allow keyboard input".to_string());
        }
        match action {
            ControlAction::TypeText { text } if text.len() > 16 * 1024 => {
                Err("Typed text exceeds the 16 KiB action bound".to_string())
            }
            ControlAction::Hotkey { keys } if keys.is_empty() || keys.len() > 8 => {
                Err("Hotkeys must contain between 1 and 8 named keys".to_string())
            }
            ControlAction::Hotkey { keys }
                if keys.iter().any(|key| key.is_empty() || key.len() > 64) =>
            {
                Err("Hotkey names are bounded printable strings".to_string())
            }
            ControlAction::Scroll { delta_x, delta_y }
                if delta_x.unsigned_abs() > 10_000 || delta_y.unsigned_abs() > 10_000 =>
            {
                Err("Scroll deltas exceed the bounded action limit".to_string())
            }
            ControlAction::Wait { milliseconds } if *milliseconds > 10_000 => {
                Err("Wait is limited to 10 seconds".to_string())
            }
            ControlAction::MouseClickAt { x, y, .. }
            | ControlAction::MouseDoubleClickAt { x, y, .. } => {
                validate_coordinates(target, *x, *y)
            }
            ControlAction::MouseDrag {
                from_x,
                from_y,
                to_x,
                to_y,
            } => {
                validate_coordinates(target, *from_x, *from_y)?;
                validate_coordinates(target, *to_x, *to_y)
            }
            ControlAction::SemanticClick { element_id, .. }
            | ControlAction::SemanticDoubleClick { element_id, .. }
            | ControlAction::Select { element_id, .. }
            | ControlAction::SetValue { element_id, .. }
                if element_id.len() > 512 || element_id.chars().any(char::is_control) =>
            {
                Err("Accessibility element id is invalid or too long".to_string())
            }
            _ => Ok(()),
        }
    }

    fn execute_for_target_with_context(
        &self,
        session_id: &str,
        target_application_id: &str,
        target_window_id: Option<&str>,
        action: &ControlAction,
        context: &AuditContext,
    ) -> Result<ExecutionResult, String> {
        let (session, target) = self.require_active_session_for_target(
            session_id,
            target_application_id,
            target_window_id,
            Self::action_requires_frontmost(action),
        )?;
        Self::validate_action(&session, &target, action)?;
        if self.action_targets_sensitive_focus(target_application_id, target_window_id, action)? {
            return Err(
                "Keyboard input into a sensitive or authentication element is blocked".to_string(),
            );
        }
        let before_value = match action {
            ControlAction::SemanticClick { element_id, .. }
            | ControlAction::SemanticDoubleClick { element_id, .. }
            | ControlAction::Select { element_id, .. }
            | ControlAction::SetValue { element_id, .. } => self
                .semantic
                .inspect(target_application_id, target_window_id, None)
                .ok()
                .and_then(|inspection| {
                    inspection
                        .elements
                        .into_iter()
                        .find(|element| element.id == *element_id)
                        .and_then(|element| element.value)
                }),
            _ => None,
        };
        let mut input_sent = false;
        match action {
            ControlAction::MouseMove { x, y } => {
                validate_coordinates(&target, *x, *y)?;
                self.backend.move_mouse(*x, *y)?;
                input_sent = true;
            }
            ControlAction::MouseClick { button } => {
                self.backend.click(*button)?;
                input_sent = true;
            }
            ControlAction::MouseClickAt { x, y, button } => {
                self.backend.move_mouse(*x, *y)?;
                self.backend.click(*button)?;
                input_sent = true;
            }
            ControlAction::MouseDoubleClick { button } => {
                self.backend.double_click(*button)?;
                input_sent = true;
            }
            ControlAction::MouseDoubleClickAt { x, y, button } => {
                self.backend.move_mouse(*x, *y)?;
                self.backend.double_click(*button)?;
                input_sent = true;
            }
            ControlAction::MouseDrag {
                from_x,
                from_y,
                to_x,
                to_y,
            } => {
                self.backend.drag(*from_x, *from_y, *to_x, *to_y)?;
                input_sent = true;
            }
            ControlAction::Scroll { delta_x, delta_y } => {
                self.backend.scroll(*delta_x, *delta_y)?;
                input_sent = true;
            }
            ControlAction::TypeText { text } => {
                self.backend.type_text(text)?;
                input_sent = true;
            }
            ControlAction::KeyPress { key } => {
                if key.is_empty() || key.len() > 64 || key.chars().any(char::is_control) {
                    return Err("Key name is invalid or too long".to_string());
                }
                self.backend.key_press(key)?;
                input_sent = true;
            }
            ControlAction::Hotkey { keys } => {
                self.backend.hotkey(keys)?;
                input_sent = true;
            }
            ControlAction::Focus => {
                self.semantic.focus(&target)?;
            }
            ControlAction::SemanticClick {
                element_id, button, ..
            } => {
                self.semantic
                    .click_element(&target, element_id, *button, false)?;
                input_sent = true;
            }
            ControlAction::SemanticDoubleClick {
                element_id, button, ..
            } => {
                self.semantic
                    .click_element(&target, element_id, *button, true)?;
                input_sent = true;
            }
            ControlAction::Select { element_id, value } => {
                self.semantic.set_value(&target, element_id, value, true)?;
                input_sent = true;
            }
            ControlAction::SetValue { element_id, value } => {
                self.semantic.set_value(&target, element_id, value, false)?;
                input_sent = true;
            }
            ControlAction::Wait { milliseconds } => {
                std::thread::sleep(Duration::from_millis(*milliseconds))
            }
        }
        let (verified, verification_evidence, verification) = self.verify_postcondition(
            target_application_id,
            target_window_id,
            action,
            before_value,
        );
        let audit_id = self.record_audit_with_context(
            session_id,
            target_application_id,
            target_window_id,
            action,
            "executed",
            "approved",
            input_sent,
            verified,
            verification.clone(),
            context,
        )?;
        self.set_audit_verification_evidence(&audit_id, verification_evidence.clone())?;
        let (approval_level, _) =
            self.action_approval(target_application_id, target_window_id, action);
        Ok(ExecutionResult {
            input_sent,
            state_verified: verified,
            verification,
            verification_evidence,
            audit_id,
            approval_level,
        })
    }

    fn record_audit_with_context(
        &self,
        session_id: &str,
        target_application_id: &str,
        target_window_id: Option<&str>,
        action: &ControlAction,
        result: &str,
        approval: &str,
        input_sent: bool,
        state_verified: bool,
        verification: Option<String>,
        context: &AuditContext,
    ) -> Result<String, String> {
        self.record_named_audit_with_context(
            session_id,
            target_application_id,
            target_window_id,
            action_summary(action),
            result,
            approval,
            input_sent,
            state_verified,
            verification,
            context,
        )
    }

    fn record_named_audit_with_context(
        &self,
        session_id: &str,
        target_application_id: &str,
        target_window_id: Option<&str>,
        action_name: String,
        result: &str,
        approval: &str,
        input_sent: bool,
        state_verified: bool,
        verification: Option<String>,
        context: &AuditContext,
    ) -> Result<String, String> {
        let audit_id = format!("desktop-audit-{}", Uuid::new_v4());
        let approval_level = approval_level_for_name(&action_name);
        let record = ComputerAuditRecord {
            audit_id: audit_id.clone(),
            run_id: context.run_id.clone(),
            tool_call_id: context.tool_call_id.clone(),
            session_id: session_id.to_string(),
            target_application_id: target_application_id.to_string(),
            target_window_id: target_window_id.map(str::to_string),
            action: action_name,
            approval_level,
            result: result.to_string(),
            approval: approval.to_string(),
            input_sent,
            state_verified,
            verification,
            verification_evidence: None,
            screenshot_ref: None,
            created_at_ms: now_ms(),
        };
        let mut audit = lock(&self.audit, "desktop control audit")?;
        if audit.len() >= 1024 {
            audit.remove(0);
        }
        audit.push(record);
        Ok(audit_id)
    }

    pub fn audit_snapshot(&self) -> Result<Vec<ComputerAuditRecord>, String> {
        Ok(lock(&self.audit, "desktop control audit")?.clone())
    }

    fn record_screenshot_audit(
        &self,
        session_id: &str,
        target_application_id: &str,
        target_window_id: Option<&str>,
        artifact_id: &str,
        context: &AuditContext,
    ) -> Result<String, String> {
        let audit_id = self.record_named_audit_with_context(
            session_id,
            target_application_id,
            target_window_id,
            "screenshot".to_string(),
            "executed",
            "grant",
            false,
            true,
            Some("bounded screenshot captured".to_string()),
            context,
        )?;
        let mut audit = lock(&self.audit, "desktop control audit")?;
        if let Some(record) = audit.iter_mut().find(|record| record.audit_id == audit_id) {
            record.screenshot_ref = Some(artifact_id.to_string());
        }
        Ok(audit_id)
    }

    /// Records a screenshot requested through the paired daemon. The remote
    /// path has no frontend turn/tool identity, but it must still create the
    /// same durable audit row as the local screenshot command.
    pub fn record_screenshot_audit_for_remote(
        &self,
        session_id: &str,
        target_application_id: &str,
        target_window_id: Option<&str>,
        artifact_id: &str,
    ) -> Result<String, String> {
        self.record_screenshot_audit(
            session_id,
            target_application_id,
            target_window_id,
            artifact_id,
            &AuditContext::default(),
        )
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
        self.begin_action_for_target(session_id, target_application_id, None, action)
    }

    pub fn begin_action_for_target(
        &self,
        session_id: &str,
        target_application_id: &str,
        target_window_id: Option<&str>,
        action: ControlAction,
    ) -> Result<ActionGate, String> {
        self.begin_action_for_target_with_context(
            session_id,
            target_application_id,
            target_window_id,
            action,
            AuditContext::default(),
        )
    }

    fn begin_action_for_target_with_context(
        &self,
        session_id: &str,
        target_application_id: &str,
        target_window_id: Option<&str>,
        action: ControlAction,
        context: AuditContext,
    ) -> Result<ActionGate, String> {
        let (session, _) = self.require_active_session_for_target(
            session_id,
            target_application_id,
            target_window_id,
            Self::action_requires_frontmost(&action),
        )?;
        let (approval, description) =
            self.action_approval(target_application_id, target_window_id, &action);
        if session.approved_batch && approval != ApprovalLevel::Critical {
            return Ok(ActionGate::Executed(self.execute_for_target_with_context(
                session_id,
                target_application_id,
                target_window_id,
                &action,
                &context,
            )));
        }
        let approval_digest = approval_digest(
            session_id,
            target_application_id,
            target_window_id,
            &action,
            approval,
            &description,
        );
        let (sender, receiver) = oneshot::channel::<bool>();
        let action_id = format!("control-action-{}", Uuid::new_v4());
        lock(&self.pending, "pending control actions")?.insert(
            action_id.clone(),
            PendingControlAction {
                session_id: session_id.to_string(),
                target_application_id: target_application_id.to_string(),
                target_window_id: target_window_id.map(str::to_string),
                action: action.clone(),
                context,
                approval_level: approval,
                approval_digest,
                description,
                sender: Some(sender),
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
        let mut pending = lock(&self.pending, "pending control actions")?;
        if !pending.contains_key(action_id) {
            return Ok(false);
        }
        // If the receiving end was already dropped (e.g. the request timed
        // out just before this call), there's nothing left to notify.
        let sender = pending
            .get_mut(action_id)
            .and_then(|pending| pending.sender.take());
        let Some(sender) = sender else {
            return Ok(false);
        };
        let _ = sender.send(approve);
        if !approve {
            pending.remove(action_id);
        }
        Ok(true)
    }

    fn take_approved_pending(
        &self,
        action_id: &str,
        action: &ControlAction,
    ) -> Result<ExecutionResult, String> {
        let pending = lock(&self.pending, "pending control actions")?
            .remove(action_id)
            .ok_or_else(|| "Approved control action was no longer pending".to_string())?;
        if &pending.action != action {
            return Err("Pending action payload changed before approval".to_string());
        }
        let (approval, description) = self.action_approval(
            &pending.target_application_id,
            pending.target_window_id.as_deref(),
            &pending.action,
        );
        let digest = approval_digest(
            &pending.session_id,
            &pending.target_application_id,
            pending.target_window_id.as_deref(),
            &pending.action,
            approval,
            &description,
        );
        if approval != pending.approval_level || digest != pending.approval_digest {
            let _ = self.record_named_audit_with_context(
                &pending.session_id,
                &pending.target_application_id,
                pending.target_window_id.as_deref(),
                format!("{} (approval invalidated)", pending.description),
                "refused",
                "approval_invalidated",
                false,
                false,
                Some("Target semantics or risk changed while approval was pending".to_string()),
                &pending.context,
            );
            return Err("Control action approval was invalidated by a target or risk change; approve the refreshed action".to_string());
        }
        self.execute_for_target_with_context(
            &pending.session_id,
            &pending.target_application_id,
            pending.target_window_id.as_deref(),
            &pending.action,
            &pending.context,
        )
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
        Ok(self
            .finish_pending_with_result(action_id, action, approve)?
            .is_some())
    }

    pub fn finish_pending_with_result(
        &self,
        action_id: &str,
        action: &ControlAction,
        approve: bool,
    ) -> Result<Option<ExecutionResult>, String> {
        let Some(pending) = lock(&self.pending, "pending control actions")?.remove(action_id)
        else {
            return Ok(None);
        };
        if let Some(sender) = pending.sender {
            let _ = sender.send(approve);
        }
        if !approve {
            let _ = self.record_named_audit_with_context(
                &pending.session_id,
                &pending.target_application_id,
                pending.target_window_id.as_deref(),
                format!("{} (denied)", action_summary(&pending.action)),
                "denied",
                "operator_denied",
                false,
                false,
                Some("operator denied the pending action".to_string()),
                &pending.context,
            );
            return Ok(None);
        }
        if &pending.action != action {
            return Err("Pending action payload changed before approval".to_string());
        }
        let (approval, description) = self.action_approval(
            &pending.target_application_id,
            pending.target_window_id.as_deref(),
            &pending.action,
        );
        let digest = approval_digest(
            &pending.session_id,
            &pending.target_application_id,
            pending.target_window_id.as_deref(),
            &pending.action,
            approval,
            &description,
        );
        if approval != pending.approval_level || digest != pending.approval_digest {
            return Err("Control action approval was invalidated by a target or risk change; approve the refreshed action".to_string());
        }
        let result = self.execute_for_target_with_context(
            &pending.session_id,
            &pending.target_application_id,
            pending.target_window_id.as_deref(),
            &pending.action,
            &pending.context,
        )?;
        Ok(Some(result))
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
                if let Some(sender) = action.sender {
                    let _ = sender.send(false);
                }
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
                if let Some(sender) = action.sender {
                    let _ = sender.send(false);
                }
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

fn ensure_control_window(window: &tauri::Window) -> Result<(), String> {
    if matches!(window.label(), "main" | "companion-overlay") {
        Ok(())
    } else {
        Err(
            "Desktop control can only be managed from the main window or visible control overlay"
                .to_string(),
        )
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
    allowed_windows: Option<Vec<String>>,
    allow_screenshots: Option<bool>,
    allow_keyboard_input: Option<bool>,
    allow_clipboard_read: Option<bool>,
    approval_policy: Option<ApprovalPolicy>,
) -> Result<ControlSession, String> {
    ensure_main_window(&window)?;
    let mode = crate::permissions::get_permission_mode_impl(&permissions_state);
    let session = state.start_session_with_options(
        &mode,
        allowed_applications,
        lifetime_ms,
        SessionGrantOptions {
            allowed_windows: allowed_windows.unwrap_or_default(),
            allow_screenshots: allow_screenshots.unwrap_or(false),
            allow_keyboard_input: allow_keyboard_input.unwrap_or(false),
            allow_clipboard_read: allow_clipboard_read.unwrap_or(false),
            approval_policy: Some(approval_policy.unwrap_or(if approved_batch {
                ApprovalPolicy::ApprovedBatch
            } else {
                ApprovalPolicy::PerAction
            })),
        },
    )?;
    // The visible, always-on-top overlay is part of the safety invariant. Do
    // not leave a live input session behind when the operator cannot see it.
    if let Err(error) = crate::m7_companion::show_overlay(&app) {
        let _ = state.stop_session(&session.session_id);
        return Err(format!(
            "Could not establish the desktop-control indicator: {error}"
        ));
    }
    let _ = app.emit("desktop-control://session-state", &session);
    Ok(session)
}

#[tauri::command]
pub fn desktop_control_stop_session(
    app: tauri::AppHandle,
    window: tauri::Window,
    state: tauri::State<'_, DesktopControlState>,
    session_id: String,
) -> Result<bool, String> {
    ensure_control_window(&window)?;
    let stopped = state.stop_session(&session_id)?;
    if !state.any_session_active()? {
        if let Some(overlay) = app.get_webview_window("companion-overlay") {
            let _ = overlay.hide();
        }
    }
    let _ = app.emit(
        "desktop-control://session-state",
        state.sessions_snapshot()?,
    );
    Ok(stopped)
}

#[tauri::command]
pub fn desktop_control_pause_session(
    app: tauri::AppHandle,
    window: tauri::Window,
    state: tauri::State<'_, DesktopControlState>,
    session_id: String,
    paused: bool,
) -> Result<bool, String> {
    ensure_control_window(&window)?;
    let changed = state.pause_session(&session_id, paused)?;
    let _ = app.emit(
        "desktop-control://session-state",
        state.sessions_snapshot()?,
    );
    Ok(changed)
}

#[tauri::command]
pub fn desktop_control_sessions(
    app: tauri::AppHandle,
    state: tauri::State<'_, DesktopControlState>,
) -> Result<Vec<ControlSession>, String> {
    let sessions = state.sessions_snapshot()?;
    if !sessions.iter().any(|session| session.active) {
        if let Some(overlay) = app.get_webview_window("companion-overlay") {
            let _ = overlay.hide();
        }
    }
    Ok(sessions)
}

#[tauri::command]
pub async fn desktop_control_request_action(
    app: tauri::AppHandle,
    window: tauri::Window,
    state: tauri::State<'_, DesktopControlState>,
    session_id: String,
    target_application_id: String,
    target_window_id: Option<String>,
    action: ControlAction,
    turn_id: Option<String>,
    tool_call_id: Option<String>,
) -> Result<ActionOutcome, String> {
    ensure_main_window(&window)?;
    request_action_impl(
        &app,
        state.inner(),
        &session_id,
        &target_application_id,
        target_window_id.as_deref(),
        action,
        turn_id,
        tool_call_id,
    )
    .await
}

async fn request_action_impl(
    app: &tauri::AppHandle,
    state: &DesktopControlState,
    session_id: &str,
    target_application_id: &str,
    target_window_id: Option<&str>,
    action: ControlAction,
    run_id: Option<String>,
    tool_call_id: Option<String>,
) -> Result<ActionOutcome, String> {
    let context = AuditContext {
        run_id,
        tool_call_id,
    };
    let gate = match state.begin_action_for_target_with_context(
        session_id,
        target_application_id,
        target_window_id,
        action.clone(),
        context.clone(),
    ) {
        Ok(gate) => gate,
        Err(error) => {
            let _ = state.record_named_audit_with_context(
                session_id,
                target_application_id,
                target_window_id,
                format!("{} (refused)", action_summary(&action)),
                "refused",
                "not_approved",
                false,
                false,
                Some(error.clone()),
                &context,
            );
            return Err(error);
        }
    };
    match gate {
        ActionGate::Executed(result) => {
            let result = result?;
            Ok(ActionOutcome {
                action_id: format!("batch-{}", Uuid::new_v4()),
                executed: true,
                input_sent: result.input_sent,
                state_verified: result.state_verified,
                verification: result.verification,
                verification_evidence: result.verification_evidence,
                audit_id: result.audit_id,
                approval_level: result.approval_level,
            })
        }
        ActionGate::Pending {
            action_id,
            receiver,
        } => {
            let (approval_level, description) =
                state.action_approval(target_application_id, target_window_id, &action);
            let _ = app.emit(
                "desktop-control://action-pending",
                PendingActionSummary {
                    action_id: action_id.clone(),
                    session_id: session_id.to_string(),
                    target_application_id: target_application_id.to_string(),
                    target_window_id: target_window_id.map(str::to_string),
                    approval_level,
                    description,
                    action: redacted_action_for_ui(&action),
                },
            );
            match tokio::time::timeout(ACTION_APPROVAL_TIMEOUT, receiver).await {
                Ok(Ok(true)) => {
                    let result = state.take_approved_pending(&action_id, &action)?;
                    Ok(ActionOutcome {
                        action_id,
                        executed: true,
                        input_sent: result.input_sent,
                        state_verified: result.state_verified,
                        verification: result.verification,
                        verification_evidence: result.verification_evidence,
                        audit_id: result.audit_id,
                        approval_level: result.approval_level,
                    })
                }
                Ok(Ok(false)) => {
                    let _ = state.record_named_audit_with_context(
                        session_id,
                        target_application_id,
                        target_window_id,
                        format!("{} (denied)", action_summary(&action)),
                        "denied",
                        "operator_denied",
                        false,
                        false,
                        Some("operator denied the pending action".to_string()),
                        &context,
                    );
                    Err("Control action was denied".to_string())
                }
                // Timed out, or the sender was dropped without a response.
                Ok(Err(_)) | Err(_) => {
                    state.remove_pending(&action_id);
                    let _ = state.record_named_audit_with_context(
                        session_id,
                        target_application_id,
                        target_window_id,
                        format!("{} (timeout)", action_summary(&action)),
                        "timeout",
                        "not_approved",
                        false,
                        false,
                        Some("operator approval timed out".to_string()),
                        &context,
                    );
                    Err("Control action approval timed out".to_string())
                }
            }
        }
    }
}

#[tauri::command]
pub fn tool_computer_list_targets(
    state: tauri::State<'_, DesktopControlState>,
    session_id: String,
    turn_id: Option<String>,
    tool_call_id: Option<String>,
) -> Result<Vec<ComputerTarget>, String> {
    let _ = (turn_id, tool_call_id);
    state.list_targets_for_session(&session_id)
}

#[tauri::command]
pub fn tool_computer_inspect(
    state: tauri::State<'_, DesktopControlState>,
    session_id: String,
    target_application_id: String,
    target_window_id: Option<String>,
    query: Option<String>,
    turn_id: Option<String>,
    tool_call_id: Option<String>,
) -> Result<ComputerInspection, String> {
    let _ = (turn_id, tool_call_id);
    state.inspect_for_session(
        &session_id,
        &target_application_id,
        target_window_id.as_deref(),
        query.as_deref(),
    )
}

#[tauri::command]
pub fn tool_computer_screenshot(
    app: tauri::AppHandle,
    app_state: tauri::State<'_, crate::AppState>,
    state: tauri::State<'_, DesktopControlState>,
    session_id: String,
    target_application_id: String,
    target_window_id: Option<String>,
    bounds: Option<ComputerBounds>,
    turn_id: Option<String>,
    tool_call_id: Option<String>,
) -> Result<ComputerScreenshot, String> {
    let (target, bytes, captured_bounds) = state.screenshot_for_session(
        &session_id,
        &target_application_id,
        target_window_id.as_deref(),
        bounds,
    )?;
    let blob = crate::artifact_commands::store_for(&app, app_state.inner())?
        .put(&bytes)
        .map_err(|error| format!("Could not store screenshot artifact: {error}"))?;
    let audit_id = state.record_screenshot_audit(
        &session_id,
        &target_application_id,
        target_window_id.as_deref(),
        &blob.id,
        &AuditContext {
            run_id: turn_id,
            tool_call_id,
        },
    )?;
    Ok(ComputerScreenshot {
        artifact_id: blob.id,
        audit_id,
        media_type: "image/png".to_string(),
        size_bytes: blob.size,
        content_base64: base64::engine::general_purpose::STANDARD.encode(bytes),
        bounds: captured_bounds,
        target,
    })
}

#[tauri::command]
pub fn tool_computer_clipboard_read(
    state: tauri::State<'_, DesktopControlState>,
    session_id: String,
    target_application_id: String,
    target_window_id: Option<String>,
    turn_id: Option<String>,
    tool_call_id: Option<String>,
) -> Result<serde_json::Value, String> {
    let (content, audit_id) = state.clipboard_for_session(
        &session_id,
        &target_application_id,
        target_window_id.as_deref(),
        &AuditContext {
            run_id: turn_id,
            tool_call_id,
        },
    )?;
    Ok(serde_json::json!({
        "content": content,
        "auditId": audit_id,
        "note": "Clipboard reads are separately granted and are never included in the audit content",
    }))
}

#[tauri::command]
pub async fn tool_computer_focus(
    app: tauri::AppHandle,
    state: tauri::State<'_, DesktopControlState>,
    session_id: String,
    target_application_id: String,
    target_window_id: Option<String>,
    turn_id: Option<String>,
    tool_call_id: Option<String>,
) -> Result<ActionOutcome, String> {
    request_action_impl(
        &app,
        state.inner(),
        &session_id,
        &target_application_id,
        target_window_id.as_deref(),
        ControlAction::Focus,
        turn_id,
        tool_call_id,
    )
    .await
}

#[tauri::command]
pub async fn tool_computer_click(
    app: tauri::AppHandle,
    state: tauri::State<'_, DesktopControlState>,
    session_id: String,
    target_application_id: String,
    target_window_id: Option<String>,
    element_id: Option<String>,
    x: Option<i32>,
    y: Option<i32>,
    button: Option<MouseButtonKind>,
    expected_value: Option<String>,
    turn_id: Option<String>,
    tool_call_id: Option<String>,
) -> Result<ActionOutcome, String> {
    let button = button.unwrap_or(MouseButtonKind::Left);
    let action = if let Some(element_id) = element_id {
        ControlAction::SemanticClick {
            element_id,
            button,
            expected_value,
        }
    } else {
        ControlAction::MouseClickAt {
            x: x.ok_or_else(|| "computer_click needs element_id or x and y".to_string())?,
            y: y.ok_or_else(|| "computer_click needs element_id or x and y".to_string())?,
            button,
        }
    };
    request_action_impl(
        &app,
        state.inner(),
        &session_id,
        &target_application_id,
        target_window_id.as_deref(),
        action,
        turn_id,
        tool_call_id,
    )
    .await
}

#[tauri::command]
pub async fn tool_computer_double_click(
    app: tauri::AppHandle,
    state: tauri::State<'_, DesktopControlState>,
    session_id: String,
    target_application_id: String,
    target_window_id: Option<String>,
    element_id: Option<String>,
    x: Option<i32>,
    y: Option<i32>,
    button: Option<MouseButtonKind>,
    expected_value: Option<String>,
    turn_id: Option<String>,
    tool_call_id: Option<String>,
) -> Result<ActionOutcome, String> {
    let button = button.unwrap_or(MouseButtonKind::Left);
    let action = if let Some(element_id) = element_id {
        ControlAction::SemanticDoubleClick {
            element_id,
            button,
            expected_value,
        }
    } else {
        ControlAction::MouseDoubleClickAt {
            x: x.ok_or_else(|| "computer_double_click needs element_id or x and y".to_string())?,
            y: y.ok_or_else(|| "computer_double_click needs element_id or x and y".to_string())?,
            button,
        }
    };
    request_action_impl(
        &app,
        state.inner(),
        &session_id,
        &target_application_id,
        target_window_id.as_deref(),
        action,
        turn_id,
        tool_call_id,
    )
    .await
}

#[tauri::command]
pub async fn tool_computer_scroll(
    app: tauri::AppHandle,
    state: tauri::State<'_, DesktopControlState>,
    session_id: String,
    target_application_id: String,
    target_window_id: Option<String>,
    delta_x: i32,
    delta_y: i32,
    turn_id: Option<String>,
    tool_call_id: Option<String>,
) -> Result<ActionOutcome, String> {
    request_action_impl(
        &app,
        state.inner(),
        &session_id,
        &target_application_id,
        target_window_id.as_deref(),
        ControlAction::Scroll { delta_x, delta_y },
        turn_id,
        tool_call_id,
    )
    .await
}

#[tauri::command]
pub async fn tool_computer_type(
    app: tauri::AppHandle,
    state: tauri::State<'_, DesktopControlState>,
    session_id: String,
    target_application_id: String,
    target_window_id: Option<String>,
    text: String,
    turn_id: Option<String>,
    tool_call_id: Option<String>,
) -> Result<ActionOutcome, String> {
    request_action_impl(
        &app,
        state.inner(),
        &session_id,
        &target_application_id,
        target_window_id.as_deref(),
        ControlAction::TypeText { text },
        turn_id,
        tool_call_id,
    )
    .await
}

#[tauri::command]
pub async fn tool_computer_key(
    app: tauri::AppHandle,
    state: tauri::State<'_, DesktopControlState>,
    session_id: String,
    target_application_id: String,
    target_window_id: Option<String>,
    key: String,
    turn_id: Option<String>,
    tool_call_id: Option<String>,
) -> Result<ActionOutcome, String> {
    request_action_impl(
        &app,
        state.inner(),
        &session_id,
        &target_application_id,
        target_window_id.as_deref(),
        ControlAction::KeyPress { key },
        turn_id,
        tool_call_id,
    )
    .await
}

#[tauri::command]
pub async fn tool_computer_hotkey(
    app: tauri::AppHandle,
    state: tauri::State<'_, DesktopControlState>,
    session_id: String,
    target_application_id: String,
    target_window_id: Option<String>,
    keys: Vec<String>,
    turn_id: Option<String>,
    tool_call_id: Option<String>,
) -> Result<ActionOutcome, String> {
    request_action_impl(
        &app,
        state.inner(),
        &session_id,
        &target_application_id,
        target_window_id.as_deref(),
        ControlAction::Hotkey { keys },
        turn_id,
        tool_call_id,
    )
    .await
}

#[tauri::command]
pub async fn tool_computer_wait(
    app: tauri::AppHandle,
    state: tauri::State<'_, DesktopControlState>,
    session_id: String,
    target_application_id: String,
    target_window_id: Option<String>,
    milliseconds: u64,
    turn_id: Option<String>,
    tool_call_id: Option<String>,
) -> Result<ActionOutcome, String> {
    request_action_impl(
        &app,
        state.inner(),
        &session_id,
        &target_application_id,
        target_window_id.as_deref(),
        ControlAction::Wait { milliseconds },
        turn_id,
        tool_call_id,
    )
    .await
}

#[tauri::command]
pub async fn tool_computer_select(
    app: tauri::AppHandle,
    state: tauri::State<'_, DesktopControlState>,
    session_id: String,
    target_application_id: String,
    target_window_id: Option<String>,
    element_id: String,
    value: String,
    turn_id: Option<String>,
    tool_call_id: Option<String>,
) -> Result<ActionOutcome, String> {
    request_action_impl(
        &app,
        state.inner(),
        &session_id,
        &target_application_id,
        target_window_id.as_deref(),
        ControlAction::Select { element_id, value },
        turn_id,
        tool_call_id,
    )
    .await
}

#[tauri::command]
pub async fn tool_computer_set_value(
    app: tauri::AppHandle,
    state: tauri::State<'_, DesktopControlState>,
    session_id: String,
    target_application_id: String,
    target_window_id: Option<String>,
    element_id: String,
    value: String,
    turn_id: Option<String>,
    tool_call_id: Option<String>,
) -> Result<ActionOutcome, String> {
    request_action_impl(
        &app,
        state.inner(),
        &session_id,
        &target_application_id,
        target_window_id.as_deref(),
        ControlAction::SetValue { element_id, value },
        turn_id,
        tool_call_id,
    )
    .await
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
    fn grants_carry_independent_capabilities_and_windows() {
        let state = state();
        let session = state
            .start_session_with_options(
                "manual",
                allow(&["TestApp"]),
                60_000,
                SessionGrantOptions {
                    allowed_windows: vec!["TestApp::window-1".to_string()],
                    allow_screenshots: false,
                    allow_keyboard_input: false,
                    allow_clipboard_read: false,
                    approval_policy: Some(ApprovalPolicy::PerAction),
                },
            )
            .unwrap();
        assert_eq!(session.allowed_windows, ["TestApp::window-1"]);
        assert!(!session.allow_screenshots);
        assert!(!session.allow_keyboard_input);
        assert_eq!(session.approval_policy, ApprovalPolicy::PerAction);
    }

    #[test]
    fn sensitive_targets_are_refused_even_when_allowlisted() {
        let state = state();
        let session = state
            .start_session_impl("manual", allow(&["1Password"]), 60_000, true)
            .unwrap();
        let error = match state.begin_action(
            &session.session_id,
            "1Password",
            ControlAction::MouseClick {
                button: MouseButtonKind::Left,
            },
        ) {
            Err(error) => error,
            Ok(_) => panic!("sensitive target must be refused"),
        };
        assert!(error.contains("Sensitive"));
    }

    #[test]
    fn paused_session_refuses_actions_without_revoking_the_grant() {
        let state = state();
        let session = state
            .start_session_impl("manual", allow(&["Notes"]), 60_000, true)
            .unwrap();
        assert!(state.pause_session(&session.session_id, true).unwrap());
        let error = match state.begin_action(
            &session.session_id,
            "Notes",
            ControlAction::MouseMove { x: 1, y: 1 },
        ) {
            Err(error) => error,
            Ok(_) => panic!("paused session must refuse actions"),
        };
        assert!(error.contains("paused"));
        assert!(state.sessions_snapshot().unwrap()[0].active);
        assert!(state.pause_session(&session.session_id, false).unwrap());
    }

    #[test]
    fn audit_redacts_typed_text_and_preserves_execution_verification() {
        let state = state();
        let session = state
            .start_session_impl("manual", allow(&["Notes"]), 60_000, true)
            .unwrap();
        let ActionGate::Executed(result) = state
            .begin_action(
                &session.session_id,
                "Notes",
                ControlAction::TypeText {
                    text: "secret-value".to_string(),
                },
            )
            .unwrap()
        else {
            panic!("approved batch must execute immediately");
        };
        let result = result.unwrap();
        assert!(result.input_sent);
        assert!(!result.state_verified);
        assert!(result
            .verification
            .as_deref()
            .is_some_and(|message| message.contains("no element postcondition")));
        let audit = state.audit_snapshot().unwrap();
        assert!(audit[0].action.contains("redacted"));
        assert!(!audit[0].action.contains("secret-value"));
    }

    #[test]
    fn durable_value_verification_evidence_is_redacted_but_outcome_can_retain_it() {
        let evidence = VerificationEvidence {
            kind: "element_value".to_string(),
            element_id: Some("element-1".to_string()),
            expected_value: Some("secret-value".to_string()),
            observed_value: Some("secret-value".to_string()),
            matched: true,
            detail: "compared".to_string(),
        };
        let audit = evidence.redacted_for_audit();
        assert!(audit.expected_value.is_none());
        assert!(audit.observed_value.is_none());
        assert!(audit.detail.contains("redacted"));
        assert_eq!(evidence.expected_value.as_deref(), Some("secret-value"));
    }

    #[test]
    fn approval_digest_changes_when_target_semantics_or_risk_changes() {
        let action = ControlAction::SetValue {
            element_id: "element-1".to_string(),
            value: "hello".to_string(),
        };
        let first = approval_digest(
            "session",
            "Notes",
            Some("window"),
            &action,
            ApprovalLevel::High,
            "Edit",
        );
        let changed_label = approval_digest(
            "session",
            "Notes",
            Some("window"),
            &action,
            ApprovalLevel::Critical,
            "Delete",
        );
        assert_ne!(first, changed_label);
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

    // ----- Wayland detection (pure, host-testable) -------------------------
    //
    // These run on this macOS build machine even though `is_wayland_session`'s
    // only production caller is Linux-gated — that is the whole point of
    // keeping the decision pure and env-free.

    #[test]
    fn wayland_session_type_is_detected() {
        assert!(is_wayland_session(Some("wayland"), None));
        // Case-insensitive and tolerant of surrounding whitespace.
        assert!(is_wayland_session(Some("Wayland"), None));
        assert!(is_wayland_session(Some(" wayland "), None));
    }

    #[test]
    fn wayland_display_set_without_a_session_type_is_detected() {
        assert!(is_wayland_session(None, Some("wayland-0")));
    }

    #[test]
    fn x11_session_type_is_not_wayland_even_with_wayland_display_set() {
        // An explicit session type is authoritative: `x11` is never Wayland,
        // even if a stray WAYLAND_DISPLAY is also present (e.g. XWayland).
        assert!(!is_wayland_session(Some("x11"), None));
        assert!(!is_wayland_session(Some("x11"), Some("wayland-0")));
    }

    #[test]
    fn no_signals_assumes_x11_and_does_not_block() {
        assert!(!is_wayland_session(None, None));
        // Empty values are treated as "unset" and must not block either.
        assert!(!is_wayland_session(Some(""), None));
        assert!(!is_wayland_session(Some("   "), Some("")));
        // An unknown session type (not wayland) with no WAYLAND_DISPLAY is
        // likewise not treated as Wayland.
        assert!(!is_wayland_session(Some("tty"), None));
    }

    #[test]
    fn wayland_unsupported_message_is_clear_about_x11_working() {
        assert!(WAYLAND_UNSUPPORTED_MESSAGE.contains("Wayland"));
        assert!(WAYLAND_UNSUPPORTED_MESSAGE.contains("X11 sessions work today"));
    }
}
