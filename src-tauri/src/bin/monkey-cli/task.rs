//! `monkey-cli task run/validate/list` — the CI-suitable headless runner for
//! saved YAML/JSON recipes (design doc: docs/roadmap/p3-scheduled-automation.md,
//! slice 1). Named `task` rather than `run` — that name is already taken by
//! the Ollama-parity `monkey-cli run <model>` (see `main.rs`'s `Cmd::Run`) —
//! leaving room for `task list`/`task validate`/(a future) `task schedule`
//! alongside `task run`.
//!
//! `task run` reuses the exact same sandboxed agent loop every other
//! `monkey-cli` invocation does (`agent::run_turn_with_max_iterations`) —
//! nothing here duplicates tool execution, permission gating, or streaming.
//! This module is purely: resolve a recipe -> render its prompt/params ->
//! build the same `AppState`/`Target`/`ChatOptions` the flat invocation
//! builds -> call the shared loop -> translate the result into an exit code
//! and (optionally) a JSON summary.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use little_monkey_lib::recipes::{self, Recipe};

use crate::chat::{self, Target};
use crate::permission::{PermissionMode, TerminalPermissions};

/// Exit codes for `task run` (design doc slice 1) — deterministic and
/// CI-parseable, distinct from the generic `fail()` (always 1) every other
/// `monkey-cli` subcommand uses on error.
pub const EXIT_OK: i32 = 0;
pub const EXIT_CONFIG_ERROR: i32 = 1;
pub const EXIT_PERMISSION_DENIED: i32 = 2;
pub const EXIT_TIMEOUT: i32 = 3;

/// Parses `key=value` `--param` flags into a map. A malformed entry (no `=`,
/// or an empty key) is a config error, never silently dropped — the same
/// "typo protection over silent leniency" stance `recipes::resolve_param_values`
/// takes for unknown keys.
pub fn parse_param_flags(raw: &[String]) -> Result<HashMap<String, String>, String> {
    let mut map = HashMap::new();
    for entry in raw {
        let Some((k, v)) = entry.split_once('=') else {
            return Err(format!("--param '{entry}' must be in key=value form"));
        };
        if k.is_empty() {
            return Err(format!("--param '{entry}' has an empty key"));
        }
        map.insert(k.to_string(), v.to_string());
    }
    Ok(map)
}

/// Bridges a recipe's own `RecipeTarget` (a shared-lib, parsed-from-YAML
/// type) into `monkey-cli`'s `chat::Target` (resolved against live
/// provider/keychain state) — mirrors `main.rs::resolve_target`'s exact XOR
/// logic, just reading from a `Recipe` instead of CLI flags.
fn resolve_chat_target(target: &recipes::RecipeTarget) -> Result<Target, String> {
    if let Some(provider) = &target.provider {
        let model = target.model.clone().ok_or("recipe target with 'provider' must also set 'model'")?;
        return Ok(Target::Provider { provider_id: provider.clone(), model });
    }
    if let Some(model) = &target.ollama {
        return Ok(Target::Local { base_url: crate::ollama_api::host(), model: Some(model.clone()), native_ollama: true });
    }
    if let Some(base_url) = &target.local_url {
        return Ok(Target::Local { base_url: base_url.clone(), model: target.model.clone(), native_ollama: false });
    }
    Err("recipe target must set exactly one of provider, ollama, or local_url".to_string())
}

/// Resolves a recipe's `workspace` field against the recipe FILE's own
/// directory (not the process's cwd) when given — matching the design doc's
/// `workspace: . # resolved against recipe file dir, defaults to cwd`
/// comment exactly. Absent entirely -> the process's current directory.
fn resolve_workspace_dir(recipe: &Recipe, recipe_path: &Path) -> PathBuf {
    match &recipe.workspace {
        Some(w) => recipe_path.parent().unwrap_or_else(|| Path::new(".")).join(w),
        None => std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
    }
}

/// `task list` — prints every recipe visible from the current directory (its
/// `.littlemonkey/recipes/`, plus the global recipes directory), one per
/// line, with a `Warning:` for any file that failed to parse instead of
/// silently omitting it.
pub fn list() -> Result<(), String> {
    let app_data_dir = crate::app_data_dir().ok_or("Could not resolve the app data directory")?;
    let workspace_root = std::env::current_dir().ok();
    let found = recipes::discover_recipes(workspace_root.as_deref(), &app_data_dir);
    if found.is_empty() {
        println!("No recipes found (checked ./.littlemonkey/recipes/ and the global recipes directory).");
        return Ok(());
    }
    for d in &found {
        match &d.recipe {
            Some(r) => println!(
                "{}\t{:?}\t{}\t{}",
                r.name,
                d.source,
                r.permission_mode,
                d.path.display()
            ),
            None => eprintln!("Warning: {} failed to parse: {}", d.path.display(), d.error.as_deref().unwrap_or("unknown error")),
        }
    }
    Ok(())
}

