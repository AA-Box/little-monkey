//! Provider-independent behavioral acceptance for messaging channels.
//!
//! This is deliberately the layer between adapter unit tests and live/provider
//! installed-service acceptance. It proves that every normalized channel kind
//! crosses the same durable ingress/outbox contract, then separately proves an
//! executable-extension channel can cross the real daemon/agent/tool boundary
//! before its reply returns through the sandboxed extension adapter.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use little_monkey_lib::channels::policy::{AccessPolicy, ChannelAccessPolicy, GroupActivation};
use little_monkey_lib::channels::routing::{ChannelRoute, RouteScope, RouteTarget};
use little_monkey_lib::channels::types::{
    ChannelConversation, ChannelEnvelope, ChannelHealth, ChannelKind, ChannelSender,
    InboundTransport, OutboundMessage, ProviderCapabilities, SendOutcome,
};
use little_monkey_lib::executable_extensions::{
    Approval, CapabilityDeclaration, CapabilityKind, ComponentReference, ExtensionManager,
    ExtensionManifest, PermissionGrant, EXTENSION_HOST_API_VERSION, EXTENSION_MANIFEST_FILE,
    EXTENSION_SCHEMA_VERSION,
};
use little_monkey_lib::package_ecosystem::{
    Compatibility, InstallSource, PackageProvenance, SemanticVersion, VersionConstraint,
};

use crate::daemon::adapters::{build_adapter, validate_non_secret_config};
use crate::daemon::channel_adapter::{AdapterConfig, ChannelAdapter, InboundBatch};
use crate::daemon::channel_agent_e2e as agent_e2e;
use crate::daemon::channel_ingress::OutboxPayload;
use crate::daemon::channel_store::{ChannelAccountRecord, EventDirection};
use crate::daemon::channel_tool::{
    plan_send, queue_send, ChannelSendRequest, SendAuthority, SendInvocation,
};
use crate::daemon::channel_worker::{drain_outbox_once, ingest_batch, poll_account_once};
use crate::daemon::store::{DaemonConfig, DaemonPaths, DaemonStore, JobState};
use crate::daemon::DaemonChannelQueue;

const NOW: i64 = 1_700_000_000_000;
const CONTRACT_REPLY: &str = "shared channel contract reply";
const EXTENSION_ID: &str = "dev.little-monkey.reference-channel";
const EXTENSION_CAPABILITY: &str = "reference";
const EXTENSION_PROVIDER_MESSAGE_ID: &str = "extension-message-9";
const EXTENSION_AGENT_REPLY: &str = "fixture reply through the extension reference transport";

/// A network-free adapter double for the provider-independent behavioral layer.
///
/// The test does not claim this is provider acceptance. Its job is the opposite:
/// hold the provider boundary fixed while every persisted `ChannelKind` is fed
/// through the exact same ingress, routing, outbox and exactly-once semantics.
struct ContractAdapter {
    kind: ChannelKind,
    sent: Arc<Mutex<Vec<OutboundMessage>>>,
}

impl ContractAdapter {
    fn new(kind: ChannelKind) -> (Self, Arc<Mutex<Vec<OutboundMessage>>>) {
        let sent = Arc::new(Mutex::new(Vec::new()));
        (
            Self {
                kind,
                sent: sent.clone(),
            },
            sent,
        )
    }
}

#[async_trait::async_trait]
impl ChannelAdapter for ContractAdapter {
    fn kind(&self) -> ChannelKind {
        self.kind
    }

    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities::minimal(self.kind, InboundTransport::LongPoll)
    }

    async fn probe(&self) -> ChannelHealth {
        ChannelHealth::connected(NOW, Some("behavioral contract".to_string()))
    }

    async fn poll(&self, _cursor: Option<&str>) -> Result<InboundBatch, String> {
        Ok(InboundBatch::default())
    }

    async fn send(&self, message: &OutboundMessage) -> SendOutcome {
        self.sent.lock().unwrap().push(message.clone());
        SendOutcome::Sent {
            provider_message_id: Some(format!("contract-{}", self.kind.as_str())),
        }
    }
}

