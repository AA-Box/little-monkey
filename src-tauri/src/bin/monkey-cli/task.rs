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
use std::sync::Arc;
use std::time::Duration;

use little_monkey_lib::knowledge_core::KnowledgeStack;
use little_monkey_lib::mcp::McpServerEntry;
use little_monkey_lib::recipes::{self, DesktopTurnSnapshot, Recipe};
use little_monkey_lib::run_ledger::RunLedger;
use little_monkey_lib::run_protocol::{
    CapabilityAssessment, CapabilityState, ClientIdentity, ClientKind, ModelCapabilitiesSnapshot,
    ModelTargetSnapshot, PermissionMode as RunPermissionMode, PermissionPolicySnapshot, RootAccess,
    RootGrant, RunBudgets, RunEvent, RunKind, RunSpec, RunStatus, ToolPermissionRule,
    ToolPolicyDecision, WorkspaceContext, RUN_PROTOCOL_SCHEMA_VERSION,
};
use little_monkey_lib::run_scope::RunScope;
use little_monkey_lib::workspace;
use little_monkey_lib::workspace::WorkspaceRoot;

use crate::chat::{self, Target};
use crate::durable_run::{
    bounded_text, safe_protocol_id, sha256_hex, unix_time_ms, CliRunEventSink, DurableRunRecorder,
    SemanticConformanceFixture, SubmissionDisposition,
};
use crate::permission::{PermissionMode, TerminalPermissions};

const RUN_DATABASE_FILE: &str = "profile-v1.sqlite3";
const RUN_KEY_ENV: &str = "LITTLE_MONKEY_RUN_KEY";
const DEFAULT_WALL_TIME_MS: u64 = 7 * 24 * 60 * 60 * 1_000;
const DEFAULT_APPROVAL_TIMEOUT_MS: u64 = 24 * 60 * 60 * 1_000;

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
fn resolve_recipe_chat_target(recipe: &Recipe) -> Result<ResolvedTarget, String> {
    let target = &recipe.target;
    // The node's own managed runtime is not listening yet — it is started for
    // the life of this run — so it resolves to an intent rather than to an
    // origin. Checked before the desktop branch below because a placed recipe
    // never carries a `desktop_turn` (`validate_recipe` refuses both at once).
    if let Some(model_id) = &target.managed_model {
        return Ok(ResolvedTarget::ManagedModel {
            model_id: model_id.clone(),
        });
    }
    if let Some(snapshot) = &recipe.desktop_turn {
        return desktop_execution_target(target, snapshot).map(ResolvedTarget::Ready);
    }
    resolve_chat_target(target).map(ResolvedTarget::Ready)
}

/// The desktop turn's frozen execution target, checked against the recipe copy
/// it was queued with. Unchanged behaviour, split out so
/// [`resolve_recipe_chat_target`] reads as the four-way choice it now is.
fn desktop_execution_target(
    target: &recipes::RecipeTarget,
    snapshot: &DesktopTurnSnapshot,
) -> Result<Target, String> {
    match (&snapshot.target, &snapshot.execution_base_url) {
        (
            ModelTargetSnapshot::Provider {
                provider_id,
                endpoint,
                model,
                ..
            },
            None,
        ) => {
            let recipe_provider = target
                .provider
                .as_deref()
                .ok_or("desktop provider snapshot requires a provider recipe target")?;
            let recipe_model = target
                .model
                .as_deref()
                .ok_or("desktop provider snapshot requires a model")?;
            if recipe_provider != provider_id || recipe_model != model {
                return Err(
                    "desktop provider execution target differs from its frozen target".to_string(),
                );
            }
            let custom = crate::providers_cli::load_custom_providers();
            let current = little_monkey_lib::providers::resolve_base_url(recipe_provider, &custom)?
                .trim_end_matches('/')
                .to_string();
            if current != endpoint.trim_end_matches('/') {
                return Err("desktop provider endpoint changed after the turn was queued; refusing target drift".to_string());
            }
            Ok(Target::Provider {
                provider_id: recipe_provider.to_string(),
                model: recipe_model.to_string(),
            })
        }
        (ModelTargetSnapshot::Ollama { model, .. }, Some(base_url)) => {
            if target.ollama.as_deref() != Some(model.as_str()) {
                return Err(
                    "desktop Ollama execution model differs from its frozen target".to_string(),
                );
            }
            Ok(Target::Local {
                base_url: base_url.trim_end_matches('/').to_string(),
                model: Some(model.clone()),
                native_ollama: true,
            })
        }
        (ModelTargetSnapshot::ManagedLlama { model_id, .. }, Some(base_url)) => {
            if target.local_url.as_deref() != Some(base_url.as_str()) {
                return Err(
                    "desktop managed runtime origin differs from its frozen recipe".to_string(),
                );
            }
            Ok(Target::Local {
                base_url: base_url.trim_end_matches('/').to_string(),
                model: target.model.clone().or_else(|| Some(model_id.clone())),
                native_ollama: false,
            })
        }
        _ => Err("desktop execution target is incomplete".to_string()),
    }
}

/// What a recipe's target resolves to before the run starts.
///
/// Two arms because one of the four recipe targets cannot be an origin yet:
/// [`Self::ManagedModel`] names a model this machine has installed and the
/// caller starts the app's own verified `llama-server` for it, on a fresh
/// loopback port, for exactly the life of the run.
enum ResolvedTarget {
    Ready(Target),
    ManagedModel { model_id: String },
}

fn resolve_chat_target(target: &recipes::RecipeTarget) -> Result<Target, String> {
    if let Some(provider) = &target.provider {
        let model = target
            .model
            .clone()
            .ok_or("recipe target with 'provider' must also set 'model'")?;
        return Ok(Target::Provider {
            provider_id: provider.clone(),
            model,
        });
    }
    if let Some(model) = &target.ollama {
        return Ok(Target::Local {
            base_url: crate::ollama_api::host(),
            model: Some(model.clone()),
            native_ollama: true,
        });
    }
    if let Some(base_url) = &target.local_url {
        return Ok(Target::Local {
            base_url: base_url.clone(),
            model: target.model.clone(),
            native_ollama: false,
        });
    }
    Err("recipe target must set exactly one of provider, ollama, or local_url".to_string())
}

fn apply_desktop_execution_roots(
    state: &little_monkey_lib::AppState,
    snapshot: &DesktopTurnSnapshot,
) -> Result<(), String> {
    let Some(workspace) = &snapshot.workspace else {
        if !snapshot.execution_roots.is_empty() {
            return Err("desktop chat-only turns must not carry execution roots".to_string());
        }
        *state
            .workspace_roots
            .lock()
            .map_err(|_| "desktop workspace roots lock was poisoned".to_string())? = Vec::new();
        return Ok(());
    };
    let mut roots = Vec::with_capacity(snapshot.execution_roots.len());
    let mut ordered = snapshot.execution_roots.clone();
    ordered.sort_by_key(|root| !root.is_primary);
    for root in ordered {
        let grant = workspace
            .roots
            .iter()
            .find(|grant| grant.root_id == root.root_id)
            .ok_or_else(|| format!("desktop workspace grant '{}' disappeared", root.root_id))?;
        if grant.access != RootAccess::ReadWrite {
            return Err(format!(
                "daemon desktop execution currently requires a read-write grant for '{}'",
                root.canonical_path
            ));
        }
        let canonical = PathBuf::from(&root.canonical_path)
            .canonicalize()
            .map_err(|error| {
                format!(
                    "desktop workspace '{}' is unavailable: {error}",
                    root.canonical_path
                )
            })?;
        if canonical.to_string_lossy() != root.canonical_path {
            return Err(format!(
                "desktop workspace '{}' no longer resolves to its frozen canonical path",
                root.canonical_path
            ));
        }
        roots.push(WorkspaceRoot {
            id: root.root_id,
            path: canonical,
            label: root.label,
        });
    }
    *state
        .workspace_roots
        .lock()
        .map_err(|_| "desktop workspace roots lock was poisoned".to_string())? = roots;
    Ok(())
}

fn desktop_chat_options(
    generation: &recipes::DesktopGenerationSettingsSnapshot,
    tool_profile: &recipes::DesktopToolProfileSnapshot,
    frozen_system: Option<String>,
    quiet: bool,
) -> chat::ChatOptions {
    chat::ChatOptions {
        temperature: generation.temperature,
        top_p: generation.top_p,
        seed: generation.seed,
        stop: generation.stop.clone(),
        num_ctx: generation.num_ctx,
        num_predict: generation.num_predict,
        system: frozen_system,
        format: generation.format.clone(),
        think: generation.think.clone(),
        hide_thinking: generation.hide_thinking,
        keep_alive: generation.keep_alive.clone(),
        effort: generation.effort.clone(),
        verbose: false,
        attach_images: false,
        verify: tool_profile.verify_enabled,
        verify_max_rounds: Some(tool_profile.verify_max_rounds),
        subagents: tool_profile.subagents_enabled,
        memory_enabled: Some(tool_profile.memory_enabled),
        quiet,
    }
}

