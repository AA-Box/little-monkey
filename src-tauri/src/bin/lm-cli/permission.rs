//! Terminal-driven permission gate. Mirrors `src-tauri/src/permissions.rs`'s
//! mode semantics (manual/acceptEdits/auto/bypass — "plan" is GUI-only,
//! since there's no chat surface here to describe a plan into), but prompts
//! on stdin/stdout instead of emitting a `permission://request` event for a
//! modal to answer — there is no window here to emit to.

use std::collections::HashSet;
use std::io::Write;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionMode {
    Manual,
    AcceptEdits,
    Auto,
    Bypass,
}

impl PermissionMode {
    pub fn parse(s: &str) -> Result<Self, String> {
        match s {
            "manual" => Ok(Self::Manual),
            "acceptEdits" => Ok(Self::AcceptEdits),
            "auto" => Ok(Self::Auto),
            "bypass" => Ok(Self::Bypass),
            other => Err(format!(
                "Unknown permission mode '{other}' (expected manual, acceptEdits, auto, or bypass)"
            )),
        }
    }
}

pub struct TerminalPermissions {
    mode: PermissionMode,
    session_allow: HashSet<String>,
}

impl TerminalPermissions {
    pub fn new(mode: PermissionMode) -> Self {
        Self {
            mode,
            session_allow: HashSet::new(),
        }
    }

    /// Ask for permission to run `tool` (human-readable `detail` describing
    /// exactly what will happen). Blocks on stdin — fine for a CLI, unlike
    /// the GUI's async oneshot-channel wait for a modal click.
    pub async fn request(&mut self, tool: &str, detail: &str) -> Result<(), String> {
        match self.mode {
            PermissionMode::Bypass => return Ok(()),
            // `run_shell` is never auto-approved outside of bypass — same
            // rule as the GUI's `permissions.rs::mode_short_circuit` — so
            // both edit-approving modes behave identically here.
            PermissionMode::AcceptEdits | PermissionMode::Auto => {
                if tool == "write_file" || tool == "edit_file" {
                    return Ok(());
                }
            }
            PermissionMode::Manual => {}
        }

        // `run_shell` is never remembered for the session — its blast
        // radius (arbitrary shell execution) is too large to silently
        // pre-authorize off the back of a single approval. Same rule as
        // the GUI's `NO_SESSION_REMEMBER`.
        if tool != "run_shell" && self.session_allow.contains(tool) {
            return Ok(());
        }

        println!("\n--- Permission requested: {tool} ---\n{detail}\n");
        let remember_hint = if tool == "run_shell" { "" } else { " / [s]ession" };
        print!("Allow? [y]es / [N]o{remember_hint}: ");
        std::io::stdout().flush().ok();

        let answer = read_line_blocking().await.trim().to_lowercase();

        match answer.as_str() {
            "y" | "yes" => Ok(()),
            "s" | "session" if tool != "run_shell" => {
                self.session_allow.insert(tool.to_string());
                Ok(())
            }
            _ => Err("Permission denied".to_string()),
        }
    }
}

async fn read_line_blocking() -> String {
    tokio::task::spawn_blocking(|| {
        let mut line = String::new();
        std::io::stdin().read_line(&mut line).ok();
        line
    })
    .await
    .unwrap_or_default()
}
