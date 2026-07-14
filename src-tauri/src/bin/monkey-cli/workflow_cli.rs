//! Headless client for the same persisted M4 workflow service used by the
//! desktop visual editor. Commands intentionally exchange the canonical
//! workflow/run JSON contracts instead of inventing a CLI-only schema.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use clap::Subcommand;
use little_monkey_lib::m4_services::WorkflowService;
use little_monkey_lib::workflow_core::{
    SecretBinding, WorkflowDefinition, WorkflowRunRequest, WorkflowTrigger, WorkflowValue,
};
use serde::de::DeserializeOwned;
use serde_json::{json, Value};
use uuid::Uuid;

const MAX_DEFINITION_BYTES: u64 = 16 * 1024 * 1024;
const MAX_ARGUMENT_BYTES: usize = 4 * 1024 * 1024;

#[derive(Subcommand, Debug)]
pub(crate) enum WorkflowCmd {
    /// List definitions saved by either the desktop editor or this CLI.
    List,
    /// Validate and compile a workflow definition without persisting it.
    Validate { definition: PathBuf },
    /// Run one saved workflow and append its durable history.
    Run {
        workflow_id: String,
        #[arg(long)]
        run_id: Option<String>,
        /// Canonical JSON map of WorkflowValue inputs, or @path/to/file.json.
        #[arg(long, default_value = "{}")]
        inputs: String,
        /// Canonical JSON map of SecretBinding references, or @path.
        #[arg(long, default_value = "{}")]
        secrets: String,
        /// Canonical WorkflowTrigger JSON, or @path.
        #[arg(long, default_value = "{\"kind\":\"manual\"}")]
        trigger: String,
    },
    /// List durable histories, or inspect one run by id.
    History { run_id: Option<String> },
    /// Replay a saved run from a declared boundary using the source snapshot.
    Replay {
        workflow_id: String,
        source_run_id: String,
        boundary_node_id: String,
        #[arg(long)]
        run_id: Option<String>,
        #[arg(long)]
        approval: bool,
        /// Optional replacement canonical input map; source snapshot by default.
        #[arg(long)]
        inputs: Option<String>,
        /// Optional replacement secret-reference map; source snapshot by default.
        #[arg(long)]
        secrets: Option<String>,
    },
}

fn bounded_read(path: &Path, maximum: u64) -> Result<Vec<u8>, String> {
    let metadata =
        std::fs::metadata(path).map_err(|error| format!("read {}: {error}", path.display()))?;
    if !metadata.is_file() || metadata.len() > maximum {
        return Err(format!(
            "{} is not a regular file at or below {maximum} bytes",
            path.display()
        ));
    }
    std::fs::read(path).map_err(|error| format!("read {}: {error}", path.display()))
}

fn parse_file<T: DeserializeOwned>(path: &Path) -> Result<T, String> {
    serde_json::from_slice(&bounded_read(path, MAX_DEFINITION_BYTES)?)
        .map_err(|error| format!("decode {}: {error}", path.display()))
}

fn parse_argument<T: DeserializeOwned>(raw: &str, label: &str) -> Result<T, String> {
    let bytes = if let Some(path) = raw.strip_prefix('@') {
        bounded_read(Path::new(path), MAX_ARGUMENT_BYTES as u64)?
    } else {
        if raw.len() > MAX_ARGUMENT_BYTES {
            return Err(format!("{label} JSON exceeds {MAX_ARGUMENT_BYTES} bytes"));
        }
        raw.as_bytes().to_vec()
    };
    serde_json::from_slice(&bytes).map_err(|error| format!("decode {label}: {error}"))
}

fn generated_run_id(prefix: &str) -> String {
    format!("{prefix}-{}", Uuid::new_v4().simple())
}