fn select_desktop_mcp_entries(
    frozen_servers: &[recipes::DesktopMcpServerSnapshot],
    configured: &[McpServerEntry],
) -> Result<Vec<McpServerEntry>, String> {
    let mut selected = Vec::with_capacity(frozen_servers.len());
    for frozen in frozen_servers {
        let current = configured
            .iter()
            .find(|entry| entry.id == frozen.id)
            .ok_or_else(|| {
                format!(
                    "Snapshotted MCP server '{}' was removed after the turn was queued",
                    frozen.id
                )
            })?;
        if !current.enabled {
            return Err(format!(
                "Snapshotted MCP server '{}' was disabled after the turn was queued",
                frozen.id
            ));
        }
        let current_allowlist =
            recipes::normalized_mcp_tool_allowlist(current.tool_allowlist.as_deref());
        if current_allowlist != frozen.tool_allowlist {
            return Err(format!(
                "Snapshotted MCP server '{}' tool allowlist changed after queueing",
                frozen.id
            ));
        }
        let digest = recipes::mcp_server_config_digest(current)?;
        if digest != frozen.config_sha256 {
            return Err(format!(
                "Snapshotted MCP server '{}' config changed after queueing",
                frozen.id
            ));
        }
        let mut exact = current.clone();
        exact.tool_allowlist = frozen.tool_allowlist.clone();
        selected.push(exact);
    }
    Ok(selected)
}

fn select_desktop_stack_names(
    frozen_ids: &[String],
    frozen_names: &[String],
    configured: &[KnowledgeStack],
) -> Result<Vec<String>, String> {
    if frozen_ids.len() != frozen_names.len() {
        return Err("Frozen knowledge stack ids/names differ in length".to_string());
    }
    let mut selected = Vec::with_capacity(frozen_ids.len());
    for (id, frozen_name) in frozen_ids.iter().zip(frozen_names) {
        let stack = configured
            .iter()
            .find(|stack| &stack.id == id)
            .ok_or_else(|| {
                format!("Attached knowledge stack '{id}' was removed after the turn was queued")
            })?;
        if stack.name != *frozen_name {
            return Err(format!(
                "Attached knowledge stack '{id}' was renamed after the turn was queued"
            ));
        }
        if configured.iter().any(|other| {
            other.id != stack.id && other.name.trim().eq_ignore_ascii_case(stack.name.trim())
        }) {
            return Err(format!(
                "Attached knowledge stack '{}' has an ambiguous duplicate name",
                stack.name
            ));
        }
        selected.push(frozen_name.clone());
    }
    Ok(selected)
}

async fn resolve_desktop_mcp_entries(
    state: &little_monkey_lib::AppState,
    snapshot: &DesktopTurnSnapshot,
) -> Result<Vec<McpServerEntry>, String> {
    let configured = crate::mcp_cli::load_all_servers_strict()?;
    let selected = select_desktop_mcp_entries(&snapshot.mcp_servers, &configured)?;
    crate::mcp_cli::connect_all_strict(state, &selected).await
}

fn resolve_desktop_stack_names(snapshot: &DesktopTurnSnapshot) -> Result<Vec<String>, String> {
    if snapshot.attached_stack_ids.is_empty() {
        return Ok(Vec::new());
    }
    let base =
        crate::stacks_cli::base_dir().ok_or("Could not resolve the knowledge stack directory")?;
    let configured = little_monkey_lib::knowledge_core::list_impl(&base)?;
    select_desktop_stack_names(
        &snapshot.attached_stack_ids,
        &snapshot.attached_stack_names,
        &configured,
    )
}

/// Resolves a recipe's `workspace` field against the recipe FILE's own
/// directory (not the process's cwd) when given — matching the design doc's
/// `workspace: . # resolved against recipe file dir, defaults to cwd`
/// comment exactly. Absent entirely -> the process's current directory.
fn resolve_workspace_dir(recipe: &Recipe, recipe_path: &Path) -> PathBuf {
    match &recipe.workspace {
        Some(w) => recipe_path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join(w),
        None => std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
    }
}

/// `task list` — prints every recipe visible from the current directory (its
/// `.littlemonkey/recipes/`, plus the global recipes directory), one per
/// line, with a `Warning:` for any file that failed to parse instead of
/// silently omitting it.
pub fn list() -> Result<(), String> {
    let global_config_roots = recipes::global_config_roots()?;
    let workspace_root = std::env::current_dir().ok();
    let found = recipes::discover_recipes(workspace_root.as_deref(), &global_config_roots);
    if found.is_empty() {
        println!(
            "No recipes found (checked ./.littlemonkey/recipes/ and the global recipes directory)."
        );
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
            None => eprintln!(
                "Warning: {} failed to parse: {}",
                d.path.display(),
                d.error.as_deref().unwrap_or("unknown error")
            ),
        }
    }
    Ok(())
}

/// `task validate <path>` — parses and validates a recipe file without
/// running it (the editor's/CI's "is this recipe well-formed" check).
pub fn validate(path: &str) -> Result<(), String> {
    let content =
        std::fs::read_to_string(path).map_err(|e| format!("Failed to read '{path}': {e}"))?;
    let ext = Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("yml");
    let recipe = recipes::parse_recipe(&content, ext)?;
    println!(
        "OK: '{}' is a valid recipe (permission_mode: {}).",
        recipe.name, recipe.permission_mode
    );
    Ok(())
}

/// Compare a fixture containing desktop and CLI envelope arrays after
/// removing ids/timestamps/emitter metadata and coalescing model deltas.
/// Prints the normalized report for CI artifacts and fails when the first
/// real semantic difference is found.
pub fn conformance(path: &str) -> Result<(), String> {
    let content = std::fs::read_to_string(path)
        .map_err(|error| format!("Failed to read conformance fixture '{path}': {error}"))?;
    let fixture: SemanticConformanceFixture = serde_json::from_str(&content)
        .map_err(|error| format!("Invalid conformance fixture '{path}': {error}"))?;
    for (surface, events) in [("desktop", &fixture.desktop), ("cli", &fixture.cli)] {
        let expected_run_id = events.first().map(|event| event.run_id.as_str());
        for (index, event) in events.iter().enumerate() {
            event.validate().map_err(|error| {
                format!("{surface} event '{}' is invalid: {error}", event.event_id)
            })?;
            let expected_sequence = u64::try_from(index + 1)
                .map_err(|_| format!("{surface} fixture contains too many events"))?;
            if event.sequence != expected_sequence {
                return Err(format!(
                    "{surface} event '{}' has sequence {}, expected {expected_sequence}",
                    event.event_id, event.sequence
                ));
            }
            if Some(event.run_id.as_str()) != expected_run_id {
                return Err(format!(
                    "{surface} fixture mixes multiple run ids at event '{}'",
                    event.event_id
                ));
            }
        }
    }
    let report = fixture.compare();
    println!(
        "{}",
        serde_json::to_string_pretty(&report)
            .map_err(|error| format!("Failed to serialize conformance report: {error}"))?
    );
    if report.matches {
        Ok(())
    } else {
        Err(format!(
            "desktop and CLI streams differ at normalized event {}",
            report.first_difference.unwrap_or(0)
        ))
    }
}

fn schedule_command_args(
    agent_home: &Path,
    binary_path: &Path,
    profile_id: &str,
    recipe_path: &Path,
) -> Result<Vec<String>, String> {
    let agent_home = agent_home
        .to_str()
        .ok_or_else(|| "The Little Monkey agent home is not valid UTF-8".to_string())?;
    let binary_path = binary_path
        .to_str()
        .ok_or_else(|| "The monkey executable path is not valid UTF-8".to_string())?;
    let recipe_path = recipe_path
        .to_str()
        .ok_or_else(|| "The recipe path is not valid UTF-8".to_string())?;
    Ok(vec![
        format!(
            "{}={agent_home}",
            little_monkey_lib::app_paths::AGENT_HOME_ENV
        ),
        binary_path.to_string(),
        "--profile".to_string(),
        profile_id.to_string(),
        "task".to_string(),
        "run".to_string(),
        recipe_path.to_string(),
        "--json".to_string(),
    ])
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

    let config_roots = little_monkey_lib::app_paths::agent_config_roots()?;
    let global_config_roots = config_roots.ordered();
    let workspace_root = std::env::current_dir().ok();
    let (recipe, recipe_path) = recipes::resolve_recipe_with_path(
        name_or_path,
        workspace_root.as_deref(),
        &global_config_roots,
    )?;
    let recipe_abs_path = recipe_path.canonicalize().map_err(|e| {
        format!(
            "Failed to resolve absolute path to '{}': {e}",
            recipe_path.display()
        )
    })?;

    let binary_path = std::env::current_exe()
        .map_err(|e| format!("Failed to resolve monkey's own binary path: {e}"))?;
    let args = schedule_command_args(
        &config_roots.agent_home,
        &binary_path,
        &config_roots.profile_id,
        &recipe_abs_path,
    )?;
    let label = format!("com.littlemonkey.task.{}", recipe.name);

    if cfg!(target_os = "macos") {
        match little_monkey_lib::automations::format_launchd_plist(
            &label,
            "/usr/bin/env",
            &args,
            cron,
        )? {
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
                println!(
                    "{}",
                    little_monkey_lib::automations::format_crontab_line(
                        cron,
                        "/usr/bin/env",
                        &args,
                    )?
                );
            }
        }
    } else {
        println!(
            "{}",
            little_monkey_lib::automations::format_crontab_line(cron, "/usr/bin/env", &args)?
        );
        eprintln!("\n# Add the above line via `crontab -e`.");
    }

    Ok(())
}

