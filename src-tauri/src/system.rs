//! OS-level actions surfaced from the folder/branch badges in the git status
//! bar: revealing the workspace folder in the file manager, opening a
//! terminal at it. Like git.rs, these are direct human-initiated UI actions,
//! not agent tools, so they bypass the permission system in `permissions.rs`.

use std::path::Path;
use std::process::Command;

use tauri::{AppHandle, Manager, WebviewUrl, WebviewWindowBuilder};

/// Reveal `path` in the OS file manager (Finder on macOS, Explorer on
/// Windows, the default file manager elsewhere on Unix).
#[tauri::command]
pub fn reveal_in_finder(path: String) -> Result<(), String> {
    reveal_in_finder_impl(Path::new(&path))
}

#[cfg(target_os = "macos")]
fn reveal_in_finder_impl(path: &Path) -> Result<(), String> {
    Command::new("open")
        .arg("-R")
        .arg(path)
        .status()
        .map_err(|e| format!("Failed to open Finder: {}", e))?;
    Ok(())
}

#[cfg(target_os = "windows")]
fn reveal_in_finder_impl(path: &Path) -> Result<(), String> {
    Command::new("explorer")
        .arg(format!("/select,{}", path.display()))
        .status()
        .map_err(|e| format!("Failed to open Explorer: {}", e))?;
    Ok(())
}

#[cfg(all(unix, not(target_os = "macos")))]
fn reveal_in_finder_impl(path: &Path) -> Result<(), String> {
    Command::new("xdg-open")
        .arg(path)
        .status()
        .map_err(|e| format!("Failed to open file manager: {}", e))?;
    Ok(())
}

/// Open a new terminal window at `path`.
#[tauri::command]
pub fn open_in_terminal(path: String) -> Result<(), String> {
    open_in_terminal_impl(Path::new(&path))
}

#[cfg(target_os = "macos")]
fn open_in_terminal_impl(path: &Path) -> Result<(), String> {
    Command::new("open")
        .arg("-a")
        .arg("Terminal")
        .arg(path)
        .status()
        .map_err(|e| format!("Failed to open Terminal: {}", e))?;
    Ok(())
}

#[cfg(target_os = "windows")]
fn open_in_terminal_impl(path: &Path) -> Result<(), String> {
    Command::new("cmd")
        .args(["/C", "start", "cmd", "/K", "cd", "/d"])
        .arg(path)
        .status()
        .map_err(|e| format!("Failed to open terminal: {}", e))?;
    Ok(())
}

#[cfg(all(unix, not(target_os = "macos")))]
fn open_in_terminal_impl(path: &Path) -> Result<(), String> {
    Command::new("x-terminal-emulator")
        .current_dir(path)
        .spawn()
        .map_err(|e| format!("Failed to open terminal: {}", e))?;
    Ok(())
}

/// Open `path` in an external code editor, for the session menu's "Open in"
/// submenu. `editor` is `"cursor"` or `"vscode"` (anything else falls back
/// to VS Code).
#[tauri::command]
pub fn open_in_editor(path: String, editor: String) -> Result<(), String> {
    open_in_editor_impl(Path::new(&path), &editor)
}

#[cfg(target_os = "macos")]
fn open_in_editor_impl(path: &Path, editor: &str) -> Result<(), String> {
    let app_name = if editor == "cursor" { "Cursor" } else { "Visual Studio Code" };
    Command::new("open")
        .arg("-a")
        .arg(app_name)
        .arg(path)
        .status()
        .map_err(|e| format!("Failed to open {}: {}", app_name, e))?;
    Ok(())
}

#[cfg(target_os = "windows")]
fn open_in_editor_impl(path: &Path, editor: &str) -> Result<(), String> {
    let bin = if editor == "cursor" { "cursor" } else { "code" };
    Command::new("cmd")
        .args(["/C", bin])
        .arg(path)
        .status()
        .map_err(|e| format!("Failed to open {}: {}", bin, e))?;
    Ok(())
}

#[cfg(all(unix, not(target_os = "macos")))]
fn open_in_editor_impl(path: &Path, editor: &str) -> Result<(), String> {
    let bin = if editor == "cursor" { "cursor" } else { "code" };
    Command::new(bin)
        .arg(path)
        .spawn()
        .map_err(|e| format!("Failed to open {}: {}", bin, e))?;
    Ok(())
}

/// Open (or focus, if already open) a second app window pre-selecting
/// `session_id` (read via `?session=` in `src/main.tsx` on boot — see
/// `useSessionStore.switchSession`). Backs the session menu's "Open in >
/// New window" action ("Split view" is an in-window pane now — see
/// `openSplit` in src/store/sessionStore.ts and App.tsx).
#[tauri::command]
pub fn open_session_window(app: AppHandle, session_id: String) -> Result<(), String> {
    let label = format!("session-{session_id}");

    if let Some(existing) = app.get_webview_window(&label) {
        return existing.set_focus().map_err(|e| e.to_string());
    }

    let url = WebviewUrl::App(format!("index.html?session={session_id}").into());
    let builder = WebviewWindowBuilder::new(&app, label, url)
        .title("Little Monkey")
        .inner_size(1280.0, 800.0);

    // Same chrome as the main window (tauri.conf.json): traffic lights
    // overlay the webview so the sidebar/workspace panels reach the top
    // instead of sitting under a native title bar.
    #[cfg(target_os = "macos")]
    let builder = builder
        .title_bar_style(tauri::TitleBarStyle::Overlay)
        .hidden_title(true);

    builder.build().map_err(|e| e.to_string())?;
    Ok(())
}
