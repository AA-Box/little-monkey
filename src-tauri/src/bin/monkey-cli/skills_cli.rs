//! Native/package/local-prompt skill parity for the CLI. The runtime is the
//! same data-only `NativeSkillManager` used by Tauri; this module only adds
//! Clap rendering and turn-scoped prompt composition.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use clap::{Subcommand, ValueEnum};
use little_monkey_lib::native_skills::{
    ExternalSignedSkill, GitSkillRequest, NativeSkillManager, SkillDescriptor, SkillScope,
};
use little_monkey_lib::prompts::PromptEntry;

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