/// One `task run` result — the `--json` output shape (design doc slice 1):
/// `{name, status, iterations_capped, final_message, files_changed}`.
#[derive(serde::Serialize)]
struct RunResult {
    name: String,
    run_id: Option<String>,
    status: String,
    iterations_capped: bool,
    final_message: Option<String>,
    files_changed: Vec<String>,
}

struct InvocationIdentity {
    run_id: String,
    idempotency_key: String,
}

fn invocation_identity(explicit_run_key: Option<&str>) -> Result<InvocationIdentity, String> {
    let seed = if let Some(value) = explicit_run_key {
        if value.trim().is_empty() {
            return Err("--run-key must not be empty".to_string());
        }
        format!("external:{value}")
    } else {
        match std::env::var(RUN_KEY_ENV) {
            Ok(value) if value.trim().is_empty() => {
                return Err(format!("{RUN_KEY_ENV} must not be empty when set"));
            }
            Ok(value) => format!("external:{value}"),
            Err(std::env::VarError::NotPresent) => format!("random:{}", uuid::Uuid::new_v4()),
            Err(std::env::VarError::NotUnicode(_)) => {
                return Err(format!("{RUN_KEY_ENV} must contain valid UTF-8"));
            }
        }
    };
    let digest = sha256_hex(seed.as_bytes());
    Ok(InvocationIdentity {
        run_id: format!("cli-task-{}", &digest[..32]),
        idempotency_key: format!("cli-task/{digest}"),
    })
}

fn capability(state: CapabilityState, evidence: &str) -> CapabilityAssessment {
    CapabilityAssessment {
        state,
        evidence: evidence.to_string(),
    }
}

pub(crate) fn cli_capabilities() -> ModelCapabilitiesSnapshot {
    let unknown = || {
        capability(
            CapabilityState::Unknown,
            "monkey does not inspect this capability before recipe submission",
        )
    };
    ModelCapabilitiesSnapshot {
        tool_calling: capability(
            CapabilityState::Supported,
            "monkey supplies the shared agent tool schema to this target",
        ),
        vision: unknown(),
        embeddings: unknown(),
        structured_output: unknown(),
        image_generation: unknown(),
        audio: unknown(),
        runtime_lifecycle: unknown(),
        fim: unknown(),
        code_completion: unknown(),
        inline_edit: unknown(),
        fim_metadata: None,
    }
}

/// Local RAM the model hub says this model id holds once resident, frozen into
/// the run spec so the daemon's admission control has a number to work with.
///
/// Before this, every CLI submission emitted `None` here and the daemon's memory
/// bound short-circuited to "fits" for every job it ever saw: admission was
/// wired up and inert on the only path that reaches it from `monkey daemon
/// queue`.
///
/// `None` is deliberately not `Some(0)`. The protocol rejects a zero estimate
/// precisely because zero means "this run holds no local weights", which is true
/// of a provider call and false of a model nobody measured. Passing the unknown
/// case through as `None` keeps those two apart all the way to
/// `admission::Reservation`, which admits an unmeasured model but refuses to
/// count it as having fitted — see that type for why the distinction is
/// load-bearing.
fn frozen_local_ram_estimate(model_id: &str) -> Option<u64> {
    use little_monkey_lib::m3_runtime_hub::M3ModelFootprint;
    let app_data = crate::app_data_dir()?;
    match little_monkey_lib::m3_runtime_hub::installed_model_footprint(&app_data, model_id) {
        M3ModelFootprint::Known { memory, .. } => Some(memory.ram_bytes).filter(|bytes| *bytes > 0),
        M3ModelFootprint::Unknown => None,
    }
}

fn snapshot_target(target: &recipes::RecipeTarget) -> Result<ModelTargetSnapshot, String> {
    let capabilities = cli_capabilities();
    if let Some(provider) = &target.provider {
        let model = target
            .model
            .clone()
            .ok_or("recipe target with 'provider' must also set 'model'")?;
        let custom = crate::providers_cli::load_custom_providers();
        let endpoint = little_monkey_lib::providers::resolve_base_url(provider, &custom)?
            .trim_end_matches('/')
            .to_string();
        let provider_id = safe_protocol_id("provider", provider);
        let target_digest =
            sha256_hex(format!("provider\0{provider_id}\0{endpoint}\0{model}").as_bytes());
        return Ok(ModelTargetSnapshot::Provider {
            target_id: format!("provider-{}", &target_digest[..24]),
            label: format!("{provider} / {model}"),
            provider_id: provider_id.clone(),
            endpoint,
            model,
            credential_ref_id: safe_protocol_id("credential", &format!("credential:{provider_id}")),
            capabilities,
        });
    }
    if let Some(model) = &target.ollama {
        let base_url = crate::ollama_api::host().trim_end_matches('/').to_string();
        let target_digest = sha256_hex(format!("ollama\0{base_url}\0{model}").as_bytes());
        return Ok(ModelTargetSnapshot::Ollama {
            target_id: format!("ollama-{}", &target_digest[..24]),
            label: format!("Ollama / {model}"),
            base_url,
            model: model.clone(),
            is_cloud: model.to_ascii_lowercase().contains("cloud"),
            capabilities,
            estimated_memory_bytes: frozen_local_ram_estimate(model),
        });
    }
    if let Some(endpoint) = &target.local_url {
        // The shared v1 protocol has no generic OpenAI-compatible-local
        // target variant. Provider is the structurally closest exact wire
        // representation (endpoint + model); `credential:none` explicitly
        // records that this CLI path sends no provider credential.
        let endpoint = endpoint.trim_end_matches('/').to_string();
        let model = target.model.clone().unwrap_or_else(|| "local".to_string());
        let target_digest = sha256_hex(format!("local-openai\0{endpoint}\0{model}").as_bytes());
        return Ok(ModelTargetSnapshot::Provider {
            target_id: format!("local-openai-{}", &target_digest[..24]),
            label: format!("Local OpenAI-compatible / {model}"),
            provider_id: "local-openai-compatible".to_string(),
            endpoint,
            model,
            credential_ref_id: "credential:none".to_string(),
            capabilities,
        });
    }
    if let Some(model_id) = &target.managed_model {
        // The frozen snapshot records the artifact this machine will serve. The
        // *path* is deliberately local and is never portable — a node receiving
        // this spec resolves the `model_id` against its own hub inventory rather
        // than trusting the path (see `daemon::placed_recipe_target`).
        let app_data = crate::app_data_dir().ok_or("Could not resolve the app data directory")?;
        let artifact =
            little_monkey_lib::m3_runtime_hub::installed_model_artifact(&app_data, model_id)
                .ok_or_else(|| {
                    format!("this machine has no managed model '{model_id}' installed")
                })?;
        let target_digest = sha256_hex(format!("managed-llama\0{model_id}").as_bytes());
        return Ok(ModelTargetSnapshot::ManagedLlama {
            target_id: format!("managed-{}", &target_digest[..24]),
            label: format!("Managed runtime / {model_id}"),
            model_id: model_id.clone(),
            model_path: artifact.to_string_lossy().to_string(),
            capabilities,
            estimated_memory_bytes:
                match little_monkey_lib::m3_runtime_hub::installed_model_footprint(
                    &app_data, model_id,
                ) {
                    little_monkey_lib::m3_runtime_hub::M3ModelFootprint::Known {
                        memory, ..
                    } => Some(memory.ram_bytes),
                    little_monkey_lib::m3_runtime_hub::M3ModelFootprint::Unknown => None,
                },
        });
    }
    Err(
        "recipe target must set exactly one of provider, ollama, local_url, or managed_model"
            .to_string(),
    )
}

fn snapshot_permission_mode(mode: PermissionMode) -> RunPermissionMode {
    match mode {
        PermissionMode::Manual => RunPermissionMode::Manual,
        PermissionMode::AcceptEdits => RunPermissionMode::AcceptEdits,
        PermissionMode::Smart => RunPermissionMode::Smart,
        PermissionMode::Plan => RunPermissionMode::Plan,
        PermissionMode::Auto => RunPermissionMode::Auto,
        PermissionMode::Bypass => RunPermissionMode::Bypass,
    }
}

