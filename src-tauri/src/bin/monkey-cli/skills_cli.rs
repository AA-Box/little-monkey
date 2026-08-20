//! Native/package/local-prompt skill parity for the CLI. The runtime is the
//! same data-only `NativeSkillManager` used by Tauri; this module only adds
//! Clap rendering and turn-scoped prompt composition.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use clap::{Subcommand, ValueEnum};
use little_monkey_lib::native_skills::{
    community_skill_git_request, ExternalSignedSkill, GitSkillRequest, NativeSkillManager,
    SkillDescriptor, SkillScope,
};
use little_monkey_lib::prompts::PromptEntry;
use little_monkey_lib::skill_activation::{SkillActivationPolicy, SkillActivationStore};
use little_monkey_lib::skill_learning::{
    EvaluationCaseReport, LearningMode, LearningPolicy, LearningSettings, PromotionOutcome,
    SkillLearningStore,
};

const MAX_SKILLS_PER_TURN: usize = 5;
const RESERVED: &[&str] = &[
    "status", "tools", "skills", "plugins", "model", "new", "compact", "stop", "usage", "learn",
];

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum CliSkillScope {
    Global,
    Workspace,
}

impl From<CliSkillScope> for SkillScope {
    fn from(value: CliSkillScope) -> Self {
        match value {
            CliSkillScope::Global => SkillScope::Global,
            CliSkillScope::Workspace => SkillScope::Workspace,
        }
    }
}

#[derive(Subcommand, Debug)]
pub enum SkillsCmd {
    /// List discovered native and signed-package skills with eligibility.
    List {
        #[arg(long)]
        json: bool,
    },
    /// Preview a local SKILL.md folder without installing it.
    PreviewLocal {
        path: PathBuf,
        #[arg(long, value_enum, default_value_t = CliSkillScope::Global)]
        scope: CliSkillScope,
    },
    /// Install/update a previously previewed local skill with its exact approval digest.
    InstallLocal {
        path: PathBuf,
        #[arg(long, value_enum, default_value_t = CliSkillScope::Global)]
        scope: CliSkillScope,
        #[arg(long)]
        approval_digest: String,
        #[arg(long)]
        yes: bool,
    },
    /// Preview a Git skill. The commit may be a 40-hex SHA, a branch/tag
    /// name, or omitted for the default branch; refs are resolved and the
    /// pinned commit is reported in the preview. When the repository root
    /// has no SKILL.md and no --subdirectory is given, the discovered skill
    /// folders are listed instead.
    PreviewGit {
        repository_url: String,
        #[arg(default_value = "")]
        commit: String,
        #[arg(long)]
        subdirectory: Option<String>,
        #[arg(long, value_enum, default_value_t = CliSkillScope::Global)]
        scope: CliSkillScope,
    },
    /// Install/update a previously previewed Git skill with its exact approval digest.
    InstallGit {
        repository_url: String,
        #[arg(default_value = "")]
        commit: String,
        #[arg(long)]
        subdirectory: Option<String>,
        #[arg(long, value_enum, default_value_t = CliSkillScope::Global)]
        scope: CliSkillScope,
        #[arg(long)]
        approval_digest: String,
        #[arg(long)]
        yes: bool,
    },
    /// Install a named community skill from little-monkey's own `skills/`
    /// directory at a pinned commit — shorthand for `preview-git` /
    /// `install-git` against that fixed repository, commit, and
    /// `skills/<name>` subdirectory. Run without `--approval-digest` first to
    /// preview and see the digest; run again with `--approval-digest` and
    /// `--yes` to install.
    Install {
        name: String,
        #[arg(long, value_enum, default_value_t = CliSkillScope::Global)]
        scope: CliSkillScope,
        #[arg(long)]
        approval_digest: Option<String>,
        #[arg(long)]
        yes: bool,
    },
    Enable {
        command: String,
        #[arg(long, value_enum, default_value_t = CliSkillScope::Global)]
        scope: CliSkillScope,
    },
    Disable {
        command: String,
        #[arg(long, value_enum, default_value_t = CliSkillScope::Global)]
        scope: CliSkillScope,
    },
    Rollback {
        command: String,
        #[arg(long, value_enum, default_value_t = CliSkillScope::Global)]
        scope: CliSkillScope,
    },
    Uninstall {
        command: String,
        #[arg(long, value_enum, default_value_t = CliSkillScope::Global)]
        scope: CliSkillScope,
        #[arg(long)]
        yes: bool,
    },
    /// Skills this agent derived from its own verified work, and the
    /// candidates still waiting on evaluation or approval.
    #[command(subcommand)]
    Learned(LearnedCmd),
    /// Read or change per-skill activation policy in the active profile.
    #[command(subcommand)]
    Activation(ActivationCmd),
}

