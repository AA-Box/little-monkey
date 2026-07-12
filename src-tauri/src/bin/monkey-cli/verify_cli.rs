//! Reads the same `verify_configs.json` the desktop app's Settings >
//! Verification tab writes (see `little_monkey_lib::verify`), without a
//! `tauri::AppHandle` to resolve its path through — same hardcoded-identifier
//! app-data convention `providers_cli.rs`/`checkpoints_cli.rs` use for
//! `providers.json`/the checkpoints directory. `verify::run_command_impl`
//! itself is already `AppHandle`-free and called directly by `agent.rs`/
//! `repl.rs` — this module only resolves "which commands are configured for
//! this workspace", not execution.

use std::path::{Path, PathBuf};

use little_monkey_lib::verify::{self, VerifyCommand};

fn verify_configs_path() -> Option<PathBuf> {
    Some(little_monkey_lib::app_paths::data_dir()?.join("verify_configs.json"))
}

/// Core lookup, parameterized by `configs_path` so it's directly testable
/// without touching the real OS app-data dir. Every configured command for
/// `root` (enabled or not) in configured order.
pub fn all_commands_at(configs_path: &Path, root: &Path) -> Vec<VerifyCommand> {
    verify::load_config_for_workspace(configs_path, root).commands
}

/// Core lookup, parameterized by `configs_path` — only the ENABLED commands,
/// mirroring `runVerificationPhase`'s `enabledCommands` filter on the
/// frontend (`src/lib/agentLoop.ts`).
pub fn enabled_commands_at(configs_path: &Path, root: &Path) -> Vec<VerifyCommand> {
    all_commands_at(configs_path, root).into_iter().filter(|c| c.enabled).collect()
}

/// Every configured command (enabled or not) for `root` — used by the
/// REPL's `/verify` listing. Empty if no config file, an unresolvable app
/// data dir, or nothing configured for this root.
pub fn all_commands(root: &Path) -> Vec<VerifyCommand> {
    match verify_configs_path() {
        Some(path) => all_commands_at(&path, root),
        None => Vec::new(),
    }
}

/// The enabled subset for `root` — what `agent.rs`'s automatic post-turn
/// verification phase and `/verify run` both execute.
pub fn enabled_commands(root: &Path) -> Vec<VerifyCommand> {
    match verify_configs_path() {
        Some(path) => enabled_commands_at(&path, root),
        None => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use little_monkey_lib::verify::VerifyConfig;

    fn temp_path(name: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        std::env::temp_dir().join(format!("little_monkey_verify_cli_test_{}_{}_{}", std::process::id(), nanos, name))
    }

    fn command(id: &str, enabled: bool) -> VerifyCommand {
        VerifyCommand {
            id: id.to_string(),
            label: id.to_string(),
            command: "echo hi".to_string(),
            kind: "custom".to_string(),
            enabled,
            timeout_secs: None,
        }
    }

    #[test]
    fn enabled_commands_at_filters_out_disabled_ones() {
        let path = temp_path("config.json");
        let root = Path::new("/some/workspace");
        let config = VerifyConfig { commands: vec![command("a", true), command("b", false)] };
        let mut map = std::collections::HashMap::new();
        map.insert(root.to_string_lossy().to_string(), config);
        let json = serde_json::to_string(&map).unwrap();
        std::fs::write(&path, json).unwrap();

        let all = all_commands_at(&path, root);
        assert_eq!(all.len(), 2);

        let enabled = enabled_commands_at(&path, root);
        assert_eq!(enabled.len(), 1);
        assert_eq!(enabled[0].id, "a");

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn commands_at_for_unconfigured_root_is_empty() {
        let path = temp_path("missing.json");
        assert!(all_commands_at(&path, Path::new("/nowhere")).is_empty());
        assert!(enabled_commands_at(&path, Path::new("/nowhere")).is_empty());
    }
}