fn permission_policy(mode: PermissionMode, approval_timeout_ms: u64) -> PermissionPolicySnapshot {
    let tool_rules = if matches!(mode, PermissionMode::AcceptEdits | PermissionMode::Auto) {
        ["write_file", "edit_file", "remember"]
            .into_iter()
            .map(|tool| ToolPermissionRule {
                tool: tool.to_string(),
                decision: ToolPolicyDecision::Allow,
            })
            .collect()
    } else {
        Vec::new()
    };
    PermissionPolicySnapshot {
        mode: snapshot_permission_mode(mode),
        unattended: true,
        approval_timeout_ms,
        default_tool_decision: if mode == PermissionMode::Plan {
            ToolPolicyDecision::Deny
        } else {
            ToolPolicyDecision::Prompt
        },
        tool_rules,
        allow_network: true,
        allow_external_mutations: std::env::var_os("LITTLE_MONKEY_DAEMON_ALLOW_EXTERNAL_MUTATIONS")
            .as_deref()
            == Some(std::ffi::OsStr::new("1")),
        egress_allowlist: None,
        channel_send: None,
    }
}

/// The one permission policy a run both records and executes under.
///
/// Precedence: a placed run's immutable policy, then a desktop turn's
/// snapshot, then the recipe's own declaration on top of the mode's defaults.
/// `run_inner` freezes exactly this into the RunSpec and hands exactly this
/// to `TerminalPermissions`, so what the ledger says the run could do and
/// what its tools consult at call time cannot be two different things.
fn frozen_permission_policy(
    recipe: &Recipe,
    mode: PermissionMode,
    approval_timeout_ms: u64,
) -> PermissionPolicySnapshot {
    match (&recipe.placed_run, &recipe.desktop_turn) {
        (Some(placed), _) => placed.permission_policy.clone(),
        (_, Some(snapshot)) => snapshot.permission_policy.clone(),
        _ => {
            let mut policy = permission_policy(mode, approval_timeout_ms);
            // A hand-authored/scheduled recipe is the only carrier of a
            // cross-conversation messaging grant on this path; the snapshot
            // records it so the run's authority is auditable after the fact.
            policy.channel_send = recipe.channel_send.clone();
            policy
        }
    }
}

fn workspace_snapshot(state: &little_monkey_lib::AppState) -> Result<WorkspaceContext, String> {
    let root = workspace::primary_root_canon(state)?;
    let canonical_path = root.to_string_lossy().to_string();
    let digest = sha256_hex(canonical_path.as_bytes());
    let repository_policy = std::env::var("LITTLE_MONKEY_DAEMON_REPOSITORY_POLICY_JSON")
        .ok()
        .map(|value| {
            let policy: little_monkey_lib::run_protocol::RepositoryPolicy =
                serde_json::from_str(&value)
                    .map_err(|error| format!("Invalid daemon repository policy: {error}"))?;
            policy.validate().map_err(|error| error.to_string())?;
            if policy.root_id != "root-primary" {
                return Err("Daemon repository policy must target root-primary".to_string());
            }
            Ok(policy)
        })
        .transpose()?;
    Ok(WorkspaceContext {
        workspace_id: format!("workspace-{}", &digest[..24]),
        primary_root_id: "root-primary".to_string(),
        roots: vec![RootGrant {
            root_id: "root-primary".to_string(),
            canonical_path,
            access: RootAccess::ReadWrite,
            allow_symlinks_within_root: true,
        }],
        repository_policy,
    })
}

fn terminal_retry_result(
    recipe_name: &str,
    recorder: &DurableRunRecorder,
    status: RunStatus,
) -> Result<(i32, RunResult), String> {
    let (code, label) = match status {
        RunStatus::Succeeded => (EXIT_OK, "already_succeeded"),
        RunStatus::Cancelled => (EXIT_TIMEOUT, "already_cancelled"),
        RunStatus::Failed => (EXIT_CONFIG_ERROR, "already_failed"),
        RunStatus::NeedsReconciliation => (EXIT_CONFIG_ERROR, "needs_reconciliation"),
        _ => return Err("nonterminal status passed to terminal retry result".to_string()),
    };
    Ok((
        code,
        RunResult {
            name: recipe_name.to_string(),
            run_id: Some(recorder.run_id()),
            status: label.to_string(),
            iterations_capped: false,
            final_message: recorder.terminal_summary()?,
            files_changed: Vec::new(),
        },
    ))
}