/// `task validate <path>` — parses and validates a recipe file without
/// running it (the editor's/CI's "is this recipe well-formed" check).
pub fn validate(path: &str) -> Result<(), String> {
    let content = std::fs::read_to_string(path).map_err(|e| format!("Failed to read '{path}': {e}"))?;
    let ext = Path::new(path).extension().and_then(|e| e.to_str()).unwrap_or("yml");
    let recipe = recipes::parse_recipe(&content, ext)?;
    println!("OK: '{}' is a valid recipe (permission_mode: {}).", recipe.name, recipe.permission_mode);
    Ok(())
}

/// `task schedule <name_or_path> --cron '...'` — emits a ready-to-install
/// launchd plist (macOS) or crontab line for running this recipe on a
/// schedule via the OS's own scheduler, rather than the app daemonizing
/// itself (design doc slice 4, optional). Always prints; never installs
/// anything — the user copies the output into `launchctl`/`crontab`
/// themselves, matching every other irreversible-action boundary in this
/// codebase.
pub fn schedule(name_or_path: &str, cron: &str) -> Result<(), String> {
    little_monkey_lib::automations::validate_cron_impl(cron)?;

    let app_data_dir = crate::app_data_dir().ok_or("Could not resolve the app data directory")?;
    let workspace_root = std::env::current_dir().ok();
    let (recipe, recipe_path) = recipes::resolve_recipe_with_path(name_or_path, workspace_root.as_deref(), &app_data_dir)?;
    let recipe_abs_path = recipe_path
        .canonicalize()
        .map_err(|e| format!("Failed to resolve absolute path to '{}': {e}", recipe_path.display()))?;

    let binary_path = std::env::current_exe()
        .map_err(|e| format!("Failed to resolve monkey-cli's own binary path: {e}"))?
        .to_string_lossy()
        .to_string();
    let recipe_path_str = recipe_abs_path.to_string_lossy().to_string();
    let args = vec!["task".to_string(), "run".to_string(), recipe_path_str, "--json".to_string()];
    let label = format!("com.littlemonkey.task.{}", recipe.name);

    if cfg!(target_os = "macos") {
        match little_monkey_lib::automations::format_launchd_plist(&label, &binary_path, &args, cron) {
            Some(plist) => {
                println!("{plist}");
                eprintln!(
                    "\n# Save the above as ~/Library/LaunchAgents/{label}.plist, then run:\n#   launchctl load ~/Library/LaunchAgents/{label}.plist\n# To remove it later: launchctl unload ~/Library/LaunchAgents/{label}.plist"
                );
            }
            None => {
                eprintln!(
                    "# '{cron}' uses cron syntax launchd can't express directly (ranges/lists/steps) — falling back to a crontab line instead:"
                );
                println!("{}", little_monkey_lib::automations::format_crontab_line(cron, &binary_path, &args));
            }
        }
    } else {
        println!("{}", little_monkey_lib::automations::format_crontab_line(cron, &binary_path, &args));
        eprintln!("\n# Add the above line via `crontab -e`.");
    }

    Ok(())
}

/// One `task run` result — the `--json` output shape (design doc slice 1):
/// `{name, status, iterations_capped, final_message, files_changed}`.
#[derive(serde::Serialize)]
struct RunResult {
    name: String,
    status: &'static str,
    iterations_capped: bool,
    final_message: Option<String>,
    files_changed: Vec<String>,
}

/// Runs `name_or_path` headlessly and returns the process exit code (design
/// doc slice 1: 0 success, 1 config/transport error, 2 permission-denied or
/// plan-blocked, 3 timeout/max-iterations). Streamed tokens go to stdout in
/// non-JSON mode (matching every other `monkey-cli` invocation) but to
/// stderr when `json_output` is set, so stdout stays a single parseable
/// result object — see `chat::stream_turn`'s printing, which already writes
/// content to stdout unconditionally; `json_output` instead suppresses it by
/// routing through a quiet options flag below.
pub async fn run(cli: &crate::Cli, client: &reqwest::Client, name_or_path: &str, param_flags: &[String], json_output: bool) -> i32 {
    match run_inner(cli, client, name_or_path, param_flags, json_output).await {
        Ok((code, result)) => {
            if json_output {
                println!("{}", serde_json::to_string(&result).unwrap_or_else(|_| "{}".to_string()));
            }
            code
        }
        Err(e) => {
            if json_output {
                let result = RunResult {
                    name: name_or_path.to_string(),
                    status: "error",
                    iterations_capped: false,
                    final_message: Some(e.clone()),
                    files_changed: Vec::new(),
                };
                println!("{}", serde_json::to_string(&result).unwrap_or_else(|_| "{}".to_string()));
            } else {
                eprintln!("Error: {e}");
            }
            classify_error_exit_code(&e)
        }
    }
}