fn contract_account(kind: ChannelKind, account_id: &str) -> ChannelAccountRecord {
    let non_secret_config = if kind == ChannelKind::Extension {
        serde_json::json!({"echo_correlation": "provider_message_id"})
    } else {
        serde_json::json!({})
    };
    ChannelAccountRecord {
        account_id: account_id.to_string(),
        kind,
        label: format!("{} behavioral contract", kind.label()),
        enabled: true,
        non_secret_config,
        credential_ref: None,
        access_policy: ChannelAccessPolicy {
            direct: AccessPolicy::Open,
            group: AccessPolicy::Open,
            group_activation: GroupActivation::Always,
        },
        health: ChannelHealth::connected(NOW, None),
        created_at_ms: NOW,
        updated_at_ms: NOW,
    }
}

fn seed_contract_account(store: &mut DaemonStore, kind: ChannelKind, account_id: &str) {
    store
        .upsert_channel_account(&contract_account(kind, account_id))
        .expect("behavioral account");
    store
        .insert_channel_route(&ChannelRoute {
            route_id: format!("route-{account_id}"),
            scope: RouteScope::account(account_id),
            target: RouteTarget::new("chat"),
            enabled: true,
            created_at_ms: NOW,
            updated_at_ms: NOW,
        })
        .expect("behavioral route");
}

/// Hermes-class behavioral E2E: the provider is doubled, the messaging core is
/// not. Every durable channel kind must obey the same normalized contract from
/// ingress through routing/queue submission and back out through the outbox.
#[tokio::test]
async fn every_channel_kind_obeys_the_same_ingress_and_outbox_contract() {
    use crate::daemon::channel_restart_tests::FakeQueue;

    let root = std::env::temp_dir().join(format!(
        "little-monkey-channel-contract-{}-{}",
        std::process::id(),
        uuid::Uuid::new_v4()
    ));
    std::fs::create_dir_all(&root).unwrap();
    let paths = DaemonPaths::under(&root);
    paths.ensure().unwrap();
    let mut store = DaemonStore::open(&paths).unwrap();

    for kind in ChannelKind::ALL.iter().copied() {
        let account_id = format!("contract-{}", kind.as_str());
        let event_id = format!("event-{}", kind.as_str());
        seed_contract_account(&mut store, kind, &account_id);

        let queue = FakeQueue::default();
        let report = ingest_batch(
            &mut store,
            &queue,
            &[ChannelEnvelope {
                account_id: account_id.clone(),
                kind,
                provider_event_id: event_id,
                provider_message_id: None,
                conversation: ChannelConversation::direct("room-1"),
                sender: ChannelSender::new("sender-1"),
                text: "exercise the shared channel contract".to_string(),
                attachments: Vec::new(),
                reply_to_provider_id: None,
                mentions_self: true,
                received_at_ms: NOW,
                metadata: Default::default(),
            }],
            NOW,
        );
        assert_eq!(
            report.accepted,
            1,
            "{} ingress: accepted {} challenged {} ignored {} duplicates {} failed {}",
            kind.label(),
            report.accepted,
            report.challenged,
            report.ignored,
            report.duplicates,
            report.failed
        );
        assert_eq!(
            queue.submitted.lock().unwrap().len(),
            1,
            "{} did not reach the shared queue seam",
            kind.label()
        );

        let request = ChannelSendRequest {
            account_id: Some(account_id.clone()),
            conversation_id: Some("room-1".to_string()),
            text: CONTRACT_REPLY.to_string(),
            ..ChannelSendRequest::default()
        };
        let authority = SendAuthority {
            accounts: vec![account_id.clone()],
            ..SendAuthority::default()
        };
        let plan = plan_send(&request, &authority, None)
            .unwrap_or_else(|error| panic!("{} outbound plan: {error}", kind.label()));
        queue_send(
            &mut store,
            &paths,
            &request,
            &plan,
            None,
            &SendInvocation {
                job_id: Some(format!("job-{}", kind.as_str())),
                tool_call_id: Some("call-contract".to_string()),
            },
            NOW,
        )
        .unwrap_or_else(|error| panic!("{} outbound queue: {error}", kind.label()));

        let (adapter, sent) = ContractAdapter::new(kind);
        let adapters: BTreeMap<String, Arc<dyn ChannelAdapter>> = BTreeMap::from([(
            account_id.clone(),
            Arc::new(adapter) as Arc<dyn ChannelAdapter>,
        )]);
        let drained = drain_outbox_once(&mut store, &adapters, NOW + 1)
            .await
            .unwrap_or_else(|error| panic!("{} outbox drain: {error}", kind.label()));
        assert_eq!(
            drained.sent,
            1,
            "{} outbox: sent {} retrying {} failed {}",
            kind.label(),
            drained.sent,
            drained.retrying,
            drained.failed
        );

        let sent = sent.lock().unwrap();
        assert_eq!(
            sent.len(),
            1,
            "{} was delivered more than once",
            kind.label()
        );
        assert_eq!(sent[0].kind, kind);
        assert_eq!(sent[0].account_id, account_id);
        assert_eq!(sent[0].conversation_id, "room-1");
        assert_eq!(sent[0].text, CONTRACT_REPLY);
        drop(sent);

        let repeat = drain_outbox_once(&mut store, &adapters, NOW + 2)
            .await
            .expect("second behavioral drain");
        assert_eq!(
            repeat,
            crate::daemon::channel_worker::OutboxReport::default(),
            "{} delivered a completed outbox row twice",
            kind.label()
        );
    }

    let _ = std::fs::remove_dir_all(root);
}

