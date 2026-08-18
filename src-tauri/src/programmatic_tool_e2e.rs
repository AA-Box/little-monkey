//! Real native-path coverage for programmatic tool execution.
//!
//! The TypeScript tests cover QuickJS binding and dispatcher behavior. These
//! tests intentionally cross the native boundary with Little Monkey's real
//! mock app, permission broker, workspace resolver, checkpoint store, run
//! ledger, and Component Model extension host.

use std::path::PathBuf;
use std::time::Duration;

use crate::checkpoints;
use crate::executable_extensions::{CapabilityKind, ExtensionManager};
use crate::run_ledger::{ChainVerification, RunLedger};
use crate::run_protocol::{
    CapabilityAssessment, CapabilityState, ClientIdentity, ClientKind, ModelCapabilitiesSnapshot,
    ModelTargetSnapshot, PermissionDecision, PermissionMode, PermissionPolicySnapshot,
    RedactedPayload, RedactionState, RootAccess, RootGrant, RunBudgets, RunEvent, RunEventEnvelope,
    RunKind, RunSpec, ToolOutcome, ToolPolicyDecision, UsageSnapshot, WorkspaceContext,
    RUN_PROTOCOL_SCHEMA_VERSION,
};
use crate::{permissions, tools, AppState};
use tauri::{Manager, WebviewWindowBuilder};

struct Harness {
    _app: tauri::App<tauri::test::MockRuntime>,
    handle: tauri::AppHandle<tauri::test::MockRuntime>,
    window: tauri::Window<tauri::test::MockRuntime>,
    root: PathBuf,
    workspace: PathBuf,
}