#[derive(Subcommand, Debug)]
pub enum ActivationCmd {
    List {
        #[arg(long)]
        json: bool,
    },
    Get {
        key: String,
    },
    Set {
        key: String,
        #[arg(value_enum)]
        policy: CliActivationPolicy,
        #[arg(long)]
        pinned: bool,
    },
}

#[derive(Subcommand, Debug)]
pub enum LearnedCmd {
    /// Installed learned skills with their provenance and effectiveness.
    List {
        #[arg(long)]
        json: bool,
    },
    /// Candidates derived from run evidence but not yet installed.
    Candidates {
        #[arg(long)]
        json: bool,
    },
    /// Full detail for one candidate, including the staged SKILL.md body.
    Inspect { candidate_id: String },
    /// Run the candidate's evaluation cases.
    ///
    /// The CLI has no agent runtime of its own for this, so without
    /// `--report` it records the evaluation as unevaluated rather than
    /// inventing a verdict. `--report` takes the JSON array of case reports a
    /// runtime produced (`[{"case_id":…,"arm":"candidate",…}]`).
    Evaluate {
        candidate_id: String,
        #[arg(long)]
        report: Option<PathBuf>,
    },
    /// Approve and install a staged candidate as a versioned native skill.
    Promote {
        candidate_id: String,
        #[arg(long)]
        yes: bool,
    },
    /// Discard a candidate and delete its staged package.
    Reject {
        candidate_id: String,
        #[arg(long, default_value = "rejected from the CLI")]
        reason: String,
    },
    /// Disable an installed learned skill, keeping its provenance and history.
    Deprecate {
        command: String,
        #[arg(long, value_enum, default_value_t = CliSkillScope::Global)]
        scope: CliSkillScope,
        #[arg(long, default_value = "deprecated from the CLI")]
        reason: String,
    },
    /// Read or change the learning mode shared with the desktop app.
    Mode {
        #[arg(value_enum)]
        mode: Option<CliLearningMode>,
    },
    /// Read or change the three-state learning policy shared with the desktop app.
    Policy {
        #[arg(value_enum)]
        policy: Option<CliLearningPolicy>,
    },
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum CliLearningMode {
    Off,
    SuggestOnly,
    AutoStage,
    AutoPromoteSafe,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum CliLearningPolicy {
    Automatic,
    Ask,
    Manual,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum CliActivationPolicy {
    Automatic,
    Ask,
    Manual,
}

impl From<CliActivationPolicy> for SkillActivationPolicy {
    fn from(value: CliActivationPolicy) -> Self {
        match value {
            CliActivationPolicy::Automatic => Self::Automatic,
            CliActivationPolicy::Ask => Self::Ask,
            CliActivationPolicy::Manual => Self::Manual,
        }
    }
}

impl From<CliLearningPolicy> for LearningPolicy {
    fn from(value: CliLearningPolicy) -> Self {
        match value {
            CliLearningPolicy::Automatic => Self::Automatic,
            CliLearningPolicy::Ask => Self::Ask,
            CliLearningPolicy::Manual => Self::Manual,
        }
    }
}

impl From<CliLearningMode> for LearningMode {
    fn from(value: CliLearningMode) -> Self {
        match value {
            CliLearningMode::Off => Self::Off,
            CliLearningMode::SuggestOnly => Self::SuggestOnly,
            CliLearningMode::AutoStage => Self::AutoStage,
            CliLearningMode::AutoPromoteSafe => Self::AutoPromoteSafe,
        }
    }
}

#[derive(Debug, Clone)]
pub struct CliSkill {
    pub id: String,
    pub command: String,
    pub name: String,
    pub version: String,
    pub sha256: String,
    pub instructions: String,
    pub source: String,
    pub permissions: BTreeSet<String>,
}

fn workspace_for_scope<'a>(
    scope: SkillScope,
    workspace: Option<&'a Path>,
) -> Result<Option<&'a Path>, String> {
    match scope {
        SkillScope::Global => Ok(None),
        SkillScope::Workspace => workspace.map(Some).ok_or_else(|| {
            "Workspace scope requires --workspace or a current working directory".to_string()
        }),
    }
}

fn manager(data_dir: &Path) -> Result<NativeSkillManager, String> {
    NativeSkillManager::new(data_dir).map_err(|error| error.to_string())
}

fn external_package_skills(data_dir: &Path) -> Result<Vec<ExternalSignedSkill>, String> {
    let state = little_monkey_lib::m4_commands::M4CommandState::production(data_dir)?;
    state
        .packages
        .active_skills()
        .map_err(|error| error.to_string())
        .map(|entries| {
            entries
                .into_iter()
                .map(|skill| ExternalSignedSkill {
                    package_id: skill.package_id,
                    name: skill.name,
                    description: skill.description,
                    command: skill.command,
                    version: skill.version.to_string(),
                    instructions: skill.instructions,
                    sha256: skill.content_sha256,
                    permissions: skill
                        .permissions
                        .into_iter()
                        .map(|permission| permission.permission_id)
                        .collect(),
                })
                .collect()
        })
}

pub fn descriptors(
    data_dir: &Path,
    workspace: Option<&Path>,
) -> Result<Vec<SkillDescriptor>, String> {
    let packages = external_package_skills(data_dir)?;
    manager(data_dir)?
        .discover(workspace, &packages)
        .map_err(|error| error.to_string())
}

pub fn discover_for_chat(
    data_dir: &Path,
    workspace: Option<&Path>,
    prompt_entries: &[PromptEntry],
) -> Result<Vec<CliSkill>, String> {
    let mut by_command = BTreeMap::<String, CliSkill>::new();
    for descriptor in descriptors(data_dir, workspace)? {
        if !descriptor.enabled || !descriptor.eligibility.eligible {
            continue;
        }
        let source = match &descriptor.source {
            little_monkey_lib::native_skills::SkillSource::Global { path } => {
                format!("global:{path}")
            }
            little_monkey_lib::native_skills::SkillSource::Workspace { path } => {
                format!("workspace:{path}")
            }
            little_monkey_lib::native_skills::SkillSource::SignedPackage { package_id } => {
                format!("package:{package_id}")
            }
        };
        by_command.insert(
            descriptor.command.clone(),
            CliSkill {
                id: source.clone(),
                command: descriptor.command,
                name: descriptor.name,
                version: descriptor.version,
                sha256: descriptor.sha256,
                instructions: descriptor.instructions,
                source,
                permissions: descriptor.permissions,
            },
        );
    }
    for entry in prompt_entries.iter().filter(|entry| entry.kind == "skill") {
        if RESERVED.contains(&entry.command.as_str()) {
            return Err(format!(
                "Local prompt skill /{} collides with a reserved built-in command",
                entry.command
            ));
        }
        if let Some(existing) = by_command.get(&entry.command) {
            return Err(format!(
                "Skill /{} is ambiguous between {} and local prompt {}",
                entry.command, existing.source, entry.id
            ));
        }
        by_command.insert(
            entry.command.clone(),
            CliSkill {
                id: entry.id.clone(),
                command: entry.command.clone(),
                name: entry.name.clone(),
                version: format!("local-{}", entry.updated_at),
                sha256: format!("local:{}:{}", entry.id, entry.updated_at),
                instructions: entry.content.clone(),
                source: "local-prompt".to_string(),
                permissions: BTreeSet::new(),
            },
        );
    }
    Ok(by_command.into_values().collect())
}

pub fn compose_for_prompt(
    base_system: Option<&str>,
    prompt: &str,
    skills: &[CliSkill],
) -> Result<Option<String>, String> {
    let registry = skills
        .iter()
        .map(|skill| (skill.command.as_str(), skill))
        .collect::<BTreeMap<_, _>>();
    let trimmed = prompt.trim_start();
    if !trimmed.starts_with('/') {
        return Ok(base_system.map(str::to_string));
    }
    let mut rest = trimmed;
    let mut selected = Vec::<&CliSkill>::new();
    loop {
        let Some(without_slash) = rest.strip_prefix('/') else {
            break;
        };
        let end = without_slash
            .find(char::is_whitespace)
            .unwrap_or(without_slash.len());
        let command = &without_slash[..end];
        let Some(skill) = registry.get(command).copied() else {
            break;
        };
        if selected.iter().any(|entry| entry.id == skill.id) {
            return Err(format!(
                "Skill /{command} can only be invoked once per turn"
            ));
        }
        selected.push(skill);
        if selected.len() > MAX_SKILLS_PER_TURN {
            return Err(format!(
                "A turn can invoke at most {MAX_SKILLS_PER_TURN} skills"
            ));
        }
        rest = without_slash[end..].trim_start();
    }
    if selected.is_empty() {
        return Ok(base_system.map(str::to_string));
    }
    let request = rest.trim();
    let mut sections = Vec::new();
    if let Some(base) = base_system.filter(|value| !value.trim().is_empty()) {
        sections.push(base.to_string());
    }
    sections.push(
        "## Explicitly invoked skills\nApply these frozen, task-scoped instructions for this turn only. They never bypass tool, workspace, network, or mutation permissions."
            .to_string(),
    );
    for skill in selected {
        sections.push(format!(
            "### {} (/{})\nFrozen source: {} {} version {} hash {}\nDeclared permissions: {}\nInstructions:\n{}\nArguments/request:\n{}",
            skill.name,
            skill.command,
            skill.source,
            skill.id,
            skill.version,
            skill.sha256,
            if skill.permissions.is_empty() { "none declared; normal run permissions still apply".to_string() } else { skill.permissions.iter().cloned().collect::<Vec<_>>().join(", ") },
            skill.instructions,
            if request.is_empty() { "(none)" } else { request },
        ));
    }
    Ok(Some(sections.join("\n\n")))
}

fn git_request(
    repository_url: &str,
    commit: &str,
    subdirectory: &Option<String>,
) -> GitSkillRequest {
    GitSkillRequest {
        repository_url: repository_url.to_string(),
        commit: commit.to_string(),
        subdirectory: subdirectory.clone(),
    }
}

pub fn run(action: &SkillsCmd, data_dir: &Path, workspace: Option<&Path>) -> Result<(), String> {
    if let SkillsCmd::Activation(action) = action {
        return run_activation(action, data_dir);
    }
    let manager = manager(data_dir)?;
    match action {
        SkillsCmd::List { json } => {
            let entries = descriptors(data_dir, workspace)?;
            if *json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&entries).map_err(|error| error.to_string())?
                );
            } else if entries.is_empty() {
                println!("No native or signed-package skills discovered.");
            } else {
                for skill in entries {
                    let status = if !skill.enabled {
                        "disabled"
                    } else if skill.eligibility.eligible {
                        "ready"
                    } else {
                        "ineligible"
                    };
                    println!(
                        "/{:<24} {:<12} {} {}",
                        skill.command, status, skill.name, skill.version
                    );
                    if !skill.eligibility.eligible {
                        println!(
                            "  missing bins: {}; missing env: {}",
                            skill.eligibility.missing_bins.join(", "),
                            skill.eligibility.missing_env.join(", ")
                        );
                    }
                }
            }
            Ok(())
        }
        SkillsCmd::PreviewLocal { path, scope } => {
            let scope = SkillScope::from(*scope);
            let preview = manager
                .preview_local(path, scope, workspace_for_scope(scope, workspace)?)
                .map_err(|error| error.to_string())?;
            println!(
                "{}",
                serde_json::to_string_pretty(&preview).map_err(|error| error.to_string())?
            );
            Ok(())
        }
        SkillsCmd::InstallLocal {
            path,
            scope,
            approval_digest,
            yes,
        } => {
            let scope = SkillScope::from(*scope);
            let result = manager
                .install_local(
                    path,
                    scope,
                    workspace_for_scope(scope, workspace)?,
                    approval_digest,
                    *yes,
                )
                .map_err(|error| error.to_string())?;
            println!(
                "Installed /{} ({})",
                result.command,
                result.active_sha256.unwrap_or_default()
            );
            Ok(())
        }
        SkillsCmd::PreviewGit {
            repository_url,
            commit,
            subdirectory,
            scope,
        } => {
            let scope = SkillScope::from(*scope);
            let preview = manager
                .preview_git(
                    &git_request(repository_url, commit, subdirectory),
                    scope,
                    workspace_for_scope(scope, workspace)?,
                )
                .map_err(|error| error.to_string())?;
            println!(
                "{}",
                serde_json::to_string_pretty(&preview).map_err(|error| error.to_string())?
            );
            Ok(())
        }
        SkillsCmd::InstallGit {
            repository_url,
            commit,
            subdirectory,
            scope,
            approval_digest,
            yes,
        } => {
            let scope = SkillScope::from(*scope);
            let result = manager
                .install_git(
                    &git_request(repository_url, commit, subdirectory),
                    scope,
                    workspace_for_scope(scope, workspace)?,
                    approval_digest,
                    *yes,
                )
                .map_err(|error| error.to_string())?;
            println!(
                "Installed /{} ({})",
                result.command,
                result.active_sha256.unwrap_or_default()
            );
            Ok(())
        }
        SkillsCmd::Install {
            name,
            scope,
            approval_digest,
            yes,
        } => {
            let scope = SkillScope::from(*scope);
            let request = community_skill_git_request(name).map_err(|error| error.to_string())?;
            match approval_digest {
                None => {
                    let preview = manager
                        .preview_git(&request, scope, workspace_for_scope(scope, workspace)?)
                        .map_err(|error| error.to_string())?;
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&preview).map_err(|error| error.to_string())?
                    );
                    println!(
                        "\nRun again with --approval-digest <digest from above> --yes to install."
                    );
                    Ok(())
                }
                Some(approval_digest) => {
                    let result = manager
                        .install_git(
                            &request,
                            scope,
                            workspace_for_scope(scope, workspace)?,
                            approval_digest,
                            *yes,
                        )
                        .map_err(|error| error.to_string())?;
                    println!(
                        "Installed /{} ({})",
                        result.command,
                        result.active_sha256.unwrap_or_default()
                    );
                    Ok(())
                }
            }
        }
        SkillsCmd::Enable { command, scope } | SkillsCmd::Disable { command, scope } => {
            let scope = SkillScope::from(*scope);
            let enabled = matches!(action, SkillsCmd::Enable { .. });
            manager
                .set_enabled(
                    scope,
                    workspace_for_scope(scope, workspace)?,
                    command,
                    enabled,
                )
                .map_err(|error| error.to_string())?;
            println!(
                "/{} {}",
                command.trim_start_matches('/'),
                if enabled { "enabled" } else { "disabled" }
            );
            Ok(())
        }
        SkillsCmd::Rollback { command, scope } => {
            let scope = SkillScope::from(*scope);
            let result = manager
                .rollback(scope, workspace_for_scope(scope, workspace)?, command)
                .map_err(|error| error.to_string())?;
            println!(
                "Rolled back /{} to {}",
                result.command,
                result.active_sha256.unwrap_or_default()
            );
            Ok(())
        }
        SkillsCmd::Uninstall {
            command,
            scope,
            yes,
        } => {
            if !yes {
                return Err("Uninstall requires --yes".to_string());
            }
            let scope = SkillScope::from(*scope);
            manager
                .uninstall(scope, workspace_for_scope(scope, workspace)?, command)
                .map_err(|error| error.to_string())?;
            println!(
                "Uninstalled /{}; rollback history retained",
                command.trim_start_matches('/')
            );
            Ok(())
        }
        SkillsCmd::Learned(action) => run_learned(action, data_dir, workspace),
        SkillsCmd::Activation(action) => run_activation(action, data_dir),
    }
}

