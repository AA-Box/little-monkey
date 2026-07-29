//! Bundled MCP servers — small, dependency-free stdio MCP servers whose
//! source ships inside the compiled binary (`include_str!`) rather than as a
//! separately-installed package, so a Settings "quick add" template
//! (`McpPanel.tsx`'s `APP_CONNECTOR_TEMPLATES`) can point a plain `Stdio`
//! transport (`command: "node"`) at a real file without asking the user to
//! `git clone`/`npm install`/`dxt pack` anything themselves first.
//!
//! [`mcp_stage_bundled_server`] is the one command this module exposes:
//! given a known bundled server id, it (re)writes that server's embedded
//! source to `<app_data>/bundled-mcp-servers/<id>/index.mjs` and returns the
//! absolute path, which the frontend then uses verbatim as the `Stdio`
//! transport's sole arg. Always overwrites (never checks if the file already
//! matches) so an app upgrade that changes a bundled server's source is
//! picked up the next time its template is used — mirrors
//! `stage-cli-sidecar.mjs`'s own "always rebuild/restage, never assume the
//! existing file is current" posture for the CLI sidecar binary.
//!
//! First (and, so far, only) entry: `osascript-control`, a from-scratch port
//! of k6l3/osascript-dxt's idea — one `run_applescript` tool wrapping
//! `osascript` — kept in `../mcp-servers/osascript-control/index.mjs` as a
//! real, directly-runnable file (so it can be smoke-tested with a bare
//! `node` during development) rather than an inline string literal here.
//! This module adds no execution or approval logic of its own: the staged
//! server's tool calls flow through the exact same `mcp_call_tool`/
//! `permissions.rs` gate every other MCP server's tools do — see that
//! server's own file-header comment for why no extra gating is layered on
//! top here.

const OSASCRIPT_CONTROL_SOURCE: &str =
    include_str!("../mcp-servers/osascript-control/index.mjs");

/// Looks up a known bundled server id's embedded source. `None` for anything
/// unrecognized — callers turn that into a clear error rather than ever
/// writing an arbitrary/unknown id's content to disk.
fn bundled_source(id: &str) -> Option<&'static str> {
    match id {
        "osascript-control" => Some(OSASCRIPT_CONTROL_SOURCE),
        _ => None,
    }
}

/// Core staging logic behind [`mcp_stage_bundled_server`], parameterized by
/// the app-data directory for testability. Writes `<data_dir>/
/// bundled-mcp-servers/<id>/index.mjs` (creating parent directories as
/// needed) and returns its absolute path as a string.
pub fn stage_bundled_server_impl(data_dir: &std::path::Path, id: &str) -> Result<String, String> {
    let source = bundled_source(id).ok_or_else(|| format!("Unknown bundled MCP server '{id}'"))?;

    let dir = data_dir.join("bundled-mcp-servers").join(id);
    std::fs::create_dir_all(&dir)
        .map_err(|e| format!("Failed to create bundled MCP server directory: {e}"))?;
    let path = dir.join("index.mjs");
    std::fs::write(&path, source)
        .map_err(|e| format!("Failed to write bundled MCP server '{id}': {e}"))?;

    path.into_os_string()
        .into_string()
        .map_err(|_| "Bundled MCP server path is not valid UTF-8".to_string())
}

/// Materializes a known bundled MCP server's source under the app data
/// directory and returns its absolute path, ready to use as a `Stdio`
/// transport's sole `node` argument.
#[tauri::command]
pub fn mcp_stage_bundled_server(id: String) -> Result<String, String> {
    let data_dir = crate::app_paths::data_dir()
        .ok_or_else(|| "Failed to resolve app data dir".to_string())?;
    stage_bundled_server_impl(&data_dir, &id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn temp_data_dir() -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "little_monkey_bundled_mcp_test_{}_{}_{}",
            std::process::id(),
            n,
            nanos
        ))
    }

    #[test]
    fn stages_the_osascript_control_server_with_its_exact_embedded_source() {
        let data_dir = temp_data_dir();
        let path = stage_bundled_server_impl(&data_dir, "osascript-control").unwrap();
        assert!(path.ends_with("bundled-mcp-servers/osascript-control/index.mjs"));
        let written = std::fs::read_to_string(&path).unwrap();
        assert_eq!(written, OSASCRIPT_CONTROL_SOURCE);
        let _ = std::fs::remove_dir_all(&data_dir);
    }

    #[test]
    fn unknown_bundled_server_id_is_rejected_before_touching_disk() {
        let data_dir = temp_data_dir();
        let error = stage_bundled_server_impl(&data_dir, "does-not-exist").unwrap_err();
        assert!(error.contains("Unknown bundled MCP server"));
        assert!(!data_dir.exists(), "an unknown id must never create the data dir");
    }

    #[test]
    fn restaging_overwrites_rather_than_erroring_on_an_existing_file() {
        let data_dir = temp_data_dir();
        let first = stage_bundled_server_impl(&data_dir, "osascript-control").unwrap();
        let second = stage_bundled_server_impl(&data_dir, "osascript-control").unwrap();
        assert_eq!(first, second);
        assert!(std::path::Path::new(&second).exists());
        let _ = std::fs::remove_dir_all(&data_dir);
    }
}