fn execute(service: &WorkflowService, command: &WorkflowCmd) -> Result<Value, String> {
    match command {
        WorkflowCmd::List => serde_json::to_value(service.list().map_err(|e| e.to_string())?)
            .map_err(|error| error.to_string()),
        WorkflowCmd::Validate { definition } => {
            let definition: WorkflowDefinition = parse_file(definition)?;
            serde_json::to_value(service.validate(&definition).map_err(|e| e.to_string())?)
                .map_err(|error| error.to_string())
        }
        WorkflowCmd::Run {
            workflow_id,
            run_id,
            inputs,
            secrets,
            trigger,
        } => {
            let request = WorkflowRunRequest {
                run_id: run_id
                    .clone()
                    .unwrap_or_else(|| generated_run_id("workflow-run")),
                inputs: parse_argument::<BTreeMap<String, WorkflowValue>>(inputs, "inputs")?,
                secret_bindings: parse_argument::<BTreeMap<String, SecretBinding>>(
                    secrets, "secrets",
                )?,
                trigger: parse_argument::<WorkflowTrigger>(trigger, "trigger")?,
            };
            serde_json::to_value(
                service
                    .run_workflow(workflow_id, request)
                    .map_err(|e| e.to_string())?,
            )
            .map_err(|error| error.to_string())
        }
        WorkflowCmd::History {
            run_id: Some(run_id),
        } => serde_json::to_value(service.history(run_id).map_err(|e| e.to_string())?)
            .map_err(|error| error.to_string()),
        WorkflowCmd::History { run_id: None } => {
            serde_json::to_value(service.histories().map_err(|e| e.to_string())?)
                .map_err(|error| error.to_string())
        }
        WorkflowCmd::Replay {
            workflow_id,
            source_run_id,
            boundary_node_id,
            run_id,
            approval,
            inputs,
            secrets,
        } => {
            let source = service.history(source_run_id).map_err(|e| e.to_string())?;
            let request = WorkflowRunRequest {
                run_id: run_id
                    .clone()
                    .unwrap_or_else(|| generated_run_id("workflow-replay")),
                inputs: inputs
                    .as_deref()
                    .map(|raw| parse_argument(raw, "inputs"))
                    .transpose()?
                    .unwrap_or_else(|| source.input_snapshot.clone()),
                secret_bindings: secrets
                    .as_deref()
                    .map(|raw| parse_argument(raw, "secrets"))
                    .transpose()?
                    .unwrap_or_else(|| source.secret_reference_snapshot.clone()),
                trigger: source.trigger.clone(),
            };
            let (plan, history) = service
                .replay(
                    workflow_id,
                    source_run_id,
                    boundary_node_id,
                    *approval,
                    request,
                )
                .map_err(|e| e.to_string())?;
            Ok(json!({ "plan": plan, "history": history }))
        }
    }
}