/// Classifies a `run_turn`-style error string into an exit code — permission
/// denials and Plan Mode blocks (see `permission.rs`'s `non_interactive_denial`/
/// `mode_short_circuit`) are exit 2, everything else is a generic exit 1.
/// String-matched rather than a typed error enum, consistent with the rest
/// of this codebase's `Result<_, String>` convention throughout the agent
/// loop — a known, documented limitation rather than an oversight.
fn classify_error_exit_code(message: &str) -> i32 {
    if message.contains("Permission denied") || message.starts_with("Blocked:") {
        EXIT_PERMISSION_DENIED
    } else {
        EXIT_CONFIG_ERROR
    }
}

async fn run_inner(
    cli: &crate::Cli,
    client: &reqwest::Client,
    name_or_path: &str,
    param_flags: &[String],
    json_output: bool,
) -> Result<(i32, RunResult), String> {
    let app_data_dir = crate::app_data_dir().ok_or("Could not resolve the app data directory")?;
    let workspace_root = std::env::current_dir().ok();
    let (recipe, recipe_path) = recipes::resolve_recipe_with_path(name_or_path, workspace_root.as_deref(), &app_data_dir)?;

    let overrides = parse_param_flags(param_flags)?;
    let rendered = recipes::render_recipe(&recipe, &overrides)?;

    let target = resolve_chat_target(&recipe.target)?;
    let mode = PermissionMode::parse(&recipe.permission_mode)?;

    // Fail fast, before any network/model work: a headless recipe run has no
    // one to answer a `manual`-mode prompt — same fail-closed reasoning as
    // `permission.rs`'s non-TTY guard, just surfaced earlier with a message
    // naming the actual problem (the recipe's own declared mode) rather than
    // a generic "no TTY" error the first time it tries to prompt.
    if mode == PermissionMode::Manual {
        return Err(
            "recipe's permission_mode 'manual' would wait for a prompt no one can answer in a headless run — use acceptEdits, smart, auto, bypass, or plan instead"
                .to_string(),
        );
    }

    let workspace_dir = resolve_workspace_dir(&recipe, &recipe_path);
    let state = crate::build_state(&Some(workspace_dir))?;

    let mut options = chat::ChatOptions { system: rendered.system, ..Default::default() };
    options.system = crate::effective_system(cli, &state, options.system.as_deref());

    let mcp_entries = crate::resolve_mcp_entries(cli, &state).await;

    let mut perms = TerminalPermissions::new(mode);
    let mut history: Vec<serde_json::Value> = Vec::new();

    let turn_future = crate::agent::run_turn_with_max_iterations(
        client,
        &target,
        &state,
        &mut perms,
        &mut history,
        &options,
        &rendered.prompt,
        &mcp_entries,
        &[],
        recipe.max_iterations,
    );

    let turn_result = match recipe.timeout_seconds {
        Some(secs) => match tokio::time::timeout(Duration::from_secs(secs), turn_future).await {
            Ok(inner) => inner,
            Err(_elapsed) => {
                let result = RunResult {
                    name: recipe.name.clone(),
                    status: "timeout",
                    iterations_capped: false,
                    final_message: Some(format!("Timed out after {secs}s")),
                    files_changed: Vec::new(),
                };
                return Ok((EXIT_TIMEOUT, result));
            }
        },
        None => turn_future.await,
    };

    let files_changed = turn_result.map_err(|e| e)?;
    let iterations_capped = history
        .last()
        .and_then(|m| m.get("content"))
        .and_then(|c| c.as_str())
        .map(|s| s.starts_with(crate::agent::ITERATION_CAP_MESSAGE_PREFIX))
        .unwrap_or(false);
    let final_message = history.last().and_then(|m| m.get("content")).and_then(|c| c.as_str()).map(str::to_string);

    let _ = json_output; // streaming is already printed by chat::stream_turn regardless of mode; nothing extra to suppress here yet.

    let result = RunResult {
        name: recipe.name,
        status: if iterations_capped { "incomplete" } else { "ok" },
        iterations_capped,
        final_message,
        files_changed,
    };
    let code = if iterations_capped { EXIT_TIMEOUT } else { EXIT_OK };
    Ok((code, result))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_param_flags_parses_key_value_pairs() {
        let map = parse_param_flags(&["manifest=package.json".to_string(), "count=3".to_string()]).unwrap();
        assert_eq!(map.get("manifest"), Some(&"package.json".to_string()));
        assert_eq!(map.get("count"), Some(&"3".to_string()));
    }

    #[test]
    fn parse_param_flags_rejects_an_entry_with_no_equals_sign() {
        assert!(parse_param_flags(&["justakey".to_string()]).is_err());
    }

    #[test]
    fn parse_param_flags_rejects_an_empty_key() {
        assert!(parse_param_flags(&["=value".to_string()]).is_err());
    }

    #[test]
    fn parse_param_flags_allows_a_value_containing_an_equals_sign() {
        let map = parse_param_flags(&["url=http://x?a=b".to_string()]).unwrap();
        assert_eq!(map.get("url"), Some(&"http://x?a=b".to_string()));
    }

    fn ollama_target() -> recipes::RecipeTarget {
        recipes::RecipeTarget { provider: None, model: None, ollama: Some("qwen2.5:14b".to_string()), local_url: None }
    }

    #[test]
    fn resolve_chat_target_maps_ollama_to_a_native_local_target() {
        let target = resolve_chat_target(&ollama_target()).unwrap();
        match target {
            Target::Local { model, native_ollama, .. } => {
                assert_eq!(model.as_deref(), Some("qwen2.5:14b"));
                assert!(native_ollama);
            }
            _ => panic!("expected a Local target"),
        }
    }

    #[test]
    fn resolve_chat_target_maps_provider_plus_model() {
        let target = recipes::RecipeTarget {
            provider: Some("openrouter".to_string()),
            model: Some("anthropic/claude-sonnet".to_string()),
            ollama: None,
            local_url: None,
        };
        let resolved = resolve_chat_target(&target).unwrap();
        match resolved {
            Target::Provider { provider_id, model } => {
                assert_eq!(provider_id, "openrouter");
                assert_eq!(model, "anthropic/claude-sonnet");
            }
            _ => panic!("expected a Provider target"),
        }
    }

    #[test]
    fn resolve_chat_target_maps_local_url_to_a_non_native_local_target() {
        let target = recipes::RecipeTarget { provider: None, model: None, ollama: None, local_url: Some("http://127.0.0.1:8090".to_string()) };
        let resolved = resolve_chat_target(&target).unwrap();
        match resolved {
            Target::Local { base_url, native_ollama, .. } => {
                assert_eq!(base_url, "http://127.0.0.1:8090");
                assert!(!native_ollama);
            }
            _ => panic!("expected a Local target"),
        }
    }

    fn recipe_with_workspace(workspace: Option<&str>) -> Recipe {
        Recipe {
            version: 1,
            name: "x".to_string(),
            description: None,
            target: ollama_target(),
            workspace: workspace.map(str::to_string),
            permission_mode: "manual".to_string(),
            system: None,
            prompt: "p".to_string(),
            params: HashMap::new(),
            max_iterations: None,
            timeout_seconds: None,
            output: recipes::RecipeOutput::default(),
        }
    }

    #[test]
    fn resolve_workspace_dir_resolves_against_the_recipe_files_directory_when_given() {
        let recipe = recipe_with_workspace(Some("."));
        let recipe_path = Path::new("/some/repo/.littlemonkey/recipes/r.yml");
        let dir = resolve_workspace_dir(&recipe, recipe_path);
        assert_eq!(dir, PathBuf::from("/some/repo/.littlemonkey/recipes/."));
    }

    #[test]
    fn resolve_workspace_dir_joins_a_relative_subpath_against_the_recipe_files_directory() {
        let recipe = recipe_with_workspace(Some("../.."));
        let recipe_path = Path::new("/some/repo/.littlemonkey/recipes/r.yml");
        let dir = resolve_workspace_dir(&recipe, recipe_path);
        assert_eq!(dir, PathBuf::from("/some/repo/.littlemonkey/recipes/../.."));
    }

    #[test]
    fn classify_error_exit_code_maps_permission_denied_to_exit_2() {
        assert_eq!(classify_error_exit_code("Permission denied"), EXIT_PERMISSION_DENIED);
        assert_eq!(classify_error_exit_code("Permission denied: write_file requires..."), EXIT_PERMISSION_DENIED);
    }

    #[test]
    fn classify_error_exit_code_maps_plan_mode_block_to_exit_2() {
        assert_eq!(classify_error_exit_code("Blocked: monkey-cli is in Plan Mode. ..."), EXIT_PERMISSION_DENIED);
    }

    #[test]
    fn classify_error_exit_code_maps_everything_else_to_exit_1() {
        assert_eq!(classify_error_exit_code("Failed to connect to the model"), EXIT_CONFIG_ERROR);
    }
}
