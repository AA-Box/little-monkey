use clap::{Args, Subcommand};
use little_monkey_lib::app_paths;
use little_monkey_lib::execution_target::{
    apply_execution_result, apply_workspace_result, discard_workspace_result,
    load_execution_result, runner_probe, runner_serve_stdio, ExecutionTargetKind, SshRunnerConfig,
    TargetCapabilities, TargetConfig, TargetError, TargetIdentity, TargetRegistry, WorkspaceResult,
    WorkspaceTransfer, EXECUTION_PROTOCOL_VERSION,
};
use sha2::{Digest, Sha256};
use std::path::PathBuf;

#[derive(Subcommand, Debug, Clone)]
pub enum TargetsCmd {
    List {
        #[arg(long)]
        json: bool,
    },
    Probe {
        id: String,
    },
    Add {
        #[command(subcommand)]
        target: TargetAddCmd,
    },
    Remove {
        id: String,
    },
}

#[derive(Subcommand, Debug, Clone)]
pub enum TargetAddCmd {
    Docker(AddDockerArgs),
    Ssh(AddSshArgs),
}

#[derive(Args, Debug, Clone)]
pub struct AddDockerArgs {
    pub id: String,
    pub image: String,
    #[arg(long)]
    pub name: Option<String>,
    #[arg(long)]
    pub runner_data: Option<PathBuf>,
}

#[derive(Args, Debug, Clone)]
pub struct AddSshArgs {
    pub id: String,
    pub host: String,
    #[arg(long)]
    pub user: Option<String>,
    #[arg(long)]
    pub port: Option<u16>,
    #[arg(long)]
    pub key_file: Option<PathBuf>,
    #[arg(long)]
    pub known_hosts: PathBuf,
    #[arg(long)]
    pub jump_host: Option<String>,
    #[arg(long)]
    pub name: Option<String>,
    #[arg(long)]
    pub runner_data: Option<PathBuf>,
}

#[derive(Subcommand, Debug, Clone)]
pub enum WorkspaceCmd {
    Push {
        path: PathBuf,
        #[arg(long)]
        workspace_id: Option<String>,
        #[arg(long)]
        output: Option<PathBuf>,
    },
    Result {
        result: PathBuf,
        #[arg(long)]
        workspace: PathBuf,
        #[arg(long)]
        base_digest: String,
    },
    /// Review a persisted remote result without changing the local workspace.
    Review { result_id: String },
    /// Apply a persisted result only after the frozen-base conflict check.
    Apply {
        result_id: String,
        workspace: PathBuf,
    },
    /// Export a persisted result for review or handoff.
    Export { result_id: String, output: PathBuf },
    /// Discard a persisted remote result and its local copy.
    Discard { result_id: String },
}

#[derive(Subcommand, Debug, Clone)]
pub enum RunnerCmd {
    Probe,
    Serve {
        #[arg(long)]
        stdio: bool,
    },
}

fn registry_path() -> Result<PathBuf, String> {
    app_paths::data_dir()
        .map(|path| path.join("execution-targets.json"))
        .ok_or_else(|| "Could not resolve the Little Monkey app data directory".to_string())
}

fn runner_data(path: Option<PathBuf>) -> Result<PathBuf, String> {
    match path {
        Some(path) if path.is_absolute() => Ok(path),
        Some(_) => Err("runner data path must be absolute".to_string()),
        None => app_paths::data_dir()
            .map(|path| path.join("execution-runner"))
            .ok_or_else(|| "Could not resolve the Little Monkey app data directory".to_string()),
    }
}

fn identity(
    id: String,
    name: Option<String>,
    kind: ExecutionTargetKind,
    capabilities: TargetCapabilities,
) -> TargetIdentity {
    TargetIdentity {
        stable_id: id,
        display_name: name.unwrap_or_else(|| format!("{kind:?}")),
        kind,
        endpoint: None,
        verified_identity: None,
        platform: "unknown".into(),
        runner_version: "unknown".into(),
        protocol_version: EXECUTION_PROTOCOL_VERSION,
        capabilities,
        last_successful_probe_ms: None,
        trust_state: little_monkey_lib::execution_target::TargetTrustState::Unverified,
    }
}

fn print_json<T: serde::Serialize>(value: &T) -> Result<(), String> {
    println!(
        "{}",
        serde_json::to_string_pretty(value).map_err(|error| error.to_string())?
    );
    Ok(())
}