fn component_wat(output: &str) -> Vec<u8> {
    let escaped = output
        .bytes()
        .flat_map(|byte| format!("\\{byte:02x}").into_bytes())
        .map(char::from)
        .collect::<String>();
    wat::parse_str(format!(
        r#"(component
          (core module $m
            (memory (export "memory") 2)
            (global $heap (mut i32) (i32.const 4096))
            (data (i32.const 1024) "{escaped}")
            (func $realloc (export "realloc")
              (param i32 i32 i32 i32) (result i32)
              (local $ret i32)
              global.get $heap
              local.set $ret
              global.get $heap
              local.get 3
              i32.add
              global.set $heap
              local.get $ret)
            (func (export "run")
              (param i32 i32 i32 i32) (result i32)
              i32.const 64
              i32.const 0
              i32.store8
              i32.const 68
              i32.const 1024
              i32.store
              i32.const 72
              i32.const {length}
              i32.store
              i32.const 64))
          (core instance $i (instantiate $m))
          (func $run
            (param "capability-id" string)
            (param "input-json" string)
            (result (result string (error string)))
            (canon lift (core func $i "run")
              (memory $i "memory")
              (realloc (func $i "realloc"))))
          (instance (export (interface "little-monkey:extension/guest@1.0.0"))
            (export "run" (func $run))))"#,
        length = output.len(),
    ))
    .expect("extension fixture component")
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    format!("{:x}", Sha256::digest(bytes))
}

async fn install_reference_extension(app_data: &Path, source_root: &Path) -> ExtensionManager {
    let output = format!(
        r#"{{"messages":[{{"provider_event_id":"extension-event-1","conversation_id":"room-1","conversation_kind":"direct","sender_id":"user-1","text":"is the build green","mentions_self":true}}],"cursor":"cursor-1","status":"sent","provider_message_id":"{EXTENSION_PROVIDER_MESSAGE_ID}"}}"#
    );
    let component = component_wat(&output);
    let digest = sha256_hex(&component);
    let source = source_root.join(EXTENSION_ID);
    std::fs::create_dir_all(&source).unwrap();
    std::fs::write(source.join("component.wasm"), &component).unwrap();

    let manifest = ExtensionManifest {
        schema_version: EXTENSION_SCHEMA_VERSION,
        extension_id: EXTENSION_ID.to_string(),
        version: SemanticVersion::new(1, 0, 0),
        display_name: "Reference channel fixture".to_string(),
        description: "Sandboxed reference transport fixture".to_string(),
        host_api: VersionConstraint::at_least(EXTENSION_HOST_API_VERSION),
        component: ComponentReference {
            path: "component.wasm".to_string(),
            sha256: digest.clone(),
        },
        capabilities: vec![CapabilityDeclaration {
            capability_id: EXTENSION_CAPABILITY.to_string(),
            kind: CapabilityKind::Channel,
            display_name: "Reference channel".to_string(),
            description: "Normalizes one fixture provider".to_string(),
            input_schema: serde_json::json!({"type":"object"}),
        }],
        permissions: Vec::new(),
        config_schema: Vec::new(),
        secret_slots: Vec::new(),
        dependencies: Vec::new(),
        compatibility: Compatibility {
            minimum_app_version: SemanticVersion::new(0, 1, 0),
            maximum_app_version_exclusive: None,
            platforms: [std::env::consts::OS.to_string()].into_iter().collect(),
            architectures: [std::env::consts::ARCH.to_string()].into_iter().collect(),
            contract: None,
        },
        publisher: "Independent Fixture".to_string(),
        provenance: PackageProvenance {
            publisher: "Independent Fixture".to_string(),
            source: InstallSource::LocalFolder {
                canonical_path: source.to_string_lossy().to_string(),
            },
            source_revision: "1.0.0".to_string(),
            build_reproducible: true,
        },
        signature: None,
        checksums: BTreeMap::from([("component.wasm".to_string(), digest)]),
    };
    std::fs::write(
        source.join(EXTENSION_MANIFEST_FILE),
        serde_json::to_vec_pretty(&manifest).unwrap(),
    )
    .unwrap();

    let manager = ExtensionManager::new(app_data).unwrap();
    let preview = manager.discover(&source).unwrap();
    let grants: Vec<PermissionGrant> = preview
        .permissions
        .iter()
        .map(|permission| PermissionGrant {
            permission_id: permission.permission_id.clone(),
            binding: None,
        })
        .collect();
    manager
        .install(
            &source,
            Approval {
                approval_digest: preview.approval_digest,
                grants,
                allow_unsigned: true,
                allow_untrusted: false,
                allow_high_risk: true,
            },
        )
        .await
        .unwrap();
    manager.set_enabled(EXTENSION_ID, true).await.unwrap();
    manager.set_running(EXTENSION_ID, true).await.unwrap();
    manager
}