fn run_activation(action: &ActivationCmd, data_dir: &Path) -> Result<(), String> {
    let store = SkillActivationStore::new(data_dir).map_err(|error| error.to_string())?;
    match action {
        ActivationCmd::List { json } => {
            let entries = store.list().map_err(|error| error.to_string())?;
            if *json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&entries).map_err(|error| error.to_string())?
                );
            } else {
                for entry in entries {
                    println!(
                        "{}  {:?}  pinned={}",
                        entry.key, entry.preference.policy, entry.preference.pinned
                    );
                }
            }
            Ok(())
        }
        ActivationCmd::Get { key } => {
            let entry = store.get(key).map_err(|error| error.to_string())?;
            println!(
                "{}",
                serde_json::to_string_pretty(&entry).map_err(|error| error.to_string())?
            );
            Ok(())
        }
        ActivationCmd::Set {
            key,
            policy,
            pinned,
        } => {
            let entry = store
                .set(key, SkillActivationPolicy::from(*policy), *pinned)
                .map_err(|error| error.to_string())?;
            println!(
                "{}  {:?}  pinned={}",
                entry.key, entry.preference.policy, entry.preference.pinned
            );
            Ok(())
        }
    }
}

/// The CLI half of the learning loop. Same durable store the desktop drives —
/// this module only renders it, so a candidate staged in the app is promotable
/// here and vice versa.
fn run_learned(
    action: &LearnedCmd,
    data_dir: &Path,
    workspace: Option<&Path>,
) -> Result<(), String> {
    let store = SkillLearningStore::new(data_dir).map_err(|error| error.to_string())?;
    let manager = manager(data_dir)?;
    let packages = external_package_skills(data_dir)?;
    match action {
        LearnedCmd::Mode { mode } => {
            let current = match mode {
                Some(mode) => store
                    .set_mode(LearningMode::from(*mode))
                    .map_err(|error| error.to_string())?,
                None => store.mode().map_err(|error| error.to_string())?,
            };
            println!("Learning mode: {current:?}");
            Ok(())
        }
        LearnedCmd::Policy { policy } => {
            let current = store.settings().map_err(|error| error.to_string())?;
            let next = match policy {
                Some(policy) => store
                    .set_settings(LearningSettings {
                        policy: LearningPolicy::from(*policy),
                        allow_global_scope: current.allow_global_scope,
                    })
                    .map_err(|error| error.to_string())?,
                None => current,
            };
            println!("Learning policy: {:?}", next.policy);
            Ok(())
        }
        LearnedCmd::List { json } => {
            let summaries = store
                .learned_skills(&manager, workspace, &packages)
                .map_err(|error| error.to_string())?;
            if *json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&summaries).map_err(|error| error.to_string())?
                );
                return Ok(());
            }
            if summaries.is_empty() {
                println!("No learned skills are installed.");
                return Ok(());
            }
            for summary in summaries {
                println!(
                    "/{:<24} {:<8} {} uses, {} failures, {} corrections{}",
                    summary.command,
                    summary.version,
                    summary.uses,
                    summary.failures,
                    summary.corrections,
                    if summary.deprecated {
                        " [deprecated]"
                    } else if !summary.enabled {
                        " [disabled]"
                    } else {
                        ""
                    }
                );
                println!(
                    "  hash {} from {} (runs {})",
                    summary.active_sha256,
                    summary.provenance.source_kind,
                    summary.provenance.source_run_ids.join(", ")
                );
                if !summary.previous_sha256.is_empty() {
                    println!(
                        "  previous versions: {}",
                        summary.previous_sha256.join(", ")
                    );
                }
            }
            Ok(())
        }
        LearnedCmd::Candidates { json } => {
            let candidates = store.list_candidates().map_err(|error| error.to_string())?;
            if *json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&candidates).map_err(|error| error.to_string())?
                );
                return Ok(());
            }
            if candidates.is_empty() {
                println!("No learning candidates.");
                return Ok(());
            }
            for candidate in candidates {
                println!(
                    "{}  {:?}  /{}  {}",
                    candidate.candidate_id,
                    candidate.status,
                    if candidate.proposed_command.is_empty() {
                        "(not drafted)".to_string()
                    } else {
                        candidate.proposed_command.clone()
                    },
                    candidate.title
                );
                println!("  why: {}", candidate.signal_summary);
            }
            Ok(())
        }
        LearnedCmd::Inspect { candidate_id } => {
            let candidate = store
                .candidate(candidate_id)
                .map_err(|error| error.to_string())?;
            let evaluations = store
                .evaluations_for(candidate_id)
                .map_err(|error| error.to_string())?;
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "candidate": candidate,
                    "evaluations": evaluations,
                }))
                .map_err(|error| error.to_string())?
            );
            Ok(())
        }
        LearnedCmd::Evaluate {
            candidate_id,
            report,
        } => {
            let plan = store
                .plan_evaluation(candidate_id)
                .map_err(|error| error.to_string())?;
            let Some(report) = report else {
                let record = store
                    .mark_unevaluated(
                        &plan.evaluation_id,
                        "no agent runtime was supplied to the CLI; pass --report with a runtime's case reports",
                    )
                    .map_err(|error| error.to_string())?;
                println!("{} {}", record.evaluation_id, record.summary);
                return Ok(());
            };
            let bytes = std::fs::read(report)
                .map_err(|error| format!("read {}: {error}", report.display()))?;
            let reports: Vec<EvaluationCaseReport> =
                serde_json::from_slice(&bytes).map_err(|error| error.to_string())?;
            // A CLI-supplied report file describes what some runtime did; it
            // is recorded as a preflight result, which can never carry a
            // promotion-grade pass. Only the app's own isolated executor,
            // which really runs the arms, reports `real_isolated`.
            let record = store
                .report_evaluation(
                    &plan.evaluation_id,
                    little_monkey_lib::skill_learning::EvaluationMode::Preflight,
                    &reports,
                )
                .map_err(|error| error.to_string())?;
            println!("{:?}: {}", record.verdict, record.summary);
            Ok(())
        }
        LearnedCmd::Promote { candidate_id, yes } => {
            if !yes {
                let candidate = store
                    .candidate(candidate_id)
                    .map_err(|error| error.to_string())?;
                let policy = candidate.policy.unwrap_or_else(|| {
                    little_monkey_lib::skill_learning::PromotionPolicy {
                        auto_promote_allowed: false,
                        requires_approval: true,
                        blocking: Vec::new(),
                        approval_reasons: vec!["the candidate has not been staged".to_string()],
                    }
                });
                println!("/{} — {}", candidate.proposed_command, candidate.title);
                println!("  digest: {}", candidate.candidate_sha256);
                if !policy.blocking.is_empty() {
                    println!("  refused: {}", policy.blocking.join("; "));
                }
                if !policy.approval_reasons.is_empty() {
                    println!("  needs approval: {}", policy.approval_reasons.join("; "));
                }
                return Err("Promotion requires --yes".to_string());
            }
            // `--yes` is a real, explicit user decision, and it produces a
            // real approval record: an id that is auditable afterwards and a
            // digest bound to exactly this version of the candidate. If the
            // candidate is re-staged between the two calls, the digest no
            // longer matches and the promotion parks instead of installing.
            let candidate = store
                .candidate(candidate_id)
                .map_err(|error| error.to_string())?;
            let grant = little_monkey_lib::skill_learning::ApprovalGrant {
                approval_id: format!("cli:{}", uuid::Uuid::new_v4().simple()),
                operation_sha256: little_monkey_lib::skill_learning::approval_operation_digest(
                    &candidate,
                ),
            };
            let outcome = store
                .promote(candidate_id, Some(&grant), false, &manager, workspace)
                .map_err(|error| error.to_string())?;
            match outcome {
                PromotionOutcome::Promoted {
                    candidate,
                    mutation,
                } => {
                    println!(
                        "Installed /{} at {}",
                        mutation.command,
                        candidate.installed_sha256.unwrap_or_default()
                    );
                    Ok(())
                }
                PromotionOutcome::AwaitingApproval { reasons, .. } => {
                    Err(format!("Awaiting approval: {}", reasons.join("; ")))
                }
                PromotionOutcome::Refused { reasons, .. } => {
                    Err(format!("Refused: {}", reasons.join("; ")))
                }
            }
        }
        LearnedCmd::Reject {
            candidate_id,
            reason,
        } => {
            let candidate = store
                .reject(candidate_id, reason)
                .map_err(|error| error.to_string())?;
            println!("Rejected {}", candidate.candidate_id);
            Ok(())
        }
        LearnedCmd::Deprecate {
            command,
            scope,
            reason,
        } => {
            let scope = SkillScope::from(*scope);
            let mutation = store
                .deprecate(
                    command.trim_start_matches('/'),
                    scope,
                    reason,
                    &manager,
                    workspace_for_scope(scope, workspace)?,
                    &packages,
                )
                .map_err(|error| error.to_string())?;
            println!("Deprecated /{}; history retained", mutation.command);
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn skill(command: &str) -> CliSkill {
        CliSkill {
            id: command.to_string(),
            command: command.to_string(),
            name: command.to_string(),
            version: "1.0.0".to_string(),
            sha256: "a".repeat(64),
            instructions: format!("instructions for {command}"),
            source: "test".to_string(),
            permissions: BTreeSet::new(),
        }
    }

    #[test]
    fn composes_stacked_explicit_skills_and_preserves_request() {
        let result = compose_for_prompt(
            Some("base"),
            "/review /concise inspect this",
            &[skill("review"), skill("concise")],
        )
        .expect("compose")
        .expect("system");
        assert!(result.contains("base"));
        assert!(result.contains("instructions for review"));
        assert!(result.contains("instructions for concise"));
        assert!(result.contains("Arguments/request:\ninspect this"));
    }

    #[test]
    fn unknown_slash_text_is_not_consumed() {
        assert_eq!(
            compose_for_prompt(Some("base"), "/unknown path", &[skill("review")]).unwrap(),
            Some("base".to_string())
        );
    }
}
