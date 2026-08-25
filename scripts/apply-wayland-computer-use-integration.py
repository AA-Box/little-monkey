from pathlib import Path


def replace_once(path: Path, old: str, new: str) -> None:
    text = path.read_text()
    count = text.count(old)
    if count != 1:
        raise SystemExit(f"{path}: expected exactly one match, got {count}: {old[:100]!r}")
    path.write_text(text.replace(old, new, 1))


root = Path(__file__).resolve().parents[1]
desktop = root / "src-tauri/src/desktop_control.rs"
cargo = root / "src-tauri/Cargo.toml"
wayland = root / "src-tauri/src/desktop_control/wayland_portal.rs"

replace_once(
    desktop,
    '''//! - **Linux/Wayland** — deliberately *unsupported*: `production_backend`\n//!   detects a Wayland session (see [`is_wayland_session`]) and returns\n//!   [`UnsupportedBackend`] rather than constructing `enigo::Enigo`, since\n//!   synthetic input on Wayland needs an xdg-desktop-portal/libei integration\n//!   that is not built here. X11 sessions work today.\n''',
    '''//! - **Linux/Wayland** — real semantic access through AT-SPI plus raw input\n//!   through the user-mediated xdg-desktop-portal RemoteDesktop/ScreenCast\n//!   session in `desktop_control/wayland_portal.rs`. No compositor bypass or\n//!   unrestricted `/dev/uinput` path exists. Active-window screenshots require\n//!   Screenshot portal v3 so capture cannot silently widen to a whole display.\n''',
)
replace_once(
    desktop,
    '''//! CAUTION: the Windows and Linux code paths below are compiled only on their\n//! own target_os, so they are NOT type-checked or runtime-verified in this\n//! macOS development environment. All non-trivial platform logic (the Wayland\n//! guard) is factored into pure, host-testable functions; the OS-gated blocks\n//! themselves are kept to a bare `enigo` call. See each block's own note.\n''',
    '''//! Linux/Wayland is deliberately capability-probed at runtime: compositors may\n//! ship different portal backends. Missing RemoteDesktop keyboard/pointer,\n//! monitor streams, or ActiveWindow screenshots fail with a precise\n//! `WAYLAND_CAPABILITY_UNAVAILABLE` error instead of falling back to insecure\n//! global input/capture.\n''',
)
replace_once(
    desktop,
    'use uuid::Uuid;\n',
    'use uuid::Uuid;\n\n#[cfg(target_os = "linux")]\nmod wayland_portal;\n',
)
replace_once(
    desktop,
    '''/// Message returned by [`production_backend`] when a Linux/Wayland session is\n/// detected — kept as a named constant so the wording is asserted in tests.\n/// Its only production use is Linux-gated, hence the non-Linux `allow`.\n#[cfg_attr(not(target_os = "linux"), allow(dead_code))]\nconst WAYLAND_UNSUPPORTED_MESSAGE: &str =\n    "Wayland session detected — desktop control needs an xdg-desktop-portal/libei integration \\\n     that isn't built yet; X11 sessions work today.";\n\n''',
    '',
)
replace_once(
    desktop,
    '''/// Selects the real [`EnigoBackend`] on macOS / Windows / Linux-X11, or a clear\n/// [`UnsupportedBackend`] otherwise (Linux-Wayland, other OSes, or when the\n/// real backend's own construction fails, e.g. missing Accessibility\n/// permission on macOS) — never a silent no-op. Only ever called once, from\n/// `DesktopControlState::production`; every test in this module constructs its\n/// own [`NullBackend`] instead.\n///\n/// NOTE: the Windows and Linux arms below are compiled only on their own\n/// target_os and were NOT compiled or runtime-verified in this macOS\n/// development environment. Each arm is deliberately just a Wayland guard (a\n/// pure, host-tested function) plus one generic `enigo::Enigo::new` call whose\n/// API is identical across every target.\nfn production_backend() -> Arc<dyn DesktopInputBackend> {\n    // Linux/Wayland fails *closed and clearly* before any `enigo` construction:\n    // building `enigo::Enigo` (x11rb backend) under Wayland would either fail\n    // confusingly or behave unpredictably. X11 sessions fall through to enigo.\n    #[cfg(target_os = "linux")]\n    {\n        if is_wayland_session_from_env() {\n            return Arc::new(UnsupportedBackend(WAYLAND_UNSUPPORTED_MESSAGE.to_string()));\n        }\n    }\n''',
    '''/// Selects the production input backend. Wayland gets its compositor-mediated\n/// portal implementation; macOS, Windows, and Linux/X11 use Enigo. A missing\n/// portal capability remains a clear fail-closed `UnsupportedBackend`, never an\n/// XWayland/uinput fallback.\nfn production_backend() -> Arc<dyn DesktopInputBackend> {\n    #[cfg(target_os = "linux")]\n    {\n        if is_wayland_session_from_env() {\n            return match wayland_portal::WaylandPortalBackend::new() {\n                Ok(backend) => Arc::new(backend),\n                Err(error) => Arc::new(UnsupportedBackend(error)),\n            };\n        }\n    }\n''',
)
replace_once(
    desktop,
    '''    #[cfg(target_os = "linux")]\n    let hint = "ensure an X11 display is reachable (DISPLAY set); Wayland sessions are not \\\n                supported yet";\n''',
    '''    #[cfg(target_os = "linux")]\n    let hint = "ensure an X11 display is reachable (DISPLAY set); Wayland is handled by the \\\n                xdg-desktop-portal backend before Enigo is constructed";\n''',
)
replace_once(
    desktop,
    '''const WAYLAND_PORTAL_MESSAGE: &str =\n    "Wayland requires an approved xdg-desktop-portal RemoteDesktop/InputCapture/libei path; \\\n     Little Monkey will not bypass compositor security";\n''',
    '''const WAYLAND_PORTAL_MESSAGE: &str =\n    "Wayland clipboard read requires an explicit xdg-desktop-portal Clipboard grant; \\\n     Little Monkey will not fall back to compositor-bypassing clipboard access";\n''',
)
replace_once(
    desktop,
    '''    #[cfg(target_os = "linux")]\n    {\n        if is_wayland_session_from_env() {\n            return Err(WAYLAND_PORTAL_MESSAGE.to_string());\n        }\n        let bytes = run_native_command("python3", &["-c", LINUX_ATSPI_SCRIPT])?;\n        let mut snapshot: NativeSnapshot = serde_json::from_slice(&bytes)\n            .map_err(|error| format!("Linux AT-SPI returned invalid data: {error}"))?;\n        normalize_linux_window_ids(&mut snapshot);\n        return Ok(snapshot);\n    }\n''',
    '''    #[cfg(target_os = "linux")]\n    {\n        // AT-SPI is display-server independent and remains the semantic source\n        // of truth on both X11 and Wayland. Only X11 needs wmctrl normalization.\n        let bytes = run_native_command("python3", &["-c", LINUX_ATSPI_SCRIPT])?;\n        let mut snapshot: NativeSnapshot = serde_json::from_slice(&bytes)\n            .map_err(|error| format!("Linux AT-SPI returned invalid data: {error}"))?;\n        if !is_wayland_session_from_env() {\n            normalize_linux_window_ids(&mut snapshot);\n        }\n        return Ok(snapshot);\n    }\n''',
)
replace_once(
    desktop,
    '''        #[cfg(target_os = "linux")]\n        {\n            if is_wayland_session_from_env() {\n                return Err(WAYLAND_PORTAL_MESSAGE.to_string());\n            }\n            return Command::new("wmctrl")\n''',
    '''        #[cfg(target_os = "linux")]\n        {\n            if is_wayland_session_from_env() {\n                let provider_window_id = target\n                    .provider_window_id\n                    .as_deref()\n                    .unwrap_or(&target.window_id);\n                let index = window_index(provider_window_id)?;\n                let bytes = run_native_command_with_env(\n                    "python3",\n                    &["-c", LINUX_ATSPI_FOCUS_SCRIPT],\n                    &[\n                        ("LM_APP_NAME", target.application_name.clone()),\n                        ("LM_WINDOW_INDEX", index.to_string()),\n                    ],\n                )?;\n                if serde_json::from_slice::<serde_json::Value>(&bytes)\n                    .ok()\n                    .and_then(|json| json.get("focused").and_then(serde_json::Value::as_bool))\n                    == Some(true)\n                {\n                    return Ok(());\n                }\n                return Err("Linux AT-SPI did not confirm the requested Wayland window focus".to_string());\n            }\n            return Command::new("wmctrl")\n''',
)
# Add the Wayland AT-SPI focus helper next to the existing Linux semantic action helper.
replace_once(
    desktop,
    '#[cfg(target_os = "linux")]\nconst LINUX_ATSPI_ACTION_SCRIPT: &str = r#"\n',
    '''#[cfg(target_os = "linux")]\nconst LINUX_ATSPI_FOCUS_SCRIPT: &str = r#"\nimport os, json, time\nimport pyatspi\napp_name=os.environ['LM_APP_NAME']; wi=int(os.environ['LM_WINDOW_INDEX'])\na=None\nfor candidate in list(pyatspi.Registry.getDesktop(0)):\n if str(getattr(candidate,'name','')) == app_name: a=candidate; break\nif a is None: raise SystemExit('AT-SPI application is stale')\nwindows=list(a)\nif wi < 0 or wi >= len(windows): raise SystemExit('AT-SPI window is stale')\nw=windows[wi]; attempted=False\ntry:\n w.queryComponent().grabFocus(); attempted=True\nexcept Exception:\n pass\ntry:\n actions=w.queryAction()\n for i in range(actions.nActions):\n  name=(actions.getName(i) or '').lower()\n  if name in ('activate','raise','focus'):\n   actions.doAction(i); attempted=True; break\nexcept Exception:\n pass\nfocused=False\nfor _ in range(10):\n try:\n  state=w.getState()\n  focused=state.contains(pyatspi.STATE_ACTIVE) or state.contains(pyatspi.STATE_FOCUSED)\n except Exception:\n  focused=False\n if focused: break\n time.sleep(0.05)\nprint(json.dumps({'focused':bool(focused and attempted)},separators=(',',':')))\n"#;\n\n#[cfg(target_os = "linux")]\nconst LINUX_ATSPI_ACTION_SCRIPT: &str = r#"\n''',
)
# Semantic AT-SPI actions are valid on Wayland; remove the old display-server rejection.
replace_once(
    desktop,
    '''    #[cfg(target_os = "linux")]\n    {\n        if is_wayland_session_from_env() {\n            return Err(WAYLAND_PORTAL_MESSAGE.to_string());\n        }\n        let provider_window_id = target\n''',
    '''    #[cfg(target_os = "linux")]\n    {\n        let provider_window_id = target\n''',
)
# Wayland screenshots are full verified active-window captures only. Never widen to display capture.
needle = '''        if target.bounds.width > 0.0\n            && (requested.x < target.bounds.x\n                || requested.y < target.bounds.y\n                || requested.x + requested.width > target.bounds.x + target.bounds.width\n                || requested.y + requested.height > target.bounds.y + target.bounds.height)\n        {\n            return Err("Screenshot region is outside the verified target bounds".to_string());\n        }\n'''
replacement = needle + '''        #[cfg(target_os = "linux")]\n        if is_wayland_session_from_env() {\n            let full_target = (requested.x - target.bounds.x).abs() < 0.5\n                && (requested.y - target.bounds.y).abs() < 0.5\n                && (requested.width - target.bounds.width).abs() < 0.5\n                && (requested.height - target.bounds.height).abs() < 0.5;\n            if !full_target {\n                return Err(\n                    "WAYLAND_CAPABILITY_UNAVAILABLE: Wayland screenshot subregions are refused; request the full verified target window"\n                        .to_string(),\n                );\n            }\n            let before = checked_target(\n                native_snapshot()?,\n                &target.application_id,\n                Some(&target.window_id),\n                true,\n            )?;\n            let bytes = wayland_portal::screenshot_active_window()?;\n            // Re-check after capture as well. If focus changed during the portal\n            // request, discard the image rather than returning ambiguous pixels.\n            let after = checked_target(\n                native_snapshot()?,\n                &target.application_id,\n                Some(&target.window_id),\n                true,\n            )?;\n            if before.target_id != after.target_id {\n                return Err("Target changed while the Wayland screenshot was captured".to_string());\n            }\n            return Ok((bytes, after.bounds));\n        }\n'''
replace_once(desktop, needle, replacement)
# The later Linux screenshot branch is now X11-only by construction.
replace_once(
    desktop,
    '''        #[cfg(target_os = "linux")]\n        let result = {\n            if is_wayland_session_from_env() {\n                return Err(WAYLAND_PORTAL_MESSAGE.to_string());\n            }\n            let geometry = format!("{x},{y} {width}x{height}");\n''',
    '''        #[cfg(target_os = "linux")]\n        let result = {\n            let geometry = format!("{x},{y} {width}x{height}");\n''',
)
# Replace the obsolete test that asserted Wayland was unsupported.
replace_once(
    desktop,
    '''    #[test]\n    fn wayland_unsupported_message_is_clear_about_x11_working() {\n        assert!(WAYLAND_UNSUPPORTED_MESSAGE.contains("Wayland"));\n        assert!(WAYLAND_UNSUPPORTED_MESSAGE.contains("X11 sessions work today"));\n    }\n''',
    '''    #[test]\n    fn wayland_clipboard_failure_remains_explicit_and_fail_closed() {\n        assert!(WAYLAND_PORTAL_MESSAGE.contains("Wayland clipboard"));\n        assert!(WAYLAND_PORTAL_MESSAGE.contains("xdg-desktop-portal"));\n        assert!(WAYLAND_PORTAL_MESSAGE.contains("will not fall back"));\n    }\n''',
)