fn extension_model_fixture() -> agent_e2e::HttpFixture {
    agent_e2e::HttpFixture::spawn(move |head, body, _index| {
        if !head.contains("/chat/completions") {
            return agent_e2e::json_response(r#"{"error":"unexpected model route"}"#);
        }
        // The tool result comes back as ordinary JSON in the request body, not
        // escaped inside a string. Looking for the escaped form meant this
        // never matched, so the fixture answered every turn with another
        // `send_message` and the run died on its iteration budget.
        if body.contains("\"role\":\"tool\"") {
            return agent_e2e::sse_response(&[
                serde_json::json!({
                    "choices": [{"index": 0, "delta": {"content": "sent."}}]
                }),
                serde_json::json!({
                    "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]
                }),
            ]);
        }
        let arguments = serde_json::json!({"text": EXTENSION_AGENT_REPLY}).to_string();
        agent_e2e::sse_response(&[
            serde_json::json!({
                "choices": [{
                    "index": 0,
                    "delta": {"tool_calls": [{
                        "index": 0,
                        "id": "call_extension_reference_1",
                        "type": "function",
                        "function": {"name": "send_message", "arguments": arguments}
                    }]}
                }]
            }),
            serde_json::json!({
                "choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls"}]
            }),
        ])
    })
    .expect("bind extension model fixture")
}

fn seed_agent_extension_channel(store: &mut DaemonStore, now: i64) {
    let account = ChannelAccountRecord {
        account_id: agent_e2e::ACCOUNT_ID.to_string(),
        kind: ChannelKind::Extension,
        label: "Reference extension channel".to_string(),
        enabled: true,
        non_secret_config: serde_json::json!({
            "extension_id": EXTENSION_ID,
            "capability_id": EXTENSION_CAPABILITY,
            "echo_correlation": "provider_message_id"
        }),
        credential_ref: None,
        access_policy: ChannelAccessPolicy {
            direct: AccessPolicy::Open,
            group: AccessPolicy::Open,
            group_activation: GroupActivation::Always,
        },
        health: ChannelHealth::connected(now, None),
        created_at_ms: now,
        updated_at_ms: now,
    };
    validate_non_secret_config(account.kind, &account.non_secret_config)
        .expect("reference extension account config");
    store
        .upsert_channel_account(&account)
        .expect("extension account");
    store
        .insert_channel_route(&ChannelRoute {
            route_id: format!("route-{}", agent_e2e::ACCOUNT_ID),
            scope: RouteScope::account(agent_e2e::ACCOUNT_ID),
            target: RouteTarget::new(agent_e2e::RECIPE),
            enabled: true,
            created_at_ms: now,
            updated_at_ms: now,
        })
        .expect("extension route");
}

