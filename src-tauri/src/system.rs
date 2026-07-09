//! OS-level actions surfaced from the folder/branch badges in the git status
//! bar: revealing the workspace folder in the file manager, opening a
//! terminal at it. Like git.rs, these are direct human-initiated UI actions,
//! not agent tools, so they bypass the permission system in `permissions.rs`.

use std::path::Path;
use std::process::Command;

use tauri::{AppHandle, LogicalPosition, LogicalSize, Manager, WebviewUrl, WebviewWindowBuilder};

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
/// Split view" / "New window" actions; `tile` is only set for "Split view",
/// which additionally resizes/repositions the invoking window to the left
/// half of its monitor's work area so the two sit side by side.
#[tauri::command]
pub fn open_session_window(
    app: AppHandle,
    window: tauri::WebviewWindow,
    session_id: String,
    tile: bool,
) -> Result<(), String> {
    let label = format!("session-{session_id}");

    if let Some(existing) = app.get_webview_window(&label) {
        return existing.set_focus().map_err(|e| e.to_string());
    }

    let url = WebviewUrl::App(format!("index.html?session={session_id}").into());
    let mut builder = WebviewWindowBuilder::new(&app, label, url)
        .title("Little Monkey")
        .inner_size(1280.0, 800.0);

    // Same chrome as the main window (tauri.conf.json): traffic lights
    // overlay the webview so the sidebar/workspace panels reach the top
    // instead of sitting under a native title bar.
    #[cfg(target_os = "macos")]
    {
        builder = builder
            .title_bar_style(tauri::TitleBarStyle::Overlay)
            .hidden_title(true);
    }

    if tile {
        // Tile against the window the menu was opened from (not always
        // "main" — the action is also available inside session windows),
        // and against the monitor's work area, not its full size: on macOS
        // the full size includes the menu bar, so positioning at the
        // monitor origin gets clamped below it while the height still
        // assumes the full screen — both windows end up spilling past the
        // bottom and the "split" reads as just another floating window.
        if let Ok(Some(monitor)) = window.current_monitor() {
            let scale = monitor.scale_factor();
            let area = monitor.work_area();
            let size = area.size.to_logical::<f64>(scale);
            let pos = area.position.to_logical::<f64>(scale);
            let half_width = (size.width / 2.0).floor();

            let _ = window.unmaximize();
            let _ = window.set_position(LogicalPosition::new(pos.x, pos.y));
            let _ = window.set_size(LogicalSize::new(half_width, size.height));

            builder = builder
                .position(pos.x + half_width, pos.y)
                .inner_size(half_width, size.height);
        }
    }

    builder.build().map_err(|e| e.to_string())?;
    Ok(())
}
