//! Native operating-system dictation for the main composer.
//!
//! This module is deliberately separate from M7 Talk. It owns one short-lived
//! recognition session, emits only text/state events, and never creates an
//! audio artifact or an agent turn.

use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use tauri::{Emitter, Manager};

#[cfg(target_os = "macos")]
mod macos;
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
mod unsupported;
#[cfg(target_os = "windows")]
mod windows;

pub const STATE_EVENT: &str = "dictation://state";
pub const PARTIAL_EVENT: &str = "dictation://partial";
pub const FINAL_EVENT: &str = "dictation://final";
pub const ERROR_EVENT: &str = "dictation://error";

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DictationLanguage {
    pub id: String,
    pub label: String,
    #[serde(default)]
    pub supports_on_device: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DictationCapabilities {
    pub supported: bool,
    pub platform: String,
    pub engine: String,
    pub supports_partial_results: bool,
    pub supports_on_device: bool,
    pub languages: Vec<DictationLanguage>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DictationStartResult {
    pub session_id: String,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DictationStateEvent {
    pub session_id: String,
    pub state: String,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DictationTextEvent {
    pub session_id: String,
    pub text: String,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DictationErrorEvent {
    pub session_id: String,
    pub code: String,
    pub message: String,
}

#[derive(Clone, Debug)]
pub enum NativeEvent {
    State {
        session_id: String,
        state: String,
    },
    Partial {
        session_id: String,
        text: String,
    },
    Final {
        session_id: String,
        text: String,
    },
    Error {
        session_id: String,
        code: String,
        message: String,
    },
}

pub type NativeCallback = Arc<dyn Fn(NativeEvent) + Send + Sync + 'static>;

/// Native backends may report an immediate startup failure on their worker
/// thread. Hold those events until the application has published the session
/// in `DictationRuntime`, so an error cannot arrive before the frontend knows
/// which session it belongs to. The frontend owns the session ID, so it can
/// publish its insertion state before calling the native start command.
struct EventGate {
    pending: Mutex<Option<Vec<NativeEvent>>>,
}

impl Default for EventGate {
    fn default() -> Self {
        Self {
            pending: Mutex::new(Some(Vec::new())),
        }
    }
}

impl EventGate {
    fn emit(&self, app: &tauri::AppHandle, event: NativeEvent) {
        let Ok(mut pending) = self.pending.lock() else {
            return;
        };
        if let Some(events) = pending.as_mut() {
            events.push(event);
        } else {
            // Keep the lock while emitting so `open` drains all queued events
            // before a concurrent native callback can overtake them.
            emit_native_event(app, event);
        }
    }

    fn open(&self, app: &tauri::AppHandle) {
        let Ok(mut pending) = self.pending.lock() else {
            return;
        };
        if let Some(events) = pending.take() {
            for event in events {
                emit_native_event(app, event);
            }
        }
    }
}

enum PlatformSession {
    #[cfg(target_os = "macos")]
    Macos(macos::Session),
    #[cfg(target_os = "windows")]
    Windows(windows::Session),
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    Unsupported(unsupported::Session),
}

impl PlatformSession {
    fn stop(&self) -> Result<(), String> {
        match self {
            #[cfg(target_os = "macos")]
            Self::Macos(session) => session.stop(),
            #[cfg(target_os = "windows")]
            Self::Windows(session) => session.stop(),
            #[cfg(not(any(target_os = "macos", target_os = "windows")))]
            Self::Unsupported(session) => session.stop(),
        }
    }

    fn cancel(&self) -> Result<(), String> {
        match self {
            #[cfg(target_os = "macos")]
            Self::Macos(session) => session.cancel(),
            #[cfg(target_os = "windows")]
            Self::Windows(session) => session.cancel(),
            #[cfg(not(any(target_os = "macos", target_os = "windows")))]
            Self::Unsupported(session) => session.cancel(),
        }
    }
}

struct ActiveSession {
    session_id: String,
    native: PlatformSession,
}

/// Application-owned native session state. The mutex is the single ownership
/// boundary that prevents two composer microphones from listening at once.
#[derive(Default)]
pub struct DictationRuntime {
    active: Mutex<Option<ActiveSession>>,
}

fn ensure_main_window(window: &tauri::Window) -> Result<(), String> {
    if window.label() == "main" {
        Ok(())
    } else {
        Err("Composer dictation is available only in the main window".to_string())
    }
}

fn validate_session_id(session_id: &str) -> Result<(), String> {
    if session_id.trim().is_empty()
        || session_id.len() > 128
        || session_id.chars().any(char::is_control)
    {
        return Err("Invalid dictation session id".to_string());
    }
    Ok(())
}

fn emit_native_event(app: &tauri::AppHandle, event: NativeEvent) {
    match event {
        NativeEvent::State { session_id, state } => {
            let _ = app.emit(STATE_EVENT, DictationStateEvent { session_id, state });
        }
        NativeEvent::Partial { session_id, text } => {
            let _ = app.emit(PARTIAL_EVENT, DictationTextEvent { session_id, text });
        }
        NativeEvent::Final { session_id, text } => {
            let _ = app.emit(FINAL_EVENT, DictationTextEvent { session_id, text });
        }
        NativeEvent::Error {
            session_id,
            code,
            message,
        } => {
            let _ = app.emit(
                ERROR_EVENT,
                DictationErrorEvent {
                    session_id,
                    code,
                    message,
                },
            );
        }
    }
}

fn platform_capabilities() -> DictationCapabilities {
    #[cfg(target_os = "macos")]
    {
        return macos::capabilities();
    }
    #[cfg(target_os = "windows")]
    {
        return windows::capabilities();
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        unsupported::capabilities()
    }
}

fn start_platform(
    session_id: String,
    language: Option<String>,
    require_on_device: bool,
    callback: NativeCallback,
) -> Result<PlatformSession, String> {
    #[cfg(target_os = "macos")]
    {
        return macos::start(session_id, language, require_on_device, callback)
            .map(PlatformSession::Macos);
    }
    #[cfg(target_os = "windows")]
    {
        return windows::start(session_id, language, callback).map(PlatformSession::Windows);
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        let _ = (session_id, language, require_on_device, callback);
        unsupported::start().map(PlatformSession::Unsupported)
    }
}

fn take_session(
    runtime: &DictationRuntime,
    session_id: &str,
) -> Result<Option<ActiveSession>, String> {
    let mut active = runtime
        .active
        .lock()
        .map_err(|_| "Dictation session state is unavailable".to_string())?;
    if active
        .as_ref()
        .is_some_and(|session| session.session_id == session_id)
    {
        Ok(active.take())
    } else {
        Ok(None)
    }
}

#[tauri::command]
pub fn dictation_capabilities(window: tauri::Window) -> Result<DictationCapabilities, String> {
    ensure_main_window(&window)?;
    Ok(platform_capabilities())
}

#[tauri::command]
pub fn dictation_open_permission_settings(
    window: tauri::Window,
    kind: String,
) -> Result<(), String> {
    ensure_main_window(&window)?;
    match kind.as_str() {
        "microphone" | "speech" => open_permission_settings(&kind),
        _ => Err("Invalid dictation permission kind".to_string()),
    }
}

fn open_permission_settings(kind: &str) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    {
        return macos::open_permission_settings(kind);
    }
    #[cfg(target_os = "windows")]
    {
        return windows::open_permission_settings(kind);
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        let _ = kind;
        unsupported::open_permission_settings()
    }
}

#[tauri::command]
pub fn dictation_start(
    window: tauri::Window,
    runtime: tauri::State<'_, DictationRuntime>,
    session_id: String,
    language: Option<String>,
    require_on_device: bool,
) -> Result<DictationStartResult, String> {
    ensure_main_window(&window)?;
    let app = window.app_handle().clone();
    let capabilities = platform_capabilities();
    if !capabilities.supported {
        return Err("Native OS speech recognition is not supported on this platform".to_string());
    }
    validate_session_id(&session_id)?;

    if let Some(previous) = runtime
        .active
        .lock()
        .map_err(|_| "Dictation session state is unavailable".to_string())?
        .take()
    {
        let _ = previous.native.cancel();
    }

    let gate = Arc::new(EventGate::default());
    let callback_gate = Arc::clone(&gate);
    let callback_app = app.clone();
    let callback: NativeCallback = Arc::new(move |event| callback_gate.emit(&callback_app, event));
    let native = start_platform(
        session_id.clone(),
        language.filter(|value| !value.trim().is_empty()),
        require_on_device,
        callback,
    )?;
    runtime
        .active
        .lock()
        .map_err(|_| "Dictation session state is unavailable".to_string())?
        .replace(ActiveSession {
            session_id: session_id.clone(),
            native,
        });
    gate.open(&app);
    Ok(DictationStartResult { session_id })
}

#[tauri::command]
pub fn dictation_stop(
    window: tauri::Window,
    runtime: tauri::State<'_, DictationRuntime>,
    session_id: String,
) -> Result<(), String> {
    ensure_main_window(&window)?;
    if let Some(session) = take_session(&runtime, &session_id)? {
        session.native.stop()?;
    }
    Ok(())
}

#[tauri::command]
pub fn dictation_cancel(
    window: tauri::Window,
    runtime: tauri::State<'_, DictationRuntime>,
    session_id: String,
) -> Result<(), String> {
    ensure_main_window(&window)?;
    if let Some(session) = take_session(&runtime, &session_id)? {
        session.native.cancel()?;
    }
    Ok(())
}

pub fn shutdown(runtime: &DictationRuntime) {
    let Ok(mut active) = runtime.active.lock() else {
        return;
    };
    if let Some(session) = active.take() {
        let _ = session.native.cancel();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_ids_are_scoped_and_stale_stop_is_a_noop() {
        let runtime = DictationRuntime::default();
        assert!(take_session(&runtime, "dictation-old").unwrap().is_none());
    }

    #[test]
    fn frontend_session_ids_are_validated_before_start() {
        assert!(validate_session_id("dictation-current").is_ok());
        assert!(validate_session_id("").is_err());
        assert!(validate_session_id("dictation\ncurrent").is_err());
        assert!(validate_session_id(&"x".repeat(129)).is_err());
    }

    #[test]
    fn event_names_are_stable() {
        assert_eq!(STATE_EVENT, "dictation://state");
        assert_eq!(PARTIAL_EVENT, "dictation://partial");
        assert_eq!(FINAL_EVENT, "dictation://final");
        assert_eq!(ERROR_EVENT, "dictation://error");
    }
}