# zbus is already present transitively in Cargo.lock; make it an explicit dependency
# because the Wayland production backend now imports its public API directly.
replace_once(
    cargo,
    'tokio = { version = "1.52.3", features = ["full"] }\n',
    'tokio = { version = "1.52.3", features = ["full"] }\nzbus = "5.17.0"\n',
)

# Attested drag/hotkey must hit the portal's atomic command implementations.
text = wayland.read_text()
text = text.replace(
    'use super::{DesktopInputBackend, MouseButtonKind};',
    'use super::{ComputerUseFailure, DesktopInputBackend, MouseButtonKind, ProviderExecutionFailure};',
    1,
)
text = text.replace(
    '''    fn hotkey(&self, keys: &[String]) -> Result<(), String> {\n        self.request(InputCommand::Hotkey(keys.to_vec()))\n    }\n''',
    '''    fn hotkey(&self, keys: &[String]) -> Result<(), String> {\n        self.request(InputCommand::Hotkey(keys.to_vec()))\n    }\n\n    fn drag_attested(\n        &self,\n        from_x: i32,\n        from_y: i32,\n        to_x: i32,\n        to_y: i32,\n    ) -> Result<(), ProviderExecutionFailure> {\n        self.request(InputCommand::Drag(from_x, from_y, to_x, to_y))\n            .map_err(ComputerUseFailure::ambiguous)\n    }\n\n    fn hotkey_attested(&self, keys: &[String]) -> Result<(), ProviderExecutionFailure> {\n        self.request(InputCommand::Hotkey(keys.to_vec()))\n            .map_err(ComputerUseFailure::ambiguous)\n    }\n''',
    1,
)
wayland.write_text(text)