async fn run_extension_agent_end_to_end(root: &Path) {
    if !agent_e2e::isolation_is_real(root) {
        println!(
            "{} on this platform: the extension agent contract could not isolate app data",
            agent_e2e::SKIPPED
        );
        return;
    }

    let model = extension_model_fixture();
    let workspace = root.join("extension-agent-workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let roots = little_monkey_lib::app_paths::ensure_agent_config_roots().expect("config roots");
    agent_e2e::write_recipe(&roots.authored, &workspace, &model.base);

    let paths = DaemonPaths::under(&roots.legacy);
    paths.ensure().unwrap();
    let config = DaemonConfig::default();
    config.save(&paths).unwrap();
    let _manager = install_reference_extension(&roots.legacy, &root.join("extension-source")).await;

    let now = agent_e2e::now_ms();
    let mut store = DaemonStore::open(&paths).unwrap();
    seed_agent_extension_channel(&mut store, now);
    let account = store
        .channel_account(agent_e2e::ACCOUNT_ID)
        .unwrap()
        .expect("extension account readback");
    let adapter: Arc<dyn ChannelAdapter> = build_adapter(
        &AdapterConfig {
            account: &account,
            secret: String::new(),
        },
        Some(&paths),
    )
    .expect("production registry builds extension adapter")
    .into();

    let queue = DaemonChannelQueue::new(paths.clone());
    let report = poll_account_once(
        &mut store,
        &queue,
        agent_e2e::ACCOUNT_ID,
        adapter.as_ref(),
        now,
    )
    .await
    .expect("extension poll enters production ingress");
    assert_eq!(
        report.accepted, 1,
        "extension inbound was not accepted: accepted {} challenged {} ignored {} duplicates {} failed {}",
        report.accepted, report.challenged, report.ignored, report.duplicates, report.failed
    );

    let inbound = store
        .recent_channel_events(agent_e2e::ACCOUNT_ID, 10)
        .unwrap()
        .into_iter()
        .find(|event| event.direction == EventDirection::Inbound)
        .expect("durable extension inbound event");
    let job_id = inbound.job_id.expect("extension inbound owns a daemon job");
    let job = store
        .get_job(&job_id)
        .unwrap()
        .expect("extension daemon job");
    assert_eq!(job.state, JobState::Queued);
    let run_id = job.run_id.expect("extension daemon job owns a run");
    assert_eq!(
        store.ingress_reply_grant_for_job(&job_id).unwrap(),
        Some(true),
        "the extension turn did not freeze the same reply grant as built-in channels"
    );
    assert!(
        store.channel_origin_for_job(&job_id).unwrap().is_some(),
        "send_message cannot resolve the extension turn's durable origin"
    );

    let proof = agent_e2e::execute_turn_through_the_daemon(
        &paths,
        &config,
        agent_e2e::ACCOUNT_ID,
        &job_id,
        &run_id,
        &adapter,
        &model,
    )
    .await;
    assert_eq!(
        proof.provider_message_id, EXTENSION_PROVIDER_MESSAGE_ID,
        "the sandboxed provider's message id did not survive the shared outbox"
    );

    let store = DaemonStore::open(&paths).unwrap();
    let events = store
        .recent_channel_events(agent_e2e::ACCOUNT_ID, 20)
        .unwrap();
    let inbound_count = events
        .iter()
        .filter(|event| event.direction == EventDirection::Inbound)
        .count();
    let outbound: Vec<_> = events
        .iter()
        .filter(|event| event.direction == EventDirection::Outbound)
        .collect();
    assert_eq!(
        inbound_count, 1,
        "extension event became multiple inbound turns"
    );
    assert_eq!(
        outbound.len(),
        1,
        "agent reply became multiple outbound events"
    );
    let payload: OutboxPayload = serde_json::from_str(&outbound[0].envelope_json)
        .expect("outbound extension event retains the production outbox payload");
    assert_eq!(payload.message.account_id, agent_e2e::ACCOUNT_ID);
    assert_eq!(payload.message.kind, ChannelKind::Extension);
    assert_eq!(payload.message.conversation_id, "room-1");
    assert_eq!(
        payload.message.text, EXTENSION_AGENT_REPLY,
        "a guest-side fixed success response cannot satisfy this assertion; the exact model-requested text must have crossed send_message and the durable outbox"
    );
}

/// Strong reference-transport proof: the sandboxed extension only normalizes
/// provider I/O. The resident daemon still owns ingress, the real agent child,
/// the production `send_message` tool, durability and outbox delivery.
#[test]
fn an_extension_channel_becomes_an_agent_reply_end_to_end() {
    agent_e2e::in_isolated_process(
        "fail_points::channel_contract_e2e",
        "an_extension_channel_becomes_an_agent_reply_end_to_end",
        |root: PathBuf| Box::pin(async move { run_extension_agent_end_to_end(&root).await }),
    );
}
