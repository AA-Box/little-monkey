//! Headless executable-extension lifecycle and developer tooling.
//! Runtime secrets are read from stdin, never argv. Developer signing keys are
//! referenced by path and are never imported into the application's trust store.

#[path = "extension_dev_cli.rs"]
mod extension_dev_cli;

use little_monkey_lib::executable_extensions::{
    Approval, ExtensionManager, InvocationRequest, PermissionGrant,
};
use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};

#[derive(clap::Subcommand, Debug)]
pub enum ExtensionsCmd {
    /// Create a standalone compilable extension project.
    Init {
        #[arg(default_value = ".")]
        path: PathBuf,
        #[arg(long = "id")]
        extension_id: Option<String>,
        #[arg(long)]
        name: Option<String>,
        #[arg(long, value_enum, default_value_t = extension_dev_cli::DevTemplate::Tool)]
        template: extension_dev_cli::DevTemplate,
        #[arg(long, default_value = "Local Developer")]
        publisher: String,
        #[arg(long)]
        json: bool,
    },
    /// Build, validate, launch in an isolated development profile, stream logs,
    /// and hot-restart when project files change.
    Dev {
        #[arg(default_value = ".")]
        source: PathBuf,
        #[arg(long)]
        capability: Option<String>,
        /// JSON input or @PATH for the optional capability invocation.
        #[arg(long, default_value = "{}")]
        input: String,
        /// Build/start once instead of watching for changes.
        #[arg(long)]
        once: bool,
    },
    /// Run extension.test.json/extension.tests.json conformance in an isolated profile.
    Test {
        #[arg(default_value = ".")]
        source: PathBuf,
        #[arg(long)]
        json: bool,
    },
    /// Validate a project/bundle path or an already-installed extension id.
    Validate {
        target: String,
        #[arg(long)]
        json: bool,
    },
    /// Build and deterministically package an extension as .lmx.
    Pack {
        #[arg(default_value = ".")]
        source: PathBuf,
        #[arg(long)]
        output: Option<PathBuf>,
        #[arg(long)]
        json: bool,
    },
    /// Sign a deterministic .lmx using an Ed25519 PKCS#8 PEM publisher key.
    Sign {
        package: PathBuf,
        #[arg(long = "private-key")]
        private_key: PathBuf,
        #[arg(long = "trust-root-id")]
        trust_root_id: String,
        #[arg(long = "key-id")]
        key_id: String,
        #[arg(long)]
        output: Option<PathBuf>,
        #[arg(long)]
        json: bool,
    },
    /// Run conformance, build, sign, publish to a static M4 registry and re-sign
    /// the registry snapshot. Registry and publisher private keys remain outside
    /// the application and are only read for this explicit command.
    Publish {
        #[arg(default_value = ".")]
        source: PathBuf,
        #[arg(long)]
        snapshot: PathBuf,
        #[arg(long = "registry-root")]
        registry_root: PathBuf,
        #[arg(long = "publisher-private-key")]
        publisher_private_key: PathBuf,
        #[arg(long = "trust-root-id")]
        trust_root_id: String,
        #[arg(long = "key-id")]
        key_id: String,
        #[arg(long = "registry-private-key")]
        registry_private_key: PathBuf,
        #[arg(long)]
        output: Option<PathBuf>,
        #[arg(long, default_value_t = 24)]
        refresh_hours: u64,
        #[arg(long, default_value_t = 7)]
        expiry_days: u64,
        #[arg(long)]
        json: bool,
    },
    Discover {
        source: PathBuf,
        #[arg(long)]
        json: bool,
    },
    List {
        #[arg(long)]
        json: bool,
    },
    Inspect {
        extension_id: String,
        #[arg(long)]
        json: bool,
    },
    Install {
        source: PathBuf,
        #[arg(long)]
        approval_digest: String,
        #[arg(long = "grant")]
        grants: Vec<String>,
        #[arg(long = "workspace", value_name = "PERMISSION_ID=PATH")]
        workspaces: Vec<String>,
        #[arg(long)]
        allow_unsigned: bool,
        #[arg(long)]
        allow_untrusted: bool,
        #[arg(long)]
        allow_high_risk: bool,
        #[arg(long)]
        json: bool,
    },
    Enable {
        extension_id: String,
        #[arg(long)]
        off: bool,
        #[arg(long)]
        json: bool,
    },
    Start {
        extension_id: String,
        #[arg(long)]
        json: bool,
    },
    Stop {
        extension_id: String,
        #[arg(long)]
        json: bool,
    },
    PreviewUpdate {
        source: PathBuf,
        #[arg(long)]
        json: bool,
    },
    Update {
        source: PathBuf,
        #[arg(long)]
        approval_digest: String,
        #[arg(long = "grant")]
        grants: Vec<String>,
        #[arg(long = "workspace", value_name = "PERMISSION_ID=PATH")]
        workspaces: Vec<String>,
        #[arg(long)]
        allow_unsigned: bool,
        #[arg(long)]
        allow_untrusted: bool,
        #[arg(long)]
        allow_high_risk: bool,
        #[arg(long)]
        json: bool,
    },
    Rollback {
        extension_id: String,
        #[arg(long)]
        json: bool,
    },
    Uninstall {
        extension_id: String,
    },
    Health {
        extension_id: String,
        #[arg(long)]
        json: bool,
    },
    Logs {
        extension_id: String,
        #[arg(long, default_value_t = 50)]
        limit: u32,
        #[arg(long)]
        json: bool,
    },
    SetConfig {
        extension_id: String,
        /// A JSON object, or @PATH to read one from a file.
        values: String,
        #[arg(long)]
        json: bool,
    },
    SetSecret {
        extension_id: String,
        slot_id: String,
    },
    RemoveSecret {
        extension_id: String,
        slot_id: String,
    },
    Invoke {
        extension_id: String,
        capability_id: String,
        /// JSON input, or @PATH to read it from a file.
        input: String,
        #[arg(long)]
        invocation_id: Option<String>,
        #[arg(long = "artifact")]
        input_artifact_ids: Vec<String>,
        #[arg(long)]
        json: bool,
    },
    Cancel {
        invocation_id: String,
    },
}