/// Runs `name_or_path` headlessly and returns the process exit code (design
/// doc slice 1: 0 success, 1 config/transport error, 2 permission-denied or
/// plan-blocked, 3 timeout/max-iterations). Streamed tokens go to stdout in
/// non-JSON mode (matching every other `monkey-cli` invocation) but to
/// stderr when `json_output` is set, so stdout stays a single parseable
/// result object — see `chat::stream_turn`'s printing, which already writes
/// content to stdout unconditionally; `json_output` instead suppresses it by
/// routing through a quiet options flag below.
pub async fn run(
    cli: &crate::Cli,
    client: &reqwest::Client,
    name_or_path: &str,
    param_flags: &[String],
    run_key: Option<&str>,
    json_output: bool,
) -> i32 {
    match run_inner(cli, client, name_or_path, param_flags, run_key, json_output).await {
        Ok((code, result)) => {
            if json_output {
                println!(
                    "{}",
                    serde_json::to_string(&result).unwrap_or_else(|_| "{}".to_string())
                );
            }
            code
        }
        Err(e) => {
            if json_output {
                let result = RunResult {
                    name: name_or_path.to_string(),
                    run_id: None,
                    status: "error".to_string(),
                    iterations_capped: false,
                    final_message: Some(e.clone()),
                    files_changed: Vec::new(),
                };
                println!(
                    "{}",
                    serde_json::to_string(&result).unwrap_or_else(|_| "{}".to_string())
                );
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

/// Reject permission modes that are unsafe or unusable when `task run` has no
/// human approval channel. This is deliberately stricter than
/// [`PermissionMode::parse`], because `bypass` remains a valid, explicit mode
/// for an interactive CLI session but must never be accepted by an unattended
/// recipe runner.
fn validate_headless_permission_mode(mode: PermissionMode) -> Result<(), String> {
    match mode {
        PermissionMode::Manual
            if std::env::var_os("LITTLE_MONKEY_DAEMON_APPROVAL_WAIT").as_deref()
                == Some(std::ffi::OsStr::new("1")) =>
        {
            Ok(())
        }
        PermissionMode::Manual => Err(
            "recipe's permission_mode 'manual' would wait for a prompt no one can answer in a headless run — install the daemon for durable approvals, or use acceptEdits, smart, auto, or plan"
                .to_string(),
        ),
        PermissionMode::Bypass => Err(
            "recipe's permission_mode 'bypass' is not allowed in a headless run — bypass auto-approves every tool, including shell commands, with nobody present; use acceptEdits, smart, auto, or plan instead"
                .to_string(),
        ),
        PermissionMode::AcceptEdits
        | PermissionMode::Smart
        | PermissionMode::Plan
        | PermissionMode::Auto => Ok(()),
    }
}

async fn run_inner(
    cli: &crate::Cli,
    client: &reqwest::Client,
    name_or_path: &str,
    param_flags: &[String],
    run_key: Option<&str>,
    json_output: bool,
) -> Result<(i32, RunResult), String> {
    let config_roots = little_monkey_lib::app_paths::agent_config_roots()?;
    let app_data_dir = config_roots.legacy.clone();
    let global_config_roots = config_roots.ordered();
    let workspace_root = std::env::current_dir().ok();
    let (recipe, recipe_path) = recipes::resolve_recipe_with_path(
        name_or_path,
        workspace_root.as_deref(),
        &global_config_roots,
    )?;

    let overrides = parse_param_flags(param_flags)?;
    let rendered = recipes::render_recipe(&recipe, &overrides)?;

    let resolved_target = resolve_recipe_chat_target(&recipe)?;
    let mode = PermissionMode::parse(&recipe.permission_mode)?;

    // Fail fast, before any network/model work. Shared recipe validation
    // already rejects `bypass`; this adapter-level check is intentional
    // defense in depth so a future recipe source cannot accidentally turn an
    // unattended run into an all-tools-approved session.
    validate_headless_permission_mode(mode)?;

    let state = if recipe
        .desktop_turn
        .as_ref()
        .is_some_and(|snapshot| snapshot.workspace.is_none())
    {
        little_monkey_lib::AppState::default()
    } else {
        let workspace_dir = resolve_workspace_dir(&recipe, &recipe_path);
        crate::build_state(&Some(workspace_dir))?
    };
    if let Some(snapshot) = &recipe.desktop_turn {
        apply_desktop_execution_roots(&state, snapshot)?;
    }

    let options = if let Some(snapshot) = &recipe.desktop_turn {
        // The recipe file is already the daemon's immutable private copy.
        // Never re-read current rules/memory here: `rendered.system` is the
        // exact composed desktop system prompt captured before queueing.
        desktop_chat_options(
            &snapshot.generation,
            &snapshot.tool_profile,
            rendered.system.clone(),
            json_output,
        )
    } else {
        let mut options = chat::ChatOptions {
            system: rendered.system.clone(),
            quiet: json_output,
            ..Default::default()
        };
        // A placed run's snapshot was frozen on the submitting machine and
        // enqueued here with `snapshot_is_frozen`. Merging this node's rules
        // into it would be the same immutability violation an explicit retry
        // avoids — and worse, it would inject one machine's instructions into
        // another machine's run.
        if recipe.placed_run.is_none() {
            options.system = crate::effective_system(cli, &state, options.system.as_deref());
        }
        options
    };

    let max_iterations = recipe.max_iterations.unwrap_or(25);
    if max_iterations == 0 || max_iterations > 10_000 {
        return Err("recipe max_iterations must be between 1 and 10000".to_string());
    }
    let max_iterations_u32 = u32::try_from(max_iterations)
        .map_err(|_| "recipe max_iterations exceeds the durable run protocol".to_string())?;
    // A placed run's wall clock is the submitter's, not this node's default:
    // the budget travelled with the spec and this is the first place it is
    // spent. `RunBudgets::validate` already bounded it, and the node's own
    // `max_runtime_ms` on the daemon job is the second, independent ceiling —
    // the run is held to whichever is tighter, which is the correct direction.
    let wall_time_ms = match (&recipe.placed_run, recipe.timeout_seconds) {
        (Some(placed), _) => placed.budgets.wall_time_ms,
        (None, Some(seconds)) => seconds
            .checked_mul(1_000)
            .filter(|millis| *millis > 0 && *millis <= DEFAULT_WALL_TIME_MS)
            .ok_or_else(|| {
                "recipe timeout_seconds must be between 1 second and 7 days".to_string()
            })?,
        (None, None) => DEFAULT_WALL_TIME_MS,
    };
    let approval_timeout_ms = wall_time_ms.clamp(60_000, DEFAULT_APPROVAL_TIMEOUT_MS);

    std::fs::create_dir_all(&app_data_dir).map_err(|error| {
        format!(
            "Failed to create app data directory '{}': {error}",
            app_data_dir.display()
        )
    })?;
    let invocation = invocation_identity(run_key)?;
    let ledger = RunLedger::open(app_data_dir.join(RUN_DATABASE_FILE))
        .map_err(|error| format!("Failed to open durable run ledger: {error}"))?;
    let existing = ledger
        .load_run_by_idempotency_key(&invocation.idempotency_key)
        .map_err(|error| error.to_string())?;
    let run_id = existing
        .as_ref()
        .map(|run| run.spec.run_id.clone())
        .unwrap_or(invocation.run_id);
    let created_at_ms = existing
        .as_ref()
        .map(|run| run.spec.created_at_ms)
        .unwrap_or(unix_time_ms()?);
    let submitted_by = ClientIdentity {
        client_id: "monkey-cli".to_string(),
        instance_id: run_id.clone(),
        kind: ClientKind::Cli,
        version: env!("CARGO_PKG_VERSION").to_string(),
    };
    let frozen_target = match (&recipe.placed_run, &recipe.desktop_turn) {
        (Some(placed), _) => placed.target.clone(),
        (_, Some(snapshot)) => snapshot.target.clone(),
        _ => snapshot_target(&recipe.target)?,
    };
    let frozen_workspace = match (&recipe.placed_run, &recipe.desktop_turn) {
        // A placed run without a workspace is a model-only run, and the node
        // must not invent one for it: `None` here is the submitter's statement
        // that this run has no filesystem, not an absence to be filled in.
        (Some(placed), _) => placed.workspace.clone(),
        (_, Some(snapshot)) => snapshot.workspace.clone(),
        _ => Some(workspace_snapshot(&state)?),
    };
    let frozen_policy = frozen_permission_policy(&recipe, mode, approval_timeout_ms);
    let input_artifact_ids = recipe
        .desktop_turn
        .as_ref()
        .map(|snapshot| {
            snapshot
                .attachments
                .iter()
                .map(|attachment| format!("attachment-{}", attachment.content_sha256))
                .collect()
        })
        .unwrap_or_default();
    let run_spec = RunSpec {
        schema_version: RUN_PROTOCOL_SCHEMA_VERSION,
        run_id: run_id.clone(),
        idempotency_key: invocation.idempotency_key,
        created_at_ms,
        kind: match (&recipe.placed_run, &recipe.desktop_turn) {
            (Some(placed), _) => placed.kind.clone(),
            (_, Some(_)) => RunKind::Interactive,
            _ => RunKind::Workflow,
        },
        submitted_by,
        task: rendered.prompt.clone(),
        instructions: options.system.clone(),
        input_artifact_ids,
        target: frozen_target,
        workspace: frozen_workspace,
        permission_policy: frozen_policy,
        budgets: match &recipe.placed_run {
            Some(placed) => placed.budgets.clone(),
            None => RunBudgets {
                wall_time_ms,
                max_iterations: max_iterations_u32,
                // The existing CLI bounds top-level iterations but an optional
                // explore subagent can add model calls inside one iteration.
                // These protocol maxima avoid claiming a tighter unenforced cap.
                max_model_calls: 100_000,
                max_tool_calls: 100_000,
                max_input_tokens: 1_000_000_000,
                max_output_tokens: 1_000_000_000,
                max_cost_micros: None,
                max_artifact_bytes: 1 << 40,
                max_event_count: 10_000_000,
            },
        },
    };
    // **The half of K17 S3 that makes a travelled policy more than paperwork.**
    //
    // `egress::send` resolves a run's allowlist through a process-wide source,
    // and this process never installed one — only the desktop app did
    // (`run_commands::install_run_egress_policy_source`). So until now a run's
    // frozen `egress_allowlist` was enforced in the app and ignored in every
    // headless `monkey-cli task run`, placed or local.
    //
    // The source is installed from the spec this process just froze rather than
    // from a ledger read, because the spec is right here and is immutable: there
    // is no row to go stale against. Every other run id answers `Unknown`, which
    // is the existing "not a ledger entity" case and stays permitted.
    {
        let scoped_run_id = run_spec.run_id.clone();
        let allowlist = run_spec
            .permission_policy
            .egress_allowlist
            .clone()
            .map(std::sync::Arc::new);
        little_monkey_lib::egress::install_run_policy_source(move |candidate| {
            if candidate != scoped_run_id {
                return little_monkey_lib::egress::RunEgressPolicy::Unknown;
            }
            match &allowlist {
                Some(allowlist) => little_monkey_lib::egress::RunEgressPolicy::Declared(
                    std::sync::Arc::clone(allowlist),
                ),
                None => little_monkey_lib::egress::RunEgressPolicy::Undeclared,
            }
        });
    }
    let (recorder, disposition) =
        DurableRunRecorder::submit(ledger, &run_spec, format!("recipe:{}", recipe.name))?;
    match disposition {
        SubmissionDisposition::AlreadyTerminal(status) => {
            return terminal_retry_result(&recipe.name, &recorder, status);
        }
        SubmissionDisposition::InterruptedReplayRefused => {
            return Ok((
                EXIT_CONFIG_ERROR,
                RunResult {
                    name: recipe.name,
                    run_id: Some(recorder.run_id()),
                    status: "interrupted_replay_refused".to_string(),
                    iterations_capped: false,
                    final_message: recorder.terminal_summary()?,
                    files_changed: Vec::new(),
                },
            ));
        }
        SubmissionDisposition::Ready { .. } => {}
    }
    // Internal queue-only boundary used by the resident daemon. The immutable
    // spec and Queued event are committed before the daemon acknowledges the
    // submission, but model/tool execution remains owned by the supervised
    // `task run` child started from the service loop.
    if std::env::var_os("LITTLE_MONKEY_TASK_QUEUE_ONLY").as_deref()
        == Some(std::ffi::OsStr::new("1"))
    {
        return Ok((
            EXIT_OK,
            RunResult {
                name: recipe.name,
                run_id: Some(recorder.run_id()),
                status: "queued".to_string(),
                iterations_capped: false,
                final_message: None,
                files_changed: Vec::new(),
            },
        ));
    }
    recorder.emit(RunEvent::Started {
        engine_id: if std::env::var_os("LITTLE_MONKEY_DAEMON_APPROVAL_WAIT").as_deref()
            == Some(std::ffi::OsStr::new("1"))
        {
            "monkey-daemon-task".to_string()
        } else {
            "monkey-cli-task".to_string()
        },
    })?;

    let runtime_inputs = async {
        let mcp_entries = if let Some(snapshot) = &recipe.desktop_turn {
            resolve_desktop_mcp_entries(&state, snapshot).await?
        } else {
            crate::resolve_mcp_entries(cli, &state).await
        };
        let attached_stacks = recipe
            .desktop_turn
            .as_ref()
            .map(resolve_desktop_stack_names)
            .transpose()?
            .unwrap_or_default();
        Ok::<_, String>((mcp_entries, attached_stacks))
    }
    .await;
    let (mcp_entries, attached_stacks) = match runtime_inputs {
        Ok(inputs) => inputs,
        Err(error) => {
            recorder.emit(RunEvent::Failed {
                code: "immutable_input_drift".to_string(),
                message: bounded_text(&error, 60 * 1024),
                retryable: error.contains("timed out") || error.contains("failed to connect"),
            })?;
            return Ok((
                EXIT_CONFIG_ERROR,
                RunResult {
                    name: recipe.name,
                    run_id: Some(recorder.run_id()),
                    status: "failed".to_string(),
                    iterations_capped: false,
                    final_message: Some(error),
                    files_changed: Vec::new(),
                },
            ));
        }
    };

    let event_sink: Arc<dyn CliRunEventSink> = recorder.clone();
    let mut perms =
        TerminalPermissions::with_event_sink(mode, event_sink, approval_timeout_ms, json_output);
    // The run executes under exactly the policy its immutable RunSpec
    // recorded — the same `frozen_policy` precedence (placed run, then
    // desktop turn, then the recipe's own declaration) — rather than a second
    // derivation that could drift from it. This is what makes a placed run's
    // cross-account messaging grant, and its external-mutations flag, real at
    // tool time instead of only auditable after the fact.
    perms.set_allow_network(run_spec.permission_policy.allow_network);
    perms.set_allow_external_mutations(run_spec.permission_policy.allow_external_mutations);
    perms.set_channel_send(run_spec.permission_policy.channel_send.clone());
    let mut history: Vec<serde_json::Value> = recipe
        .desktop_turn
        .as_ref()
        .map(|snapshot| snapshot.history.clone())
        .unwrap_or_default();

    // The app's own verified `llama-server`, started for exactly this run and
    // killed when `_managed_session` drops — normal return, error, and unwind
    // all reap it, which is what `ManagedServerSession`'s `Drop` is for.
    //
    // Started here rather than at resolution time because this is the point at
    // which the run really begins: a start failure is recorded against the
    // durable run like any other execution failure, instead of being a bare
    // error from a process the ledger never heard finish.
    let (target, _managed_session) = match resolved_target {
        ResolvedTarget::Ready(target) => (target, None),
        ResolvedTarget::ManagedModel { model_id } => {
            let started = async {
                let artifact = little_monkey_lib::m3_runtime_hub::installed_model_artifact(
                    &app_data_dir,
                    &model_id,
                )
                .ok_or_else(|| {
                    format!("this machine has no managed model '{model_id}' installed")
                })?;
                // Managed llama-server consumes the context size at process
                // startup, so it is never forwarded as a request option.
                let context = crate::managed_model_cli::context_tokens(None)?;
                let projector =
                    little_monkey_lib::models::projector_for_model(&app_data_dir, &artifact)?
                        .map(|component| PathBuf::from(component.path));
                crate::managed_model_cli::start_server(
                    client,
                    &artifact,
                    projector.as_deref(),
                    context,
                )
                .await
            }
            .await;
            match started {
                Ok(session) => (
                    Target::Local {
                        base_url: session.base_url(),
                        model: Some(session.model_alias().to_string()),
                        native_ollama: false,
                    },
                    Some(session),
                ),
                Err(error) => {
                    recorder.emit(RunEvent::Failed {
                        code: "managed_runtime_unavailable".to_string(),
                        message: bounded_text(&error, 60 * 1024),
                        retryable: false,
                    })?;
                    return Ok((
                        EXIT_CONFIG_ERROR,
                        RunResult {
                            name: recipe.name,
                            run_id: Some(recorder.run_id()),
                            status: "failed".to_string(),
                            iterations_capped: false,
                            final_message: Some(error),
                            files_changed: Vec::new(),
                        },
                    ));
                }
            }
        }
    };

    // The turn's frozen workspace-mutation contract, read from the immutable
    // snapshot rather than re-derived from the prompt: whether this turn
    // promised a file would change was decided when it was accepted.
    let mutation_required = recipe
        .desktop_turn
        .as_ref()
        .is_some_and(|snapshot| snapshot.workspace_mutation_required);
    let turn_future = async {
        if recipe.desktop_turn.is_some() {
            crate::agent::run_prepared_turn_with_max_iterations(
                client,
                &target,
                &state,
                &mut perms,
                &mut history,
                &options,
                &rendered.prompt,
                &mcp_entries,
                &attached_stacks,
                Some(max_iterations),
                mutation_required,
            )
            .await
        } else {
            crate::agent::run_turn_with_max_iterations(
                client,
                &target,
                &state,
                &mut perms,
                &mut history,
                &options,
                &rendered.prompt,
                &mcp_entries,
                &attached_stacks,
                Some(max_iterations),
            )
            .await
        }
    };

    // The run identity travels implicitly through `run_scope`'s task-local, and
    // that is what `egress::send` reads before asking the policy source
    // installed above. Without this the source would be installed and never
    // consulted — the allowlist would be attached to a run nothing knew it was
    // inside. Wrapping the turn rather than the whole function is deliberate:
    // this is where the run's own model and tool traffic happens.
    let turn_future =
        little_monkey_lib::run_scope::scoped(RunScope::run(recorder.run_id()), turn_future);

    let turn_result =
        match tokio::time::timeout(Duration::from_millis(wall_time_ms), turn_future).await {
            Ok(inner) => inner,
            Err(_elapsed) => {
                let reason = format!("Timed out after {} ms", wall_time_ms);
                if let Some(checkpoint_id) = recorder.latest_checkpoint_id()? {
                    let _ = little_monkey_lib::checkpoints::end_impl(&state, &checkpoint_id);
                }
                recorder.emit(RunEvent::CancellationRequested {
                    requested_by: recorder.client_identity(),
                    reason: Some(reason.clone()),
                })?;
                recorder.emit(RunEvent::Cancelling {
                    reason: Some(reason.clone()),
                })?;
                recorder.emit(RunEvent::Cancelled {
                    reason: Some(reason.clone()),
                })?;
                return Ok((
                    EXIT_TIMEOUT,
                    RunResult {
                        name: recipe.name,
                        run_id: Some(recorder.run_id()),
                        status: "timeout".to_string(),
                        iterations_capped: false,
                        final_message: Some(reason),
                        files_changed: Vec::new(),
                    },
                ));
            }
        };

    let files_changed = match turn_result {
        Ok(files_changed) => files_changed,
        Err(error) => {
            let exit_code = classify_error_exit_code(&error);
            let failure_code = if exit_code == EXIT_PERMISSION_DENIED {
                "permission_denied"
            } else {
                "execution_failed"
            };
            recorder.emit(RunEvent::Failed {
                code: failure_code.to_string(),
                message: bounded_text(&error, 60 * 1024),
                retryable: error.contains("Request failed")
                    || error.contains("Stream error")
                    || error.contains("connect"),
            })?;
            return Ok((
                exit_code,
                RunResult {
                    name: recipe.name,
                    run_id: Some(recorder.run_id()),
                    status: "failed".to_string(),
                    iterations_capped: false,
                    final_message: Some(error),
                    files_changed: Vec::new(),
                },
            ));
        }
    };
    let iterations_capped = history
        .last()
        .and_then(|m| m.get("content"))
        .and_then(|c| c.as_str())
        .map(|s| s.starts_with(crate::agent::ITERATION_CAP_MESSAGE_PREFIX))
        .unwrap_or(false);
    let final_message = history
        .last()
        .and_then(|m| m.get("content"))
        .and_then(|c| c.as_str())
        .map(|message| bounded_text(message, 60 * 1024));

    if iterations_capped {
        recorder.emit(RunEvent::Failed {
            code: "iteration_limit".to_string(),
            message: final_message
                .clone()
                .unwrap_or_else(|| "Iteration limit reached".to_string()),
            retryable: false,
        })?;
    } else {
        recorder.emit(RunEvent::Completed {
            summary: final_message.clone(),
            result_artifact_ids: Vec::new(),
            usage: recorder.current_usage()?,
        })?;
    }

    let result = RunResult {
        name: recipe.name,
        run_id: Some(recorder.run_id()),
        status: if iterations_capped {
            "incomplete"
        } else {
            "ok"
        }
        .to_string(),
        iterations_capped,
        final_message,
        files_changed,
    };
    let code = if iterations_capped {
        EXIT_TIMEOUT
    } else {
        EXIT_OK
    };
    Ok((code, result))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_generation() -> recipes::DesktopGenerationSettingsSnapshot {
        recipes::DesktopGenerationSettingsSnapshot {
            temperature: Some(0.25),
            top_p: Some(0.8),
            seed: Some(42),
            stop: vec!["STOP".to_string()],
            num_ctx: Some(8_192),
            num_predict: Some(512),
            format: Some(serde_json::json!({"type":"object"})),
            think: Some(serde_json::json!("high")),
            hide_thinking: true,
            keep_alive: Some("10m".to_string()),
            effort: Some("xhigh".to_string()),
        }
    }

    #[test]
    fn desktop_options_preserve_frozen_system_generation_and_tool_profile() {
        let profile = recipes::DesktopToolProfileSnapshot {
            memory_enabled: false,
            web_tools_enabled: false,
            verify_enabled: true,
            verify_max_rounds: 3,
            subagents_enabled: false,
        };
        let options = desktop_chat_options(
            &test_generation(),
            &profile,
            Some("system captured before queue".to_string()),
            true,
        );
        assert_eq!(
            options.system.as_deref(),
            Some("system captured before queue")
        );
        assert_eq!(options.temperature, Some(0.25));
        assert_eq!(options.top_p, Some(0.8));
        assert_eq!(options.seed, Some(42));
        assert_eq!(options.num_ctx, Some(8_192));
        assert_eq!(options.num_predict, Some(512));
        assert_eq!(options.effort.as_deref(), Some("xhigh"));
        assert!(options.verify);
        assert_eq!(options.verify_max_rounds, Some(3));
        assert_eq!(options.memory_enabled, Some(false));
        assert!(!options.subagents);
        assert!(options.quiet);
    }

    fn mcp_entry() -> McpServerEntry {
        McpServerEntry {
            id: "docs".to_string(),
            label: "Docs".to_string(),
            transport: little_monkey_lib::mcp::McpTransport::Stdio {
                command: "docs-server".to_string(),
                args: vec!["--safe".to_string()],
                env: std::collections::BTreeMap::from([(
                    "TOKEN".to_string(),
                    "local-only".to_string(),
                )]),
            },
            enabled: true,
            tool_allowlist: Some(vec!["read".to_string(), "search".to_string()]),
            timeout_secs: Some(30),
        }
    }

    #[test]
    fn desktop_mcp_selection_preserves_exact_entries_and_rejects_config_drift() {
        let entry = mcp_entry();
        let frozen = recipes::DesktopMcpServerSnapshot {
            id: entry.id.clone(),
            config_sha256: recipes::mcp_server_config_digest(&entry).unwrap(),
            tool_allowlist: recipes::normalized_mcp_tool_allowlist(entry.tool_allowlist.as_deref()),
        };
        let selected =
            select_desktop_mcp_entries(std::slice::from_ref(&frozen), std::slice::from_ref(&entry))
                .unwrap();
        assert_eq!(selected, vec![entry.clone()]);

        let mut changed = entry.clone();
        changed.timeout_secs = Some(31);
        assert!(select_desktop_mcp_entries(&[frozen.clone()], &[changed])
            .unwrap_err()
            .contains("config changed"));
        assert!(select_desktop_mcp_entries(&[frozen], &[])
            .unwrap_err()
            .contains("removed"));
    }

    fn knowledge_stack(id: &str, name: &str) -> KnowledgeStack {
        KnowledgeStack {
            id: id.to_string(),
            name: name.to_string(),
            sources: Vec::new(),
            embedding: little_monkey_lib::knowledge_core::EmbeddingSpec {
                backend: little_monkey_lib::knowledge_core::EmbeddingBackend::Llama,
                model_id_or_tag: "embed".to_string(),
                dim: 768,
                query_prefix: String::new(),
                doc_prefix: String::new(),
                extension_id: None,
            },
            chunk_chars: 1_600,
            chunk_overlap: 200,
            indexed_at: Some(1),
            chunk_count: 1,
        }
    }

    #[test]
    fn desktop_stack_ids_preserve_order_and_fail_on_missing_or_ambiguous_drift() {
        let configured = vec![
            knowledge_stack("stack-a", "Docs"),
            knowledge_stack("stack-b", "Notes"),
        ];
        assert_eq!(
            select_desktop_stack_names(
                &["stack-b".to_string(), "stack-a".to_string()],
                &["Notes".to_string(), "Docs".to_string()],
                &configured,
            )
            .unwrap(),
            vec!["Notes".to_string(), "Docs".to_string()]
        );
        assert!(select_desktop_stack_names(
            &["stack-missing".to_string()],
            &["Missing".to_string()],
            &configured,
        )
        .unwrap_err()
        .contains("removed"));
        assert!(select_desktop_stack_names(
            &["stack-a".to_string()],
            &["Old Docs".to_string()],
            &configured,
        )
        .unwrap_err()
        .contains("renamed"));
        let ambiguous = vec![
            knowledge_stack("stack-a", "Docs"),
            knowledge_stack("stack-c", "docs"),
        ];
        assert!(select_desktop_stack_names(
            &["stack-a".to_string()],
            &["Docs".to_string()],
            &ambiguous,
        )
        .unwrap_err()
        .contains("ambiguous"));
    }

    #[test]
    fn parse_param_flags_parses_key_value_pairs() {
        let map = parse_param_flags(&["manifest=package.json".to_string(), "count=3".to_string()])
            .unwrap();
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

    #[test]
    fn schedule_command_pins_agent_home_as_environment_and_profile_as_an_argument() {
        let agent_home = Path::new("/home/test/Agent Home");
        let binary_path = Path::new("/opt/little monkey/bin/monkey");
        let recipe_path = Path::new("/repo/recipes/nightly audit.yml");
        assert_eq!(
            schedule_command_args(agent_home, binary_path, "work", recipe_path),
            Ok(vec![
                "LITTLE_MONKEY_HOME=/home/test/Agent Home".to_string(),
                "/opt/little monkey/bin/monkey".to_string(),
                "--profile".to_string(),
                "work".to_string(),
                "task".to_string(),
                "run".to_string(),
                recipe_path.to_string_lossy().into_owned(),
                "--json".to_string(),
            ])
        );
    }

    #[cfg(unix)]
    #[test]
    fn schedule_rejects_non_utf8_paths_instead_of_changing_them() {
        use std::os::unix::ffi::OsStringExt;

        let invalid = PathBuf::from(std::ffi::OsString::from_vec(vec![
            b'/', b't', b'm', b'p', 0xff,
        ]));
        let binary = Path::new("/tmp/monkey");
        let recipe = Path::new("/tmp/recipe.yml");
        assert!(schedule_command_args(&invalid, binary, "work", recipe).is_err());
        assert!(schedule_command_args(Path::new("/tmp/home"), &invalid, "work", recipe).is_err());
        assert!(schedule_command_args(Path::new("/tmp/home"), binary, "work", &invalid).is_err());
    }

    #[test]
    fn explicit_run_key_is_stable_and_never_stored_verbatim() {
        let first = invocation_identity(Some("ci-job-42/attempt-1")).unwrap();
        let second = invocation_identity(Some("ci-job-42/attempt-1")).unwrap();
        assert_eq!(first.run_id, second.run_id);
        assert_eq!(first.idempotency_key, second.idempotency_key);
        assert!(!first.run_id.contains("ci-job-42"));
        assert!(!first.idempotency_key.contains("ci-job-42"));
    }

    #[test]
    fn explicit_run_key_rejects_empty_values() {
        assert!(invocation_identity(Some("   ")).is_err());
    }

    /// A managed target resolves to an *intent*, never to an origin: the
    /// runtime it names is not listening until the run starts it. This is the
    /// gap that made K17 refuse `ManagedLlama` placements outright.
    #[test]
    fn a_managed_recipe_target_resolves_to_a_runtime_to_start_rather_than_a_url() {
        let mut recipe = recipe_with_workspace(None);
        recipe.target = recipes::RecipeTarget {
            provider: None,
            model: None,
            ollama: None,
            local_url: None,
            managed_model: Some("qwen3-8b".to_string()),
        };
        match resolve_recipe_chat_target(&recipe).unwrap() {
            ResolvedTarget::ManagedModel { model_id } => assert_eq!(model_id, "qwen3-8b"),
            ResolvedTarget::Ready(_) => {
                panic!("a managed target must not resolve to an origin that is not listening yet")
            }
        }
    }

    fn ollama_target() -> recipes::RecipeTarget {
        recipes::RecipeTarget {
            provider: None,
            model: None,
            ollama: Some("qwen2.5:14b".to_string()),
            local_url: None,
            managed_model: None,
        }
    }

    #[test]
    fn resolve_chat_target_maps_ollama_to_a_native_local_target() {
        let target = resolve_chat_target(&ollama_target()).unwrap();
        match target {
            Target::Local {
                model,
                native_ollama,
                ..
            } => {
                assert_eq!(model.as_deref(), Some("qwen2.5:14b"));
                assert!(native_ollama);
            }
            _ => panic!("expected a Local target"),
        }
    }

    #[test]
    fn durable_ollama_target_snapshot_is_protocol_valid() {
        let target = snapshot_target(&ollama_target()).unwrap();
        target.validate().unwrap();
        match target {
            ModelTargetSnapshot::Ollama {
                model, base_url, ..
            } => {
                assert_eq!(model, "qwen2.5:14b");
                assert!(base_url.starts_with("http://") || base_url.starts_with("https://"));
            }
            _ => panic!("expected an Ollama snapshot"),
        }
    }

    #[test]
    fn resolve_chat_target_maps_provider_plus_model() {
        let target = recipes::RecipeTarget {
            provider: Some("openrouter".to_string()),
            model: Some("anthropic/claude-sonnet".to_string()),
            ollama: None,
            local_url: None,
            managed_model: None,
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
        let target = recipes::RecipeTarget {
            provider: None,
            model: None,
            ollama: None,
            local_url: Some("http://127.0.0.1:8090".to_string()),
            managed_model: None,
        };
        let resolved = resolve_chat_target(&target).unwrap();
        match resolved {
            Target::Local {
                base_url,
                native_ollama,
                ..
            } => {
                assert_eq!(base_url, "http://127.0.0.1:8090");
                assert!(!native_ollama);
            }
            _ => panic!("expected a Local target"),
        }
    }

    #[test]
    fn durable_local_openai_target_explicitly_records_no_credential() {
        let target = recipes::RecipeTarget {
            provider: None,
            model: None,
            ollama: None,
            local_url: Some("http://127.0.0.1:8090".to_string()),
            managed_model: None,
        };
        let snapshot = snapshot_target(&target).unwrap();
        snapshot.validate().unwrap();
        match snapshot {
            ModelTargetSnapshot::Provider {
                provider_id,
                credential_ref_id,
                model,
                ..
            } => {
                assert_eq!(provider_id, "local-openai-compatible");
                assert_eq!(credential_ref_id, "credential:none");
                assert_eq!(model, "local");
            }
            _ => panic!("expected the v1 provider-shaped local snapshot"),
        }
    }

    #[test]
    fn checked_in_conformance_fixture_runs_through_the_cli_entrypoint() {
        let path = format!(
            "{}/src/bin/monkey-cli/fixtures/durable_run_conformance.json",
            env!("CARGO_MANIFEST_DIR")
        );
        conformance(&path).unwrap();
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
            channel_send: None,
            desktop_turn: None,
            placed_run: None,
        }
    }

    /// The immutable snapshot a submitter placed this run with, carrying one
    /// explicit cross-account messaging grant.
    fn placed_with_grant(
        channel_send: Option<little_monkey_lib::run_protocol::ChannelSendPolicy>,
    ) -> little_monkey_lib::node_placement::PlacedRunSnapshot {
        let mut policy = permission_policy(PermissionMode::Manual, 1_000);
        policy.allow_external_mutations = true;
        policy.channel_send = channel_send;
        little_monkey_lib::node_placement::PlacedRunSnapshot {
            schema_version: 1,
            submitted_run_id: "run:placed".to_string(),
            kind: little_monkey_lib::run_protocol::RunKind::Workflow,
            target: snapshot_target(&ollama_target()).expect("target"),
            workspace: None,
            permission_policy: policy,
            budgets: little_monkey_lib::run_protocol::RunBudgets {
                wall_time_ms: 60_000,
                max_iterations: 5,
                max_model_calls: 100,
                max_tool_calls: 100,
                max_input_tokens: 1_000_000,
                max_output_tokens: 1_000_000,
                max_cost_micros: None,
                max_artifact_bytes: 1 << 20,
                max_event_count: 10_000,
            },
        }
    }

    #[test]
    fn a_placed_run_executes_under_the_grant_it_was_placed_with() {
        use little_monkey_lib::run_protocol::ChannelSendPolicy;
        // The recipe wrapping a placed run declares nothing of its own; the
        // grant must come from the placement snapshot and nowhere else.
        let mut recipe = recipe_with_workspace(None);
        recipe.placed_run = Some(placed_with_grant(Some(ChannelSendPolicy {
            cross_conversation: false,
            accounts: vec!["chan-ops".to_string()],
        })));

        let policy = frozen_permission_policy(&recipe, PermissionMode::Manual, 1_000);
        let grant = policy.channel_send.expect("the placed grant survives");
        assert_eq!(grant.accounts, vec!["chan-ops".to_string()]);
        assert!(policy.allow_external_mutations);

        // And the grant is exactly what the tool's authorization ladder then
        // consults: that account is reachable, any other is refused.
        let authority = crate::daemon::channel_tool::SendAuthority {
            reply: false,
            cross_conversation: grant.cross_conversation,
            accounts: grant.accounts,
        };
        let mut request = crate::daemon::channel_tool::ChannelSendRequest {
            account_id: Some("chan-ops".to_string()),
            conversation_id: Some("conv-1".to_string()),
            text: "placed".to_string(),
            ..Default::default()
        };
        crate::daemon::channel_tool::plan_send(&request, &authority, None)
            .expect("the placed grant reaches exactly that account");
        request.account_id = Some("chan-other".to_string());
        crate::daemon::channel_tool::plan_send(&request, &authority, None)
            .expect_err("an account the placement never granted stays refused");
    }

    #[test]
    fn a_placed_run_without_the_grant_cannot_send_cross_account() {
        let mut recipe = recipe_with_workspace(None);
        // The wrapping recipe tries to smuggle a grant of its own in; the
        // placed snapshot, which carries none, must win.
        recipe.channel_send = Some(little_monkey_lib::run_protocol::ChannelSendPolicy {
            cross_conversation: true,
            accounts: vec!["chan-ops".to_string()],
        });
        recipe.placed_run = Some(placed_with_grant(None));

        let policy = frozen_permission_policy(&recipe, PermissionMode::Manual, 1_000);
        assert!(policy.channel_send.is_none());

        let authority = crate::daemon::channel_tool::SendAuthority {
            reply: false,
            cross_conversation: false,
            accounts: Vec::new(),
        };
        let request = crate::daemon::channel_tool::ChannelSendRequest {
            account_id: Some("chan-ops".to_string()),
            conversation_id: Some("conv-1".to_string()),
            text: "placed".to_string(),
            ..Default::default()
        };
        crate::daemon::channel_tool::plan_send(&request, &authority, None)
            .expect_err("no grant on the placement, no cross-account send");
    }

    #[test]
    fn a_plain_recipes_own_declaration_still_reaches_execution() {
        let mut recipe = recipe_with_workspace(None);
        recipe.channel_send = Some(little_monkey_lib::run_protocol::ChannelSendPolicy {
            cross_conversation: true,
            accounts: Vec::new(),
        });
        let policy = frozen_permission_policy(&recipe, PermissionMode::Manual, 1_000);
        assert!(policy.channel_send.expect("declared").cross_conversation);
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
        assert_eq!(
            classify_error_exit_code("Permission denied"),
            EXIT_PERMISSION_DENIED
        );
        assert_eq!(
            classify_error_exit_code("Permission denied: write_file requires..."),
            EXIT_PERMISSION_DENIED
        );
    }

    #[test]
    fn classify_error_exit_code_maps_plan_mode_block_to_exit_2() {
        assert_eq!(
            classify_error_exit_code("Blocked: monkey-cli is in Plan Mode. ..."),
            EXIT_PERMISSION_DENIED
        );
    }

    #[test]
    fn classify_error_exit_code_maps_everything_else_to_exit_1() {
        assert_eq!(
            classify_error_exit_code("Failed to connect to the model"),
            EXIT_CONFIG_ERROR
        );
    }

    #[test]
    fn headless_permission_validation_rejects_manual_without_recommending_bypass() {
        let error = validate_headless_permission_mode(PermissionMode::Manual).unwrap_err();
        assert!(error.contains("no one can answer"));
        assert!(!error.contains("bypass"));
    }

    #[test]
    fn headless_permission_validation_rejects_bypass() {
        let error = validate_headless_permission_mode(PermissionMode::Bypass).unwrap_err();
        assert!(error.contains("not allowed in a headless run"));
        assert!(error.contains("auto-approves every tool"));
        assert!(!error.contains("use bypass"));
    }

    #[test]
    fn headless_permission_validation_accepts_only_non_bypass_automatic_or_plan_modes() {
        for mode in [
            PermissionMode::AcceptEdits,
            PermissionMode::Smart,
            PermissionMode::Plan,
            PermissionMode::Auto,
        ] {
            assert!(
                validate_headless_permission_mode(mode).is_ok(),
                "expected {mode:?} to be allowed"
            );
        }
    }
}