impl Harness {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "little-monkey-programmatic-tool-e2e-{}",
            uuid::Uuid::new_v4().simple()
        ));
        let workspace = root.join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        let workspace = workspace.canonicalize().unwrap();

        let state = AppState::default();
        state
            .workspace_roots
            .lock()
            .unwrap()
            .push(crate::workspace::WorkspaceRoot {
                id: workspace.to_string_lossy().to_string(),
                label: "test".to_string(),
                path: workspace.clone(),
            });

        let app = crate::test_support::build(tauri::test::mock_builder().manage(state));
        let handle = app.handle().clone();
        let webview = WebviewWindowBuilder::new(&app, "main", Default::default())
            .build()
            .unwrap();
        let window = webview.as_ref().window();
        *handle.state::<AppState>().run_ledger.lock().unwrap() =
            Some(RunLedger::open_in_memory().unwrap());

        Self {
            _app: app,
            handle,
            window,
            root,
            workspace,
        }
    }

    fn state(&self) -> tauri::State<'_, AppState> {
        self.handle.state::<AppState>()
    }

    fn set_mode(&self, mode: &str) {
        *self.state().permissions.mode.lock().unwrap() = mode.to_string();
    }

    fn append(&self, envelope: RunEventEnvelope) {
        self.state()
            .run_ledger
            .lock()
            .unwrap()
            .as_mut()
            .unwrap()
            .append_event(&envelope)
            .unwrap();
    }

    fn append_next(&self, run_id: &str, event_id: &str, event: RunEvent) {
        let sequence = self.with_ledger(|ledger| {
            ledger
                .load_events(run_id, 0, 1_000)
                .unwrap()
                .last()
                .map_or(1, |event| event.sequence + 1)
        });
        self.append(envelope(run_id, sequence, event_id, event));
    }

    fn with_ledger<T>(&self, operation: impl FnOnce(&RunLedger) -> T) -> T {
        let state = self.state();
        let ledger = state.run_ledger.lock().unwrap();
        operation(ledger.as_ref().unwrap())
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn client() -> ClientIdentity {
    ClientIdentity {
        client_id: "programmatic-tool-e2e".to_string(),
        instance_id: "native-test".to_string(),
        kind: ClientKind::Test,
        version: "1.0.0-test".to_string(),
    }
}

fn capability() -> CapabilityAssessment {
    CapabilityAssessment {
        state: CapabilityState::Supported,
        evidence: "real native test fixture".to_string(),
    }
}

fn capabilities() -> ModelCapabilitiesSnapshot {
    ModelCapabilitiesSnapshot {
        tool_calling: capability(),
        vision: capability(),
        embeddings: capability(),
        structured_output: capability(),
        image_generation: capability(),
        audio: capability(),
        runtime_lifecycle: capability(),
        fim: capability(),
        code_completion: capability(),
        inline_edit: capability(),
        fim_metadata: None,
    }
}

fn run_spec(harness: &Harness, run_id: &str) -> RunSpec {
    let root = harness.workspace.to_string_lossy().to_string();
    RunSpec {
        schema_version: RUN_PROTOCOL_SCHEMA_VERSION,
        run_id: run_id.to_string(),
        idempotency_key: format!("programmatic/{run_id}"),
        created_at_ms: 1_000,
        kind: RunKind::Background,
        submitted_by: client(),
        task: "exercise native programmatic tool execution".to_string(),
        instructions: None,
        input_artifact_ids: Vec::new(),
        target: ModelTargetSnapshot::Ollama {
            target_id: "ollama-test".to_string(),
            label: "Ollama test".to_string(),
            base_url: "http://127.0.0.1:11434".to_string(),
            model: "qwen-test".to_string(),
            is_cloud: false,
            capabilities: capabilities(),
            estimated_memory_bytes: Some(1),
        },
        workspace: Some(WorkspaceContext {
            workspace_id: "workspace-test".to_string(),
            primary_root_id: "root-test".to_string(),
            roots: vec![RootGrant {
                root_id: "root-test".to_string(),
                canonical_path: root,
                access: RootAccess::ReadWrite,
                allow_symlinks_within_root: false,
            }],
            repository_policy: None,
        }),
        permission_policy: PermissionPolicySnapshot {
            mode: PermissionMode::Manual,
            unattended: false,
            approval_timeout_ms: 60_000,
            default_tool_decision: ToolPolicyDecision::Prompt,
            tool_rules: Vec::new(),
            allow_network: false,
            allow_external_mutations: false,
            egress_allowlist: None,
            channel_send: None,
        },
        budgets: RunBudgets {
            wall_time_ms: 60_000,
            max_iterations: 10,
            max_model_calls: 10,
            max_tool_calls: 10,
            max_input_tokens: 10_000,
            max_output_tokens: 10_000,
            max_cost_micros: None,
            max_artifact_bytes: 1_000_000,
            max_event_count: 1_000,
        },
    }
}

fn envelope(run_id: &str, sequence: u64, event_id: &str, event: RunEvent) -> RunEventEnvelope {
    RunEventEnvelope {
        schema_version: RUN_PROTOCOL_SCHEMA_VERSION,
        event_id: event_id.to_string(),
        run_id: run_id.to_string(),
        sequence,
        occurred_at_ms: 2_000 + sequence,
        actor_id: None,
        emitter: client(),
        event,
    }
}

fn proposed(
    run_id: &str,
    sequence: u64,
    tool_call_id: &str,
    tool_name: &str,
    mutation: bool,
) -> RunEventEnvelope {
    envelope(
        run_id,
        sequence,
        &format!("event-{sequence}"),
        RunEvent::ToolProposed {
            tool_call_id: tool_call_id.to_string(),
            tool_name: tool_name.to_string(),
            arguments: RedactedPayload {
                value: serde_json::json!({"path": "nested.txt"}),
                redaction: RedactionState::NotNeeded,
            },
            arguments_sha256: "a".repeat(64),
            mutation,
        },
    )
}

fn started(run_id: &str, sequence: u64, tool_call_id: &str) -> RunEventEnvelope {
    envelope(
        run_id,
        sequence,
        &format!("event-{sequence}"),
        RunEvent::ToolStarted {
            tool_call_id: tool_call_id.to_string(),
        },
    )
}

fn finished(
    run_id: &str,
    sequence: u64,
    tool_call_id: &str,
    outcome: ToolOutcome,
) -> RunEventEnvelope {
    envelope(
        run_id,
        sequence,
        &format!("event-{sequence}"),
        RunEvent::ToolFinished {
            tool_call_id: tool_call_id.to_string(),
            outcome,
            output_excerpt: None,
            output_sha256: None,
            duration_ms: 1,
        },
    )
}

async fn write_with_decision(
    harness: &Harness,
    path: &str,
    content: &str,
    checkpoint_id: Option<&str>,
    run_id: Option<&str>,
    tool_call_id: Option<&str>,
    allow: bool,
) -> Result<String, String> {
    let handle = harness.handle.clone();
    let task_path = path.to_string();
    let task_content = content.to_string();
    let task_checkpoint_id = checkpoint_id.map(str::to_string);
    let task_run_id = run_id.map(str::to_string);
    let task_tool_call_id = tool_call_id.map(str::to_string);
    let task = tokio::spawn(async move {
        let state = handle.state::<AppState>();
        tools::tool_write_file(
            handle.clone(),
            state,
            task_path,
            task_content,
            task_checkpoint_id,
            task_run_id,
            task_tool_call_id,
            None,
            None,
            Some("programmatic-e2e".to_string()),
            None,
        )
        .await
    });

    let request_id = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if let Some(id) = harness
                .state()
                .permissions
                .pending
                .lock()
                .unwrap()
                .keys()
                .next()
                .cloned()
            {
                break id;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .expect("real permission request should become pending");

    if let Some(run_id) = run_id {
        let operation_sha256 = harness.with_ledger(|ledger| {
            ledger
                .load_approval(run_id, &request_id)
                .unwrap()
                .unwrap()
                .operation_sha256
        });
        crate::run_commands::run_decide_permission(
            harness.handle.clone(),
            harness.window.clone(),
            harness.state(),
            run_id.to_string(),
            request_id,
            operation_sha256,
            if allow {
                PermissionDecision::AllowOnce
            } else {
                PermissionDecision::Deny
            },
        )?;
    } else {
        assert!(permissions::respond_if_pending(
            harness.state().inner(),
            &request_id,
            allow,
            false,
        )?);
    }

    task.await.map_err(|error| error.to_string())?
}

#[tokio::test]
async fn ordinary_tool_regression_uses_the_real_workspace_resolver() {
    let harness = Harness::new();
    std::fs::write(harness.workspace.join("ordinary.txt"), "ordinary tool").unwrap();

    let result = tools::tool_read_file(harness.state(), "ordinary.txt".to_string(), None)
        .await
        .unwrap();

    assert_eq!(result, "ordinary tool");
}

#[tokio::test]
async fn workspace_escape_is_rejected_before_permission_or_mutation() {
    let harness = Harness::new();
    harness.set_mode("acceptEdits");
    let outside = harness.root.join("outside.txt");

    let error = tools::tool_write_file(
        harness.handle.clone(),
        harness.state(),
        "../outside.txt".to_string(),
        "must not escape".to_string(),
        None,
        None,
        Some("escape-call".to_string()),
        None,
        None,
        None,
        None,
    )
    .await
    .unwrap_err();

    assert!(error.to_ascii_lowercase().contains("workspace"), "{error}");
    assert!(!outside.exists());
    assert!(harness
        .state()
        .permissions
        .pending
        .lock()
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn permission_broker_allows_and_denies_real_write_calls() {
    let harness = Harness::new();

    let allowed = write_with_decision(
        &harness,
        "allowed.txt",
        "approved",
        None,
        None,
        Some("allowed-call"),
        true,
    )
    .await
    .unwrap();
    assert!(allowed.contains("Wrote"));
    assert_eq!(
        std::fs::read_to_string(harness.workspace.join("allowed.txt")).unwrap(),
        "approved"
    );

    let denied = write_with_decision(
        &harness,
        "denied.txt",
        "rejected",
        None,
        None,
        Some("denied-call"),
        false,
    )
    .await
    .unwrap_err();
    assert!(denied.contains("Permission denied"));
    assert!(!harness.workspace.join("denied.txt").exists());
}

#[tokio::test]
async fn checkpoint_records_real_mutation_and_persists_summary() {
    let harness = Harness::new();
    harness.set_mode("acceptEdits");
    std::fs::write(harness.workspace.join("checkpoint.txt"), "before").unwrap();
    let checkpoint_dir = harness.root.join("checkpoints");
    let checkpoint_id = checkpoints::begin_impl(
        harness.state().inner(),
        &checkpoint_dir,
        "session-e2e".to_string(),
        4,
        "programmatic tool".to_string(),
        Some(10),
    )
    .unwrap();

    tools::tool_write_file(
        harness.handle.clone(),
        harness.state(),
        "checkpoint.txt".to_string(),
        "after".to_string(),
        Some(checkpoint_id.clone()),
        None,
        Some("checkpoint-call".to_string()),
        None,
        None,
        None,
        None,
    )
    .await
    .unwrap();

    assert_eq!(
        harness
            .state()
            .checkpoints
            .lock()
            .unwrap()
            .get(&checkpoint_id)
            .unwrap()
            .entries
            .len(),
        1
    );
    let summary = checkpoints::end_impl(harness.state().inner(), &checkpoint_id).unwrap();
    assert_eq!(
        summary.files,
        vec![harness.workspace.join("checkpoint.txt").to_string_lossy()]
    );
    assert_eq!(
        std::fs::read_to_string(harness.workspace.join("checkpoint.txt")).unwrap(),
        "after"
    );
}

#[tokio::test]
async fn nested_calls_have_real_run_evidence_and_outer_linkage() {
    let harness = Harness::new();
    let run_id = "programmatic-run";
    let spec = run_spec(&harness, run_id);
    {
        let state = harness.state();
        let mut ledger = state.run_ledger.lock().unwrap();
        let ledger = ledger.as_mut().unwrap();
        ledger.submit_run(&spec).unwrap();
        ledger
            .append_event(&envelope(
                run_id,
                1,
                "queued",
                RunEvent::Queued { queue: None },
            ))
            .unwrap();
        ledger
            .append_event(&envelope(
                run_id,
                2,
                "started",
                RunEvent::Started {
                    engine_id: "programmatic-e2e".to_string(),
                },
            ))
            .unwrap();
    }

    let outer_id = "outer-call";
    let nested_id = "outer-call:nested:1";
    harness.append(proposed(run_id, 3, outer_id, "read_file", false));
    harness.append(started(run_id, 4, outer_id));
    let read = tools::tool_read_file(harness.state(), "ordinary.txt".to_string(), None).await;
    assert!(read.is_err());
    harness.append(finished(run_id, 5, outer_id, ToolOutcome::Failed));

    harness.append(proposed(run_id, 6, nested_id, "write_file", true));
    harness.append(started(run_id, 7, nested_id));
    let nested = write_with_decision(
        &harness,
        "nested.txt",
        "nested result",
        None,
        Some(run_id),
        Some(nested_id),
        true,
    )
    .await
    .unwrap();
    assert!(nested.contains("Wrote"));
    harness.append_next(
        run_id,
        "nested-finished",
        RunEvent::ToolFinished {
            tool_call_id: nested_id.to_string(),
            outcome: ToolOutcome::Succeeded,
            output_excerpt: None,
            output_sha256: None,
            duration_ms: 1,
        },
    );
    harness.append_next(
        run_id,
        "outer-finished",
        RunEvent::ToolFinished {
            tool_call_id: outer_id.to_string(),
            outcome: ToolOutcome::Succeeded,
            output_excerpt: None,
            output_sha256: None,
            duration_ms: 1,
        },
    );
    harness.append_next(
        run_id,
        "completed",
        RunEvent::Completed {
            summary: Some("nested call complete".to_string()),
            result_artifact_ids: Vec::new(),
            usage: UsageSnapshot {
                input_tokens: 1,
                output_tokens: 1,
                cached_input_tokens: 0,
                model_calls: 1,
                tool_calls: 2,
                cost_micros: None,
            },
        },
    );

    harness.with_ledger(|ledger| {
        let events = ledger.load_events(run_id, 0, 100).unwrap();
        assert!(events.iter().any(|event| {
            matches!(&event.event, RunEvent::ToolProposed { tool_call_id, .. } if tool_call_id == outer_id)
        }));
        assert!(events.iter().any(|event| {
            matches!(&event.event, RunEvent::ToolFinished { tool_call_id, .. } if tool_call_id == nested_id)
        }));
        let decisions = ledger
            .permission_decisions_for_tool_call(nested_id)
            .unwrap();
        assert_eq!(decisions.len(), 1);
        assert_eq!(decisions[0].decision, Some(PermissionDecision::AllowOnce));
        assert!(ledger
            .permission_gaps(run_id)
            .unwrap()
            .iter()
            .all(|gap| !gap.is_unauthorized_mutation()));
        assert!(matches!(
            ledger.verify_run_chain(run_id).unwrap(),
            ChainVerification::Intact { .. }
        ));
    });
}

#[tokio::test]
async fn extension_provided_tool_uses_the_real_component_host() {
    let _runtime = crate::executable_extensions::test_fixtures::runtime_guard();
    let root = crate::executable_extensions::test_fixtures::TestRoot::new();
    let source = crate::executable_extensions::test_fixtures::write_bundle(
        &root.0,
        "source",
        &crate::executable_extensions::test_fixtures::component_wat(r#"{"ok":true}"#, ""),
        crate::package_ecosystem::SemanticVersion::new(1, 0, 0),
    );
    let manager = ExtensionManager::new(&root.0.join("app-data")).unwrap();
    crate::executable_extensions::test_fixtures::install_running(
        &manager,
        &source,
        "dev.example.echo",
    )
    .await;

    let capabilities = manager
        .active_capabilities(Some(CapabilityKind::Tool))
        .unwrap();
    assert_eq!(capabilities.len(), 1);
    assert_eq!(capabilities[0].capability_id, "echo");
    let result = manager
        .invoke_active_capability(
            CapabilityKind::Tool,
            "echo",
            r#"{"value":"programmatic"}"#.to_string(),
            Some("programmatic-extension-call".to_string()),
            Vec::new(),
        )
        .await
        .unwrap();
    assert_eq!(result.output_json, r#"{"ok":true}"#);
}