pub async fn run(action: &ExtensionsCmd, app_data: &Path) -> Result<(), String> {
    let manager = ExtensionManager::new(app_data)?;
    match action {
        ExtensionsCmd::Init {
            path,
            extension_id,
            name,
            template,
            publisher,
            json,
        } => extension_dev_cli::init(
            path,
            extension_id.as_deref(),
            name.as_deref(),
            *template,
            publisher,
            *json,
        ),
        ExtensionsCmd::Dev {
            source,
            capability,
            input,
            once,
        } => extension_dev_cli::dev(source, capability.as_deref(), input, *once).await,
        ExtensionsCmd::Test { source, json } => {
            extension_dev_cli::test_extension(source, *json).await
        }
        ExtensionsCmd::Validate { target, json } => {
            extension_dev_cli::validate(&manager, target, *json).await
        }
        ExtensionsCmd::Pack {
            source,
            output,
            json,
        } => extension_dev_cli::pack(source, output.as_deref(), *json),
        ExtensionsCmd::Sign {
            package,
            private_key,
            trust_root_id,
            key_id,
            output,
            json,
        } => extension_dev_cli::sign(
            package,
            private_key,
            trust_root_id,
            key_id,
            output.as_deref(),
            *json,
        ),
        ExtensionsCmd::Publish {
            source,
            snapshot,
            registry_root,
            publisher_private_key,
            trust_root_id,
            key_id,
            registry_private_key,
            output,
            refresh_hours,
            expiry_days,
            json,
        } => {
            extension_dev_cli::publish(
                source,
                snapshot,
                registry_root,
                publisher_private_key,
                trust_root_id,
                key_id,
                registry_private_key,
                output.as_deref(),
                *refresh_hours,
                *expiry_days,
                *json,
            )
            .await
        }
        ExtensionsCmd::Discover { source, json } => print(&manager.discover(source)?, *json),
        ExtensionsCmd::List { json } => print(&manager.list()?, *json),
        ExtensionsCmd::Inspect { extension_id, json }
        | ExtensionsCmd::Health { extension_id, json } => {
            print(&manager.inspect(extension_id)?, *json)
        }
        ExtensionsCmd::Install {
            source,
            approval_digest,
            grants,
            workspaces,
            allow_unsigned,
            allow_untrusted,
            allow_high_risk,
            json,
        } => {
            let approval = approval(
                approval_digest,
                grants,
                workspaces,
                *allow_unsigned,
                *allow_untrusted,
                *allow_high_risk,
            )?;
            print(&manager.install(source, approval).await?, *json)
        }
        ExtensionsCmd::Enable {
            extension_id,
            off,
            json,
        } => print(&manager.set_enabled(extension_id, !off).await?, *json),
        ExtensionsCmd::Start { extension_id, json } => {
            print(&manager.set_running(extension_id, true).await?, *json)
        }
        ExtensionsCmd::Stop { extension_id, json } => {
            print(&manager.set_running(extension_id, false).await?, *json)
        }
        ExtensionsCmd::PreviewUpdate { source, json } => {
            print(&manager.preview_update(source)?, *json)
        }
        ExtensionsCmd::Update {
            source,
            approval_digest,
            grants,
            workspaces,
            allow_unsigned,
            allow_untrusted,
            allow_high_risk,
            json,
        } => {
            let approval = approval(
                approval_digest,
                grants,
                workspaces,
                *allow_unsigned,
                *allow_untrusted,
                *allow_high_risk,
            )?;
            print(&manager.update(source, approval).await?, *json)
        }
        ExtensionsCmd::Rollback { extension_id, json } => {
            print(&manager.rollback(extension_id).await?, *json)
        }
        ExtensionsCmd::Uninstall { extension_id } => {
            manager.uninstall(extension_id)?;
            println!("Extension '{extension_id}' removed.");
            Ok(())
        }
        ExtensionsCmd::Logs {
            extension_id,
            limit,
            json,
        } => print(&manager.logs(extension_id, *limit)?, *json),
        ExtensionsCmd::SetConfig {
            extension_id,
            values,
            json,
        } => {
            let text = read_json_arg(values)?;
            let values: BTreeMap<String, serde_json::Value> = serde_json::from_str(&text)
                .map_err(|error| format!("Configuration must be a JSON object: {error}"))?;
            print(&manager.set_config(extension_id, values)?, *json)
        }
        ExtensionsCmd::SetSecret {
            extension_id,
            slot_id,
        } => {
            let mut secret = String::new();
            std::io::stdin()
                .read_to_string(&mut secret)
                .map_err(|error| format!("Cannot read secret from stdin: {error}"))?;
            while secret.ends_with(['\n', '\r']) {
                secret.pop();
            }
            manager.set_secret(extension_id, slot_id, &secret)?;
            println!("Secret slot configured.");
            Ok(())
        }
        ExtensionsCmd::RemoveSecret {
            extension_id,
            slot_id,
        } => {
            manager.remove_secret(extension_id, slot_id)?;
            println!("Secret slot cleared.");
            Ok(())
        }
        ExtensionsCmd::Invoke {
            extension_id,
            capability_id,
            input,
            invocation_id,
            input_artifact_ids,
            json,
        } => {
            let input_json = read_json_arg(input)?;
            serde_json::from_str::<serde_json::Value>(&input_json)
                .map_err(|error| format!("Invocation input must be JSON: {error}"))?;
            print(
                &manager
                    .invoke(InvocationRequest {
                        extension_id: extension_id.clone(),
                        capability_id: capability_id.clone(),
                        input_json,
                        invocation_id: invocation_id.clone(),
                        input_artifact_ids: input_artifact_ids.clone(),
                        expected_kind: None,
                        expected_version: None,
                    })
                    .await?,
                *json,
            )
        }
        ExtensionsCmd::Cancel { invocation_id } => {
            println!(
                "{}",
                if manager.cancel_invocation(invocation_id)? {
                    "Cancellation requested."
                } else {
                    "No matching active invocation."
                }
            );
            Ok(())
        }
    }
}