pub fn targets(command: TargetsCmd) -> Result<(), String> {
    let path = registry_path()?;
    let mut registry = TargetRegistry::load(&path).map_err(|error| error.to_string())?;
    match command {
        TargetsCmd::List { json } => {
            if json {
                return print_json(&registry.targets);
            }
            for (id, config) in &registry.targets {
                println!(
                    "{id}\t{:?}\t{}",
                    config.identity().kind,
                    config.identity().display_name
                );
            }
            Ok(())
        }
        TargetsCmd::Probe { id } => {
            let previous = registry
                .get(&id)
                .map_err(|error| error.to_string())?
                .identity()
                .clone();
            let target = registry
                .get(&id)
                .map_err(|error| error.to_string())?
                .target()
                .map_err(|error| error.to_string())?;
            let snapshot = target.probe().map_err(|error| error.to_string())?;
            if previous
                .verified_identity
                .as_ref()
                .zip(snapshot.identity.verified_identity.as_ref())
                .is_some_and(|(before, after)| before != after)
            {
                if let Some(config) = registry.targets.get_mut(&id) {
                    match config {
                        TargetConfig::Local { identity }
                        | TargetConfig::Docker { identity, .. }
                        | TargetConfig::RemoteNode { identity }
                        | TargetConfig::SshRunner { identity, .. } => {
                            identity.trust_state =
                                little_monkey_lib::execution_target::TargetTrustState::Changed;
                        }
                    }
                }
                registry.save(&path).map_err(|error| error.to_string())?;
                return Err(TargetError::TargetIdentityChanged(format!(
                    "target '{id}' identity changed during probe"
                ))
                .to_string());
            }
            if let Some(config) = registry.targets.get_mut(&id) {
                match config {
                    TargetConfig::Local { identity }
                    | TargetConfig::Docker { identity, .. }
                    | TargetConfig::RemoteNode { identity }
                    | TargetConfig::SshRunner { identity, .. } => {
                        *identity = snapshot.identity.clone()
                    }
                }
            }
            registry.save(&path).map_err(|error| error.to_string())?;
            print_json(&snapshot)
        }
        TargetsCmd::Add {
            target: TargetAddCmd::Docker(args),
        } => {
            let data = runner_data(args.runner_data)?;
            let config = TargetConfig::Docker {
                identity: identity(
                    args.id,
                    args.name,
                    ExecutionTargetKind::Docker,
                    TargetCapabilities::docker(),
                ),
                image: args.image,
                runner_data: data,
            };
            registry.add(config).map_err(|error| error.to_string())?;
            registry.save(&path).map_err(|error| error.to_string())
        }
        TargetsCmd::Add {
            target: TargetAddCmd::Ssh(args),
        } => {
            let data = runner_data(args.runner_data)?;
            let config = TargetConfig::SshRunner {
                identity: identity(
                    args.id,
                    args.name,
                    ExecutionTargetKind::SshRunner,
                    TargetCapabilities::default(),
                ),
                config: SshRunnerConfig {
                    host: args.host,
                    user: args.user,
                    port: args.port,
                    key_file: args.key_file,
                    known_hosts: args.known_hosts,
                    jump_host: args.jump_host,
                    runner_binary: "monkey".into(),
                },
                runner_data: data,
            };
            registry.add(config).map_err(|error| error.to_string())?;
            registry.save(&path).map_err(|error| error.to_string())
        }
        TargetsCmd::Remove { id } => {
            registry.remove(&id).map_err(|error| error.to_string())?;
            registry.save(&path).map_err(|error| error.to_string())
        }
    }
}

pub fn workspace(command: WorkspaceCmd) -> Result<(), String> {
    match command {
        WorkspaceCmd::Push {
            path,
            workspace_id,
            output,
        } => {
            let path = path.canonicalize().map_err(|error| error.to_string())?;
            let path_digest = format!("{:x}", Sha256::digest(path.to_string_lossy().as_bytes()));
            let id = workspace_id.unwrap_or_else(|| format!("workspace-{}", &path_digest[..24]));
            let transfer =
                WorkspaceTransfer::from_workspace(&path, &id).map_err(|error| error.to_string())?;
            let bytes = serde_json::to_vec_pretty(&transfer).map_err(|error| error.to_string())?;
            if let Some(output) = output {
                std::fs::write(output, bytes).map_err(|error| error.to_string())?;
            } else {
                println!("{}", String::from_utf8_lossy(&bytes));
            }
            Ok(())
        }
        WorkspaceCmd::Result {
            result,
            workspace,
            base_digest,
        } => {
            let bytes = std::fs::read(result).map_err(|error| error.to_string())?;
            let result: WorkspaceResult =
                serde_json::from_slice(&bytes).map_err(|error| error.to_string())?;
            apply_workspace_result(&workspace, &base_digest, &result)
                .map_err(|error| error.to_string())
        }
        WorkspaceCmd::Review { result_id } => {
            let data_dir = app_paths::data_dir().ok_or("Could not resolve app data directory")?;
            print_json(
                &load_execution_result(&data_dir, &result_id).map_err(|error| error.to_string())?,
            )
        }
        WorkspaceCmd::Apply {
            result_id,
            workspace,
        } => {
            let data_dir = app_paths::data_dir().ok_or("Could not resolve app data directory")?;
            let result =
                load_execution_result(&data_dir, &result_id).map_err(|error| error.to_string())?;
            apply_execution_result(&workspace, &result).map_err(|error| error.to_string())
        }
        WorkspaceCmd::Export { result_id, output } => {
            let data_dir = app_paths::data_dir().ok_or("Could not resolve app data directory")?;
            let result =
                load_execution_result(&data_dir, &result_id).map_err(|error| error.to_string())?;
            let bytes = serde_json::to_vec_pretty(&result).map_err(|error| error.to_string())?;
            std::fs::write(output, bytes).map_err(|error| error.to_string())
        }
        WorkspaceCmd::Discard { result_id } => {
            let data_dir = app_paths::data_dir().ok_or("Could not resolve app data directory")?;
            discard_workspace_result(&data_dir, &result_id).map_err(|error| error.to_string())
        }
    }
}

pub fn runner(command: RunnerCmd) -> Result<(), String> {
    match command {
        RunnerCmd::Probe => print_json(&runner_probe().map_err(|error| error.to_string())?),
        RunnerCmd::Serve { stdio } => {
            if !stdio {
                return Err("runner serve requires --stdio".to_string());
            }
            runner_serve_stdio().map_err(|error| error.to_string())
        }
    }
}
