//! Single source of truth for the app's on-disk data-directory identifier.
//!
//! The desktop app resolves its app-data directory through a Tauri
//! `AppHandle` (`app.path().app_data_dir()`), which reads `tauri.conf.json`'s
//! `identifier`. `monkey-cli` has no `AppHandle` and, before this module
//! existed, independently hardcoded the exact same identifier string in
//! eight separate files (`providers_cli.rs`, `checkpoints_cli.rs`,
//! `verify_cli.rs`, `web_cli.rs`, `tools_cli.rs`, `mcp_cli.rs`,
//! `stacks_cli.rs`, `main.rs`) — a drift risk ROADMAP.md §3.5 names
//! explicitly. This is the shared, `AppHandle`-free replacement: every one of
//! those eight call sites now resolves the shared prefix through
//! [`data_dir`] and only appends its own file/subdirectory name.

use std::path::PathBuf;

/// Must match `identifier` in `src-tauri/tauri.conf.json`.
const APP_IDENTIFIER: &str = "com.littlemonkey.app";

/// The base app-data directory (e.g.
/// `~/Library/Application Support/com.littlemonkey.app` on macOS,
/// `~/.local/share/com.littlemonkey.app` on Linux). Callers join their own
/// file or subdirectory name onto this and create any subdirectory
/// themselves — this function only resolves the shared prefix, exactly like
/// every one of the eight call sites it replaces did individually before.
pub fn data_dir() -> Option<PathBuf> {
    Some(dirs::data_dir()?.join(APP_IDENTIFIER))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn data_dir_ends_with_the_app_identifier() {
        let dir = data_dir().expect("dirs::data_dir() should resolve on any supported OS");
        assert_eq!(dir.file_name().and_then(|n| n.to_str()), Some(APP_IDENTIFIER));
    }
}