pub(crate) fn run(command: &WorkflowCmd, app_data_dir: &Path) -> Result<(), String> {
    let service = little_monkey_lib::m4_runtime::production_workflow_service(app_data_dir)?;
    let value = execute(&service, command)?;
    println!(
        "{}",
        serde_json::to_string_pretty(&value).map_err(|error| error.to_string())?
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;
    use std::sync::Arc;

    use little_monkey_lib::m4_runtime::SystemWorkflowClock;
    use little_monkey_lib::workflow_core::{
        workflow_core_fixture_capabilities, workflow_core_fixtures, NodeAdapterResult,
        NodeExecutionRequest, ResourceUsage, WorkflowClock, WorkflowNodeExecutor, WorkflowNodeKind,
        WorkflowRunStatus,
    };
    use tokio_util::sync::CancellationToken;

    struct FixtureExecutor;

    impl WorkflowNodeExecutor for FixtureExecutor {
        fn execute(
            &self,
            request: NodeExecutionRequest,
            _cancel: &CancellationToken,
        ) -> Result<NodeAdapterResult, String> {
            let output = match request.node.kind {
                WorkflowNodeKind::PromptModel { .. } => {
                    WorkflowValue::String("fixture model result".to_string())
                }
                WorkflowNodeKind::Agent { .. } | WorkflowNodeKind::Subagent { .. } => {
                    WorkflowValue::String("fixture agent result".to_string())
                }
                WorkflowNodeKind::HumanApproval { .. } => WorkflowValue::Boolean(true),
                WorkflowNodeKind::Tool { .. }
                | WorkflowNodeKind::Mcp { .. }
                | WorkflowNodeKind::Browser { .. }
                | WorkflowNodeKind::Git { .. }
                | WorkflowNodeKind::PullRequest { .. } => request.inputs["arguments"].clone(),
                WorkflowNodeKind::Shell { .. } => WorkflowValue::Json(json!({"status":0})),
                WorkflowNodeKind::Verify { .. }
                | WorkflowNodeKind::Transform { .. }
                | WorkflowNodeKind::BoundedLoop { .. } => request.inputs["input"].clone(),
                WorkflowNodeKind::Condition => request.inputs["condition"].clone(),
                WorkflowNodeKind::Artifact { .. } => {
                    return Err("artifact fixture is not used by the parity suite".to_string())
                }
                WorkflowNodeKind::Output => request.inputs["value"].clone(),
                WorkflowNodeKind::LegacyRecipe { .. } => {
                    WorkflowValue::String("legacy fixture".to_string())
                }
            };
            Ok(NodeAdapterResult::Succeeded {
                outputs: BTreeMap::from([("out".to_string(), output)]),
                usage: ResourceUsage::default(),
            })
        }
    }

    fn service(root: &Path) -> WorkflowService {
        WorkflowService::new(
            root,
            BTreeSet::new(),
            workflow_core_fixture_capabilities(),
            Arc::new(FixtureExecutor),
            Arc::new(SystemWorkflowClock) as Arc<dyn WorkflowClock>,
            None,
        )
        .unwrap()
    }

    #[test]
    fn all_visual_editor_fixtures_run_through_the_cli_service_contract() {
        let root = std::env::temp_dir().join(format!("lm-workflow-cli-{}", Uuid::new_v4()));
        let service = service(&root);
        let fixtures = workflow_core_fixtures();
        assert_eq!(fixtures.len(), 5);
        for fixture in fixtures {
            service.create(fixture.workflow.clone()).unwrap();
            let value = execute(
                &service,
                &WorkflowCmd::Run {
                    workflow_id: fixture.workflow.workflow_id,
                    run_id: Some(format!("cli-{}", fixture.fixture_id)),
                    inputs: "{}".to_string(),
                    secrets: "{}".to_string(),
                    trigger: "{\"kind\":\"manual\"}".to_string(),
                },
            )
            .unwrap();
            assert_eq!(
                value.get("status"),
                Some(&serde_json::to_value(WorkflowRunStatus::Succeeded).unwrap())
            );
        }
        assert_eq!(service.histories().unwrap().len(), 5);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn at_file_arguments_are_bounded_and_replay_reuses_source_snapshots() {
        let root = std::env::temp_dir().join(format!("lm-workflow-replay-cli-{}", Uuid::new_v4()));
        let service = service(&root);
        let fixture = workflow_core_fixtures()
            .into_iter()
            .find(|fixture| fixture.fixture_id == "parallel-transform")
            .unwrap();
        service.create(fixture.workflow.clone()).unwrap();
        execute(
            &service,
            &WorkflowCmd::Run {
                workflow_id: fixture.workflow.workflow_id.clone(),
                run_id: Some("source-run".to_string()),
                inputs: "{}".to_string(),
                secrets: "{}".to_string(),
                trigger: "{\"kind\":\"manual\"}".to_string(),
            },
        )
        .unwrap();
        let replay = execute(
            &service,
            &WorkflowCmd::Replay {
                workflow_id: fixture.workflow.workflow_id,
                source_run_id: "source-run".to_string(),
                boundary_node_id: "left".to_string(),
                run_id: Some("replay-run".to_string()),
                approval: false,
                inputs: None,
                secrets: None,
            },
        )
        .unwrap();
        assert_eq!(
            replay.pointer("/history/run_id"),
            Some(&json!("replay-run"))
        );
        let _ = std::fs::remove_dir_all(root);
    }
}