fn approval(
    digest: &str,
    grants: &[String],
    workspaces: &[String],
    allow_unsigned: bool,
    allow_untrusted: bool,
    allow_high_risk: bool,
) -> Result<Approval, String> {
    let mut result = grants
        .iter()
        .map(|permission_id| PermissionGrant {
            permission_id: permission_id.clone(),
            binding: None,
        })
        .collect::<Vec<_>>();
    for binding in workspaces {
        let (permission_id, path) = binding
            .split_once('=')
            .ok_or_else(|| "Workspace grants must be PERMISSION_ID=PATH".to_string())?;
        result.push(PermissionGrant {
            permission_id: permission_id.to_string(),
            binding: Some(path.to_string()),
        });
    }
    Ok(Approval {
        approval_digest: digest.to_string(),
        grants: result,
        allow_unsigned,
        allow_untrusted,
        allow_high_risk,
    })
}

fn read_json_arg(value: &str) -> Result<String, String> {
    if let Some(path) = value.strip_prefix('@') {
        std::fs::read_to_string(path).map_err(|error| format!("Cannot read '{path}': {error}"))
    } else {
        Ok(value.to_string())
    }
}

fn print<T: serde::Serialize + std::fmt::Debug>(value: &T, json: bool) -> Result<(), String> {
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(value).map_err(|error| error.to_string())?
        );
    } else {
        println!("{value:#?}");
    }
    Ok(())
}
