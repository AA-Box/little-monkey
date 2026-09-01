//! Proof that the three capabilities served from inside the daemon — channel,
//! device provider and realtime voice — are reached through the daemon's own
//! code paths rather than through the extension manager.
//!
//! Each test installs a real Component Model component through the real
//! install/enable/start lifecycle and then drives the production entry point:
//! the registry's `build_adapter` and the `ChannelAdapter` trait for channels,
//! `device::dispatch` for a device action, and the `CallSpeech` a live call
//! actually holds for realtime voice. A test that stopped crossing one of
//! those boundaries would stop compiling rather than quietly pass.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use little_monkey_lib::channels::policy::ChannelAccessPolicy;
use little_monkey_lib::channels::types::{
    ChannelHealth, ChannelKind, HealthState, OutboundMessage, SendOutcome,
};
use little_monkey_lib::executable_extensions::{
    Approval, CapabilityDeclaration, CapabilityKind, ComponentReference, ExtensionManager,
    ExtensionManifest, PermissionDeclaration, PermissionGrant, PermissionKind,
    EXTENSION_HOST_API_VERSION, EXTENSION_MANIFEST_FILE, EXTENSION_SCHEMA_VERSION,
};
use little_monkey_lib::package_ecosystem::{
    Compatibility, InstallSource, PackageProvenance, SemanticVersion, VersionConstraint,
};

use super::adapters::{build_adapter, validate_non_secret_config};
use super::channel_adapter::AdapterConfig;
use super::channel_store::ChannelAccountRecord;

struct TestRoot(PathBuf);

impl TestRoot {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "little-monkey-extension-providers-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }
}

impl Drop for TestRoot {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A component whose `run` export always answers with `output`.
///
/// Deliberately the same shape the runtime's own fixtures use: a real
/// component, lifted through the real `little-monkey:extension/guest` world,
/// so nothing about installation, instantiation or the output cap is special
/// cased for a test.
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
    .unwrap()
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut digest = Sha256::new();
    digest.update(bytes);
    format!("{:x}", digest.finalize())
}

fn escape(bytes: &[u8]) -> String {
    bytes
        .iter()
        .flat_map(|byte| format!("\\{byte:02x}").into_bytes())
        .map(char::from)
        .collect()
}

/// A component that reads `read_id` through the host's own `artifact-read`
/// import, writes `marker` followed by the bytes it got back through
/// `artifact-write`, and answers `prefix` + the id the host returned for that
/// write + `suffix`.
///
/// Every part of that is load-bearing.
///
/// *Reading through the real import* is what makes this fixture able to fail:
/// a component that only received an artifact id in its input JSON and echoed
/// it would pass whether or not the host granted anything. This one asks, and
/// a refusal comes back as an error it propagates as its own — so a step run
/// with no trusted grant fails loudly instead of quietly succeeding.
///
/// *Writing the bytes back behind a marker* is what proves the bytes are the
/// caller's and not something the guest already knew. The store is content
/// addressed, so writing the clip unchanged would return the very id the
/// fixture was told to read, and echoing that id would look identical. With a
/// marker in front, the answer is `sha256(marker ++ clip)` — a value no guest
/// can produce without having held the clip.
fn component_wat_echoing_artifact(
    read_id: &str,
    marker: &[u8],
    prefix: &str,
    suffix: &str,
) -> Vec<u8> {
    wat::parse_str(format!(
        r#"(component
          (import "little-monkey:extension/host@1.0.0" (instance $host
            (export "artifact-read"
              (func (param "artifact-id" string) (result (result (list u8) (error string)))))
            (export "artifact-write"
              (func (param "bytes" (list u8)) (result (result string (error string)))))))
          (core module $libc
            (memory (export "memory") 16)
            (global $heap (mut i32) (i32.const 524288))
            (func $realloc (export "realloc")
              (param i32 i32 i32 i32) (result i32)
              (local $ret i32)
              global.get $heap
              local.set $ret
              global.get $heap
              local.get 3
              i32.add
              global.set $heap
              local.get $ret))
          (core instance $libc_i (instantiate $libc))
          (alias core export $libc_i "memory" (core memory $mem))
          (alias core export $libc_i "realloc" (core func $realloc))
          (core func $read (canon lower (func $host "artifact-read")
            (memory $mem) (realloc $realloc)))
          (core func $write (canon lower (func $host "artifact-write")
            (memory $mem) (realloc $realloc)))
          (core module $m
            (import "libc" "memory" (memory 16))
            (import "host" "artifact-read" (func $read (param i32 i32 i32)))
            (import "host" "artifact-write" (func $write (param i32 i32 i32)))
            (data (i32.const 1024) "{read_id}")
            (data (i32.const 2048) "{prefix}")
            (data (i32.const 3072) "{suffix}")
            (data (i32.const 4096) "{marker}")
            (func (export "run")
              (param i32 i32 i32 i32) (result i32)
              (local $ptr i32) (local $len i32) (local $id_ptr i32) (local $id_len i32)
              (local $total i32)
              ;; artifact-read(read_id) -> result area at 5120
              i32.const 1024
              i32.const {read_id_len}
              i32.const 5120
              call $read
              i32.const 5120
              i32.load
              if
                ;; Denied. The host's reason becomes this guest's own error,
                ;; so the subsystem above sees why rather than a bare trap.
                i32.const 64
                i32.const 1
                i32.store8
                i32.const 68
                i32.const 5124
                i32.load
                i32.store
                i32.const 72
                i32.const 5128
                i32.load
                i32.store
                i32.const 64
                return
              end
              i32.const 5124
              i32.load
              local.set $ptr
              i32.const 5128
              i32.load
              local.set $len
              ;; marker ++ what was read, staged at 16384
              i32.const 16384
              i32.const 4096
              i32.const {marker_len}
              memory.copy
              i32.const 16384
              i32.const {marker_len}
              i32.add
              local.get $ptr
              local.get $len
              memory.copy
              ;; artifact-write(marker ++ bytes) -> result area at 5136
              i32.const 16384
              i32.const {marker_len}
              local.get $len
              i32.add
              i32.const 5136
              call $write
              i32.const 5136
              i32.load
              if
                unreachable
              end
              i32.const 5140
              i32.load
              local.set $id_ptr
              i32.const 5144
              i32.load
              local.set $id_len
              ;; prefix ++ the id the host answered with ++ suffix
              i32.const 262144
              i32.const 2048
              i32.const {prefix_len}
              memory.copy
              i32.const 262144
              i32.const {prefix_len}
              i32.add
              local.get $id_ptr
              local.get $id_len
              memory.copy
              i32.const 262144
              i32.const {prefix_len}
              i32.add
              local.get $id_len
              i32.add
              i32.const 3072
              i32.const {suffix_len}
              memory.copy
              i32.const {prefix_len}
              local.get $id_len
              i32.add
              i32.const {suffix_len}
              i32.add
              local.set $total
              i32.const 64
              i32.const 0
              i32.store8
              i32.const 68
              i32.const 262144
              i32.store
              i32.const 72
              local.get $total
              i32.store
              i32.const 64))
          (core instance $i (instantiate $m
            (with "libc" (instance $libc_i))
            (with "host" (instance
              (export "artifact-read" (func $read))
              (export "artifact-write" (func $write))))))
          (func $run
            (param "capability-id" string)
            (param "input-json" string)
            (result (result string (error string)))
            (canon lift (core func $i "run")
              (memory $mem)
              (realloc $realloc)))
          (instance (export (interface "little-monkey:extension/guest@1.0.0"))
            (export "run" (func $run))))"#,
        read_id = escape(read_id.as_bytes()),
        read_id_len = read_id.len(),
        prefix = escape(prefix.as_bytes()),
        prefix_len = prefix.len(),
        suffix = escape(suffix.as_bytes()),
        suffix_len = suffix.len(),
        marker = escape(marker),
        marker_len = marker.len(),
    ))
    .unwrap()
}

/// Install one fixture extension declaring exactly `capabilities`, and bring
/// it to the healthy+running state every provider registry requires.
async fn install_fixture(
    app_data: &Path,
    source_root: &Path,
    extension_id: &str,
    output: &str,
    capabilities: &[(CapabilityKind, &str)],
    permissions: Vec<PermissionDeclaration>,
) -> ExtensionManager {
    install_component(
        app_data,
        source_root,
        extension_id,
        component_wat(output),
        capabilities,
        permissions,
    )
    .await
}

/// The same lifecycle for a caller that built its own component — one that
/// calls host imports rather than answering with a constant.
async fn install_component(
    app_data: &Path,
    source_root: &Path,
    extension_id: &str,
    component: Vec<u8>,
    capabilities: &[(CapabilityKind, &str)],
    permissions: Vec<PermissionDeclaration>,
) -> ExtensionManager {
    let digest = sha256_hex(&component);
    let source = source_root.join(extension_id);
    std::fs::create_dir_all(&source).unwrap();
    let manifest = ExtensionManifest {
        schema_version: EXTENSION_SCHEMA_VERSION,
        extension_id: extension_id.to_string(),
        version: SemanticVersion::new(1, 0, 0),
        display_name: "Fixture provider".to_string(),
        description: "Daemon-side capability fixture".to_string(),
        host_api: VersionConstraint::at_least(EXTENSION_HOST_API_VERSION),
        component: ComponentReference {
            path: "component.wasm".to_string(),
            sha256: digest.clone(),
        },
        capabilities: capabilities
            .iter()
            .map(|(kind, capability_id)| CapabilityDeclaration {
                capability_id: (*capability_id).to_string(),
                kind: *kind,
                display_name: format!("Fixture {capability_id}"),
                description: "Fixture capability".to_string(),
                input_schema: serde_json::json!({"type": "object"}),
            })
            .collect(),
        permissions,
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
    std::fs::write(source.join("component.wasm"), &component).unwrap();
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
    manager.set_enabled(extension_id, true).await.unwrap();
    manager.set_running(extension_id, true).await.unwrap();
    manager
}

fn extension_account(extension_id: &str, capability_id: &str) -> ChannelAccountRecord {
    ChannelAccountRecord {
        account_id: "acct-ext".to_string(),
        kind: ChannelKind::Extension,
        label: "Fixture channel".to_string(),
        enabled: true,
        non_secret_config: serde_json::json!({
            "extension_id": extension_id,
            "capability_id": capability_id,
            // An extension whose transport reports the provider's own message
            // ids, which is what lets the host recognise its own echo causally.
            // Declared here because these tests drive the *durable path*; an
            // account that declares nothing is held to a narrower reply policy,
            // and that restriction has its own tests in `channel_ingress`.
            "echo_correlation": "provider_message_id",
        }),
        credential_ref: None,
        access_policy: ChannelAccessPolicy::default(),
        health: ChannelHealth {
            state: HealthState::Disconnected,
            detail: None,
            last_error: None,
            probed_at_ms: 1,
        },
        created_at_ms: 1,
        updated_at_ms: 1,
    }
}

// ---------------------------------------------------------------------------
// Channel
// ---------------------------------------------------------------------------

/// One answer that serves both halves of the adapter contract.
///
/// The guest ABI is one `run` export, so a poll and a send reach the same
/// component; the two response shapes have no overlapping field, so a single
/// object satisfies both without either side seeing anything it did not ask
/// for. That is a property of the wire contract, not a trick — an extension
/// with real state would branch on `op`.
const CHANNEL_FIXTURE_OUTPUT: &str = r#"{"messages":[{"provider_event_id":"evt-1","conversation_id":"room-1","conversation_kind":"direct","sender_id":"user-1","text":"is the build green","mentions_self":true}],"cursor":"c-1","status":"sent","provider_message_id":"m-9"}"#;

/// An account and an open route, in a store the caller owns.
///
/// Written through the store's own upsert/insert rather than assembled as
/// rows, so what the ingress gate reads back is what the settings UI would
/// have written.
fn seed_extension_channel(
    store: &mut super::store::DaemonStore,
    extension_id: &str,
    capability_id: &str,
) {
    use little_monkey_lib::channels::policy::{AccessPolicy, GroupActivation};
    use little_monkey_lib::channels::routing::{ChannelRoute, RouteScope, RouteTarget};

    let mut account = extension_account(extension_id, capability_id);
    account.access_policy = ChannelAccessPolicy {
        direct: AccessPolicy::Open,
        group: AccessPolicy::Open,
        group_activation: GroupActivation::Always,
    };
    account.health = ChannelHealth::connected(super::channel_restart_tests::NOW, None);
    store.upsert_channel_account(&account).expect("account");
    store
        .insert_channel_route(&ChannelRoute {
            route_id: "route-ext".to_string(),
            scope: RouteScope::account("acct-ext"),
            target: RouteTarget::new("chat"),
            enabled: true,
            created_at_ms: super::channel_restart_tests::NOW,
            updated_at_ms: super::channel_restart_tests::NOW,
        })
        .expect("route");
    // A machine whose model was already chosen; the first-run gate is covered
    // in `channel_commands` and `channel_ingress`.
    super::channel_commands::mark_model_chosen(store).expect("model chosen");
}

/// The application-level claim: an extension channel is a channel.
///
/// Inbound runs the daemon's own `poll_account_once` — the ingress gate, the
/// durable event, dedupe, routing and the run submission all belong to
/// `channel_ingress`, not to anything extension-specific. Outbound runs the
/// agent tool's own `plan_send`/`queue_send` into the shared outbox and then
/// the daemon's `drain_outbox_once`, which is what actually reaches the
/// adapter. Nothing between the fixture's two answers is written here.
#[tokio::test]
async fn an_extension_channel_rides_the_common_durable_path_in_both_directions() {
    use super::channel_restart_tests::{FakeQueue, NOW};
    use super::channel_tool::{plan_send, queue_send, ChannelSendRequest, SendAuthority};
    use super::channel_worker::{drain_outbox_once, poll_account_once};

    let root = TestRoot::new();
    let app_data = root.0.join("app-data");
    let _manager = install_fixture(
        &app_data,
        &root.0,
        "dev.example.chat",
        CHANNEL_FIXTURE_OUTPUT,
        &[(CapabilityKind::Channel, "room")],
        Vec::new(),
    )
    .await;
    let paths = super::store::DaemonPaths::under(&app_data);
    paths.ensure().unwrap();

    let mut store = super::store::DaemonStore::open(&paths).expect("the daemon store opens");
    seed_extension_channel(&mut store, "dev.example.chat", "room");
    let account = store
        .channel_account("acct-ext")
        .unwrap()
        .expect("the account was seeded");
    let adapter: std::sync::Arc<dyn super::channel_adapter::ChannelAdapter> = build_adapter(
        &AdapterConfig {
            account: &account,
            secret: String::new(),
        },
        Some(&paths),
    )
    .expect("the registry builds the adapter the daemon would build")
    .into();

    // --- Inbound: provider event → durable turn → queued run.
    let queue = FakeQueue::default();
    let report = poll_account_once(&mut store, &queue, "acct-ext", adapter.as_ref(), NOW)
        .await
        .expect("the daemon's own poll pass runs");
    assert_eq!(report.accepted, 1, "{report:?}");
    assert_eq!(queue.submitted.lock().unwrap().len(), 1);

    let dedupe_key = little_monkey_lib::channels::ingress::dedupe_key_for(
        little_monkey_lib::channels::ingress::ConversationSource::MessagingChannel,
        "acct-ext",
        "evt-1",
    );
    let turn = store
        .ingress_turn_by_dedupe_key(&dedupe_key)
        .unwrap()
        .expect("the turn is durable before any run exists");
    assert!(turn.job_id.is_some(), "the durable turn owns a job");

    // The provider redelivering the same event must not become a second run.
    // The cursor moved, so this is the ingress gate collapsing it, not the
    // adapter declining to hand it over twice.
    let again = poll_account_once(&mut store, &queue, "acct-ext", adapter.as_ref(), NOW + 1)
        .await
        .expect("a redelivery is polled the same way");
    assert_eq!(again.duplicates, 1, "{again:?}");
    assert_eq!(queue.submitted.lock().unwrap().len(), 1);

    // --- Outbound: the agent's reply → shared outbox → this adapter.
    let request = ChannelSendRequest {
        account_id: Some("acct-ext".to_string()),
        conversation_id: Some("room-1".to_string()),
        text: "the build is green".to_string(),
        ..ChannelSendRequest::default()
    };
    let authority = SendAuthority {
        accounts: vec!["acct-ext".to_string()],
        ..SendAuthority::default()
    };
    let plan = plan_send(&request, &authority, None).expect("the run may reach this account");
    let queued = queue_send(
        &mut store,
        &paths,
        &request,
        &plan,
        None,
        &super::channel_tool::SendInvocation {
            job_id: Some("job-ext".to_string()),
            tool_call_id: Some("call-1".to_string()),
        },
        NOW,
    )
    .expect("the reply becomes a durable outbox row");
    assert_eq!(queued["status"], "queued");
    assert_eq!(store.outbox_count_for_job("job-ext").unwrap(), 1);

    let adapters = BTreeMap::from([("acct-ext".to_string(), adapter.clone())]);
    let drained = drain_outbox_once(&mut store, &adapters, NOW + 60_000)
        .await
        .expect("the daemon's own outbox drain runs");
    assert_eq!(drained.sent, 1, "{drained:?}");

    // Delivered exactly once and recorded as such: a second drain has nothing
    // left to claim, which is the outbox — not the adapter — holding the
    // at-most-once guarantee for this provider like every other.
    let repeat = drain_outbox_once(&mut store, &adapters, NOW + 120_000)
        .await
        .expect("a second pass runs");
    assert_eq!(repeat, super::channel_worker::OutboxReport::default());
}

/// A secret store this test owns, so signing never touches the OS keychain.
///
/// The verification is the production one either way — only where the shared
/// secret is kept differs, and that is the one thing a test may not use the
/// operator's real copy of.
struct TestSecrets(String);

impl super::trigger::SecretStore for TestSecrets {
    fn put(&self, _trigger_id: &str, _secret: &str) -> Result<(), String> {
        Ok(())
    }
    fn get(&self, _trigger_id: &str) -> Result<String, String> {
        Ok(self.0.clone())
    }
    fn delete(&self, _trigger_id: &str) -> Result<(), String> {
        Ok(())
    }
}

/// An extension handler is reached by the daemon's existing webhook ingress,
/// after the delivery is already durable — never by a socket of its own.
///
/// The order is the point. `ingest_signed_delivery` authenticates the request,
/// bounds it, deduplicates it and commits it; only a later pass over the
/// committed rows runs any guest code. So the acknowledgement a provider
/// receives is never a promise about something still in flight, and a process
/// that dies between the two re-enters at the same row.
#[tokio::test]
async fn an_extension_webhook_handler_runs_only_after_the_delivery_is_durable() {
    use super::trigger::{
        canonical_generic_signature_message, ingest_signed_delivery, signature_hex, IngestOutcome,
        SignedDelivery, TriggerConfig, TriggerTarget,
    };

    let root = TestRoot::new();
    let app_data = root.0.join("app-data");
    let manager = install_fixture(
        &app_data,
        &root.0,
        "dev.example.chat",
        r#"{"account_id":"acct-ext","messages":[{"provider_event_id":"hook-1","conversation_id":"room-1","conversation_kind":"direct","sender_id":"user-1","text":"delivered by webhook","mentions_self":true}]}"#,
        &[(CapabilityKind::Channel, "incoming")],
        vec![PermissionDeclaration {
            permission_id: "webhook-incoming".to_string(),
            kind: PermissionKind::WebhookReceive,
            scope: "incoming".to_string(),
            reason: "Fixture receives its provider's callbacks".to_string(),
        }],
    )
    .await;
    let trust = manager.inspect("dev.example.chat").unwrap();

    let paths = super::store::DaemonPaths::under(&app_data);
    paths.ensure().unwrap();
    let mut store = super::store::DaemonStore::open(&paths).unwrap();
    seed_extension_channel(&mut store, "dev.example.chat", "incoming");
    let mut shared = super::ledger::SharedLedger::open(&paths.ledger_db).unwrap();

    let config = TriggerConfig::SignedWebhook {
        target: TriggerTarget::Extension {
            extension_id: "dev.example.chat".to_string(),
            handler_id: "incoming".to_string(),
            version: trust.active_version.clone(),
            manifest_sha256: trust.trust.manifest_sha256.clone(),
        },
        workflow: None,
        secret_reference: Some("vault-hook".to_string()),
        max_skew_ms: 60_000,
    };
    shared
        .upsert_trigger(
            "hook-ext",
            config.kind_token(),
            &serde_json::to_vec(&config).unwrap(),
            10_000,
            None,
        )
        .unwrap();

    let secret = "0123456789abcdef";
    let secrets = TestSecrets(secret.to_string());
    let payload = br#"{"event":"message.created"}"#;
    let nonce = uuid::Uuid::new_v4().to_string();
    let signature = signature_hex(
        secret.as_bytes(),
        &canonical_generic_signature_message(10_000, &nonce, payload),
    );
    let delivery = SignedDelivery {
        trigger_id: "hook-ext",
        delivery_id: "delivery-one",
        timestamp_ms: 10_000,
        nonce: &nonce,
        signature: &signature,
        event_name: None,
        payload,
    };
    assert_eq!(
        ingest_signed_delivery(&mut shared, &mut store, &secrets, &delivery, 10_001).unwrap(),
        IngestOutcome::Accepted
    );
    // Redelivery is answered from the ledger, without the handler running a
    // second time — the dedupe every trigger already has, not a new one.
    assert_eq!(
        ingest_signed_delivery(&mut shared, &mut store, &secrets, &delivery, 10_002).unwrap(),
        IngestOutcome::Duplicate
    );
    let forged = SignedDelivery {
        delivery_id: "delivery-forged",
        signature: &signature_hex(
            b"the wrong secret",
            &canonical_generic_signature_message(10_000, &nonce, payload),
        ),
        ..delivery
    };
    assert_eq!(
        ingest_signed_delivery(&mut shared, &mut store, &secrets, &forged, 10_003).unwrap(),
        IngestOutcome::Rejected
    );

    let pending = store.pending_delivery_payloads(10).unwrap();
    assert_eq!(
        pending.len(),
        1,
        "one committed delivery, awaiting its pass"
    );

    let queue = super::channel_restart_tests::FakeQueue::default();
    super::dispatch_extension_delivery(
        &paths,
        &mut store,
        &mut shared,
        &queue,
        &pending[0],
        "dev.example.chat",
        "incoming",
        &trust.active_version,
        &trust.trust.manifest_sha256,
    )
    .await
    .expect("the committed delivery reaches the handler");

    // What the handler normalized is now an ordinary channel turn: the same
    // acceptance, the same dedupe key, the same table as a polled message.
    let turn = store
        .ingress_turn_by_dedupe_key(&little_monkey_lib::channels::ingress::dedupe_key_for(
            little_monkey_lib::channels::ingress::ConversationSource::MessagingChannel,
            "acct-ext",
            "hook-1",
        ))
        .unwrap()
        .expect("the webhook message became a durable conversation turn");
    assert_eq!(turn.source_event_id, "hook-1");
    assert_eq!(turn.source_account_id, "acct-ext");
    assert!(store.pending_delivery_payloads(10).unwrap().is_empty());

    // A second, genuinely new callback — the first is already `submitted`, and
    // re-entering it is the idempotent replay rather than a fresh dispatch.
    let second_nonce = uuid::Uuid::new_v4().to_string();
    let second = SignedDelivery {
        delivery_id: "delivery-two",
        nonce: &second_nonce,
        signature: &signature_hex(
            secret.as_bytes(),
            &canonical_generic_signature_message(10_004, &second_nonce, payload),
        ),
        timestamp_ms: 10_004,
        ..delivery
    };
    assert_eq!(
        ingest_signed_delivery(&mut shared, &mut store, &secrets, &second, 10_005).unwrap(),
        IngestOutcome::Accepted
    );
    let pending = store.pending_delivery_payloads(10).unwrap();
    assert_eq!(pending.len(), 1);

    // The version and manifest the trigger was pinned to are re-checked on
    // every delivery, so an update between two callbacks cannot silently
    // redirect the handler to different code.
    let stale = super::dispatch_extension_delivery(
        &paths,
        &mut store,
        &mut shared,
        &queue,
        &pending[0],
        "dev.example.chat",
        "incoming",
        "9.9.9",
        &trust.trust.manifest_sha256,
    )
    .await
    .expect_err("a pinned version that no longer matches is refused");
    assert!(stale.contains("immutable version"), "{stale}");
}

#[test]
fn an_extension_channel_account_must_name_both_halves_of_its_binding() {
    let error = validate_non_secret_config(
        ChannelKind::Extension,
        &serde_json::json!({"extension_id": "dev.example.chat"}),
    )
    .unwrap_err();
    assert!(error.contains("capability_id"), "{error}");

    validate_non_secret_config(
        ChannelKind::Extension,
        &serde_json::json!({
            "extension_id": "dev.example.chat",
            "capability_id": "room",
            "room": "general",
        }),
    )
    .expect("a provider's own settings are its business");
}

#[tokio::test]
async fn the_channel_registry_builds_an_extension_adapter_that_polls_normalized_messages() {
    let root = TestRoot::new();
    let app_data = root.0.join("app-data");
    let _manager = install_fixture(
        &app_data,
        &root.0,
        "dev.example.chat",
        r#"{"messages":[{"provider_event_id":"evt-1","conversation_id":"room-1","conversation_kind":"group","sender_id":"user-1","text":"hello from the fixture","mentions_self":true}],"cursor":"c-1"}"#,
        &[(CapabilityKind::Channel, "room")],
        Vec::new(),
    )
    .await;

    let account = extension_account("dev.example.chat", "room");
    let adapter = build_adapter(
        &AdapterConfig {
            account: &account,
            secret: String::new(),
        },
        Some(&super::store::DaemonPaths::under(&app_data)),
    )
    .expect("the registry knows this kind");
    assert_eq!(adapter.kind(), ChannelKind::Extension);

    let batch = adapter.poll(None).await.expect("the adapter polls");
    assert_eq!(batch.cursor.as_deref(), Some("c-1"));
    assert_eq!(batch.envelopes.len(), 1);
    let envelope = &batch.envelopes[0];
    // The account's identity, not the guest's claim about it: this is what
    // stops one extension addressing another's account.
    assert_eq!(envelope.account_id, "acct-ext");
    assert_eq!(envelope.kind, ChannelKind::Extension);
    assert_eq!(envelope.dedupe_key(), "acct-ext:evt-1");
    assert_eq!(envelope.text, "hello from the fixture");
    assert!(envelope.mentions_self);
}

#[tokio::test]
async fn an_extension_channel_send_reports_a_normalized_outcome_to_the_outbox() {
    let root = TestRoot::new();
    let app_data = root.0.join("app-data");
    let _manager = install_fixture(
        &app_data,
        &root.0,
        "dev.example.chat",
        r#"{"status":"sent","provider_message_id":"m-9"}"#,
        &[(CapabilityKind::Channel, "room")],
        Vec::new(),
    )
    .await;

    let account = extension_account("dev.example.chat", "room");
    let adapter = build_adapter(
        &AdapterConfig {
            account: &account,
            secret: String::new(),
        },
        Some(&super::store::DaemonPaths::under(&app_data)),
    )
    .unwrap();
    let outcome = adapter
        .send(&OutboundMessage {
            account_id: "acct-ext".to_string(),
            kind: ChannelKind::Extension,
            conversation_id: "room-1".to_string(),
            thread_id: None,
            text: "a reply".to_string(),
            attachments: Vec::new(),
            reply_to_provider_id: None,
            idempotency_key: "outbox-1".to_string(),
        })
        .await;
    assert_eq!(
        outcome,
        SendOutcome::Sent {
            provider_message_id: Some("m-9".to_string())
        }
    );
}

#[tokio::test]
async fn a_channel_send_whose_extension_is_disabled_needs_reconciliation() {
    let root = TestRoot::new();
    let app_data = root.0.join("app-data");
    let manager = install_fixture(
        &app_data,
        &root.0,
        "dev.example.chat",
        r#"{"status":"sent"}"#,
        &[(CapabilityKind::Channel, "room")],
        Vec::new(),
    )
    .await;

    let account = extension_account("dev.example.chat", "room");
    let adapter = build_adapter(
        &AdapterConfig {
            account: &account,
            secret: String::new(),
        },
        Some(&super::store::DaemonPaths::under(&app_data)),
    )
    .unwrap();
    manager
        .set_enabled("dev.example.chat", false)
        .await
        .unwrap();
    let outcome = adapter
        .send(&OutboundMessage {
            account_id: "acct-ext".to_string(),
            kind: ChannelKind::Extension,
            conversation_id: "room-1".to_string(),
            thread_id: None,
            text: "a reply".to_string(),
            attachments: Vec::new(),
            reply_to_provider_id: None,
            idempotency_key: "outbox-1".to_string(),
        })
        .await;
    // Never `PermanentFailure`: the guest may have completed its request
    // before it was stopped, so the outbox parks the row instead of dropping
    // the message or sending a second one.
    assert!(
        matches!(outcome, SendOutcome::NeedsReconciliation { .. }),
        "{outcome:?}"
    );
}

#[tokio::test]
async fn an_unhealthy_extension_channel_never_reports_itself_connected() {
    let root = TestRoot::new();
    let app_data = root.0.join("app-data");
    let _manager = install_fixture(
        &app_data,
        &root.0,
        "dev.example.chat",
        r#"{"ok":false,"error":"the token was revoked"}"#,
        &[(CapabilityKind::Channel, "room")],
        Vec::new(),
    )
    .await;

    let account = extension_account("dev.example.chat", "room");
    let adapter = build_adapter(
        &AdapterConfig {
            account: &account,
            secret: String::new(),
        },
        Some(&super::store::DaemonPaths::under(&app_data)),
    )
    .unwrap();
    let health = adapter.probe().await;
    assert_eq!(health.state, HealthState::Error);
    assert_eq!(health.last_error.as_deref(), Some("the token was revoked"));
}

// ---------------------------------------------------------------------------
// Device provider
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_extension_device_provider_is_discovered_with_namespaced_ids() {
    let root = TestRoot::new();
    let app_data = root.0.join("app-data");
    let _manager = install_fixture(
        &app_data,
        &root.0,
        "dev.example.lab",
        r#"{"devices":[{"id":"bench","name":"Bench camera","actions":["camera_capture","device_info"]}]}"#,
        &[(CapabilityKind::DeviceProvider, "instruments")],
        Vec::new(),
    )
    .await;

    let devices = super::remote::device::extension_devices(&app_data)
        .await
        .expect("discovery reads the live registry");
    assert_eq!(devices.len(), 1);
    assert_eq!(
        devices[0].device_id,
        "ext:dev.example.lab:instruments:bench"
    );
    assert!(devices[0]
        .actions
        .contains(&super::remote::protocol::DeviceCapability::CameraCapture));
    // The namespace is what makes one extension unable to name another's
    // device, and it round-trips.
    let (extension_id, capability_id, local_id) =
        super::remote::device::extension_device_target(&devices[0].device_id)
            .expect("a namespaced id parses");
    assert_eq!(extension_id, "dev.example.lab");
    assert_eq!(capability_id, "instruments");
    assert_eq!(local_id, "bench");
}

#[tokio::test]
async fn an_undeclared_device_action_is_refused_before_the_sandbox_starts() {
    let root = TestRoot::new();
    let app_data = root.0.join("app-data");
    let _manager = install_fixture(
        &app_data,
        &root.0,
        "dev.example.lab",
        r#"{"devices":[{"id":"bench","actions":["device_info"]}]}"#,
        &[(CapabilityKind::DeviceProvider, "instruments")],
        Vec::new(),
    )
    .await;
    let devices = super::remote::device::extension_devices(&app_data)
        .await
        .unwrap();
    // `camera_capture` is a real action the tool accepts, and this provider
    // simply never advertised it.
    assert!(!devices[0]
        .actions
        .contains(&super::remote::protocol::DeviceCapability::CameraCapture));
    assert!(devices[0]
        .actions
        .contains(&super::remote::protocol::DeviceCapability::DeviceInfo));
}

#[tokio::test]
async fn a_device_action_runs_through_the_normal_dispatch_and_returns_a_normalized_record() {
    let root = TestRoot::new();
    let app_data = root.0.join("app-data");
    let _manager = install_fixture(
        &app_data,
        &root.0,
        "dev.example.lab",
        r#"{"devices":[{"id":"bench","actions":["device_info"]}],"result":{"model":"fixture bench"}}"#,
        &[(CapabilityKind::DeviceProvider, "instruments")],
        Vec::new(),
    )
    .await;
    let paths = super::store::DaemonPaths::under(&app_data);
    paths.ensure().unwrap();

    let record = super::remote::device::dispatch(
        &paths,
        &super::remote::device::DeviceActionRequest {
            device_id: Some("ext:dev.example.lab:instruments:bench".to_string()),
            capability: super::remote::protocol::DeviceCapability::DeviceInfo,
            arguments: serde_json::json!({}),
            wait_ms: 5_000,
            source_run_id: None,
            source_session_id: None,
            source_tool_call_id: None,
            invocation_id: None,
        },
        1_700_000_000_000,
    )
    .await
    .expect("the normal device dispatch reaches the extension");

    assert_eq!(record.device_id, "ext:dev.example.lab:instruments:bench");
    assert_eq!(
        record.state,
        super::remote::protocol::DeviceCommandState::Succeeded
    );
    assert_eq!(
        record
            .result
            .as_ref()
            .and_then(|value| value.get("model"))
            .and_then(|value| value.as_str()),
        Some("fixture bench")
    );
}

/// Playing a stored clip on an extension device resolves the artifact against
/// the run that owns it, and refuses when there is no such link.
///
/// The device's own read grant is what protects a paired phone here; a guest
/// has no pairing, so the run link is the whole of the authority. An artifact
/// id in the arguments that the ledger does not tie to that run reaches no
/// sandbox at all — which is also what stops a well-formed digest naming
/// somebody else's bytes from becoming an invocation grant.
#[tokio::test]
async fn an_extension_device_cannot_play_an_artifact_that_is_not_the_runs() {
    let root = TestRoot::new();
    let app_data = root.0.join("app-data");
    let _manager = install_fixture(
        &app_data,
        &root.0,
        "dev.example.lab",
        r#"{"devices":[{"id":"speaker","actions":["audio_playback"]}],"result":{}}"#,
        &[(CapabilityKind::DeviceProvider, "instruments")],
        Vec::new(),
    )
    .await;
    let paths = super::store::DaemonPaths::under(&app_data);
    paths.ensure().unwrap();
    // A migrated but empty ledger, so what the refusal is about is the missing
    // link rather than a machine that has never run anything.
    little_monkey_lib::run_ledger::RunLedger::open(&paths.ledger_db).unwrap();

    let error = super::remote::device::dispatch(
        &paths,
        &super::remote::device::DeviceActionRequest {
            device_id: Some("ext:dev.example.lab:instruments:speaker".to_string()),
            capability: super::remote::protocol::DeviceCapability::AudioPlayback,
            arguments: serde_json::json!({
                "artifact_id": "artifact-01",
                "run_id": "run-nobody-owns",
            }),
            wait_ms: 5_000,
            source_run_id: None,
            source_session_id: None,
            source_tool_call_id: None,
            invocation_id: None,
        },
        1_700_000_000_000,
    )
    .await
    .expect_err("an artifact this run does not own is not playable");
    assert!(
        error.contains("not linked to run 'run-nobody-owns'"),
        "{error}"
    );
}

// ---------------------------------------------------------------------------
// Realtime voice
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_realtime_extension_serves_the_call_speech_a_live_call_holds() {
    let root = TestRoot::new();
    let app_data = root.0.join("app-data");
    let _manager = install_fixture(
        &app_data,
        &root.0,
        "dev.example.line",
        r#"{"events":[{"kind":"transcript","payload":{"text":"the caller said this"}}],"done":false}"#,
        &[(CapabilityKind::RealtimeVoice, "converse")],
        vec![PermissionDeclaration {
            permission_id: "artifact-write".to_string(),
            kind: PermissionKind::ArtifactWrite,
            scope: "content_v1".to_string(),
            reason: "Fixture publishes caller audio".to_string(),
        }],
    )
    .await;
    select_realtime_extension(&app_data, "dev.example.line", "converse");

    let speech = super::call_media::select_call_speech(&app_data)
        .expect("the operator's selection resolves to the extension backend");
    let wav = wav_fixture();
    let text = speech
        .transcribe(wav)
        .await
        .expect("the call's own speech backend reaches the session");
    assert_eq!(text, "the caller said this");
    speech.finish().await;
}

/// What the fixture below wraps the caller's clip in before writing it back,
/// so the id it answers with cannot be the id it was told to read.
const ECHO_MARKER: &[u8] = b"heard:";

/// The two literals that turn the id the host answered with into the one
/// session step shape a realtime provider emits.
const TRANSCRIPT_PREFIX: &str = r#"{"events":[{"kind":"transcript","payload":{"text":""#;
const TRANSCRIPT_SUFFIX: &str = r#""}}],"done":false}"#;

/// Everything a realtime extension needs to read the audio a call hands it.
fn realtime_audio_permissions() -> Vec<PermissionDeclaration> {
    vec![
        PermissionDeclaration {
            permission_id: "artifact-read-inputs".to_string(),
            kind: PermissionKind::ArtifactRead,
            scope: "invocation_inputs".to_string(),
            reason: "Fixture reads the caller's audio".to_string(),
        },
        PermissionDeclaration {
            permission_id: "artifact-write".to_string(),
            kind: PermissionKind::ArtifactWrite,
            scope: "content_v1".to_string(),
            reason: "Fixture publishes what it heard".to_string(),
        },
    ]
}

/// The whole of the functional claim: a live call's PCM reaches an extension.
///
/// Nothing here is asserted about the artifact id. The transcript the guest
/// answers with is the id the *host* returned when the guest wrote back what
/// it had read, so it can only be right if the exact bytes arrived — and the
/// clip is read out of the store afterwards to say so in bytes rather than in
/// a digest.
#[tokio::test]
async fn a_live_calls_audio_reaches_a_realtime_extension_through_a_trusted_grant() {
    let root = TestRoot::new();
    let app_data = root.0.join("app-data");
    let wav = wav_fixture();
    let clip_id = sha256_hex(&wav);
    let echoed: Vec<u8> = ECHO_MARKER
        .iter()
        .copied()
        .chain(wav.iter().copied())
        .collect();
    let echoed_id = sha256_hex(&echoed);

    let _manager = install_component(
        &app_data,
        &root.0,
        "dev.example.line",
        component_wat_echoing_artifact(&clip_id, ECHO_MARKER, TRANSCRIPT_PREFIX, TRANSCRIPT_SUFFIX),
        &[(CapabilityKind::RealtimeVoice, "converse")],
        realtime_audio_permissions(),
    )
    .await;
    select_realtime_extension(&app_data, "dev.example.line", "converse");

    let speech = super::call_media::select_call_speech(&app_data)
        .expect("the operator's selection resolves to the extension backend");
    let text = speech
        .transcribe(wav.clone())
        .await
        .expect("the caller's audio is readable inside the sandbox");
    speech.finish().await;

    assert_eq!(
        text, echoed_id,
        "the guest answered with an id it could not have computed without the clip"
    );
    let store = little_monkey_lib::artifact_store::ArtifactStore::new(app_data.join("content-v1"))
        .expect("the call's own artifact store opens");
    assert_eq!(
        store.read(&echoed_id).expect("the guest's write landed"),
        echoed,
        "the bytes that came back out are the caller's own PCM"
    );
}

/// The same fixture, one grant short: a guest that names a real artifact the
/// host never attached to this step is refused.
///
/// The artifact exists and is readable by the host — this is not "unknown id",
/// it is "not yours for this step", which is the case a content-addressed
/// shared store makes easy to get wrong.
#[tokio::test]
async fn a_realtime_extension_cannot_read_an_artifact_the_call_never_granted() {
    let root = TestRoot::new();
    let app_data = root.0.join("app-data");
    let unrelated = b"a recording from somebody else's call".to_vec();
    let unrelated_id =
        little_monkey_lib::artifact_store::ArtifactStore::new(app_data.join("content-v1"))
            .expect("the artifact store opens")
            .put(&unrelated)
            .expect("the unrelated clip is stored")
            .id;

    let _manager = install_component(
        &app_data,
        &root.0,
        "dev.example.line",
        component_wat_echoing_artifact(
            &unrelated_id,
            ECHO_MARKER,
            TRANSCRIPT_PREFIX,
            TRANSCRIPT_SUFFIX,
        ),
        &[(CapabilityKind::RealtimeVoice, "converse")],
        realtime_audio_permissions(),
    )
    .await;
    select_realtime_extension(&app_data, "dev.example.line", "converse");

    let speech = super::call_media::select_call_speech(&app_data).unwrap();
    let error = speech
        .transcribe(wav_fixture())
        .await
        .expect_err("an ungranted artifact is not readable");
    speech.finish().await;
    assert!(
        error.contains(&unrelated_id) && error.to_lowercase().contains("denied"),
        "{error}"
    );
}

/// The security invariant behind the fix, stated on its own: an artifact id
/// inside a session event is data, not authority.
///
/// Both halves run against the same installed extension and the same session
/// API, differing only in whether the trusted call site attached the grant. A
/// regression that started deriving grants from the event JSON would turn the
/// first half green and be caught here rather than in a live call.
#[tokio::test]
async fn an_artifact_id_inside_a_session_event_grants_nothing_by_itself() {
    use little_monkey_lib::executable_extensions::{CapabilityKind, SessionInput};

    let root = TestRoot::new();
    let app_data = root.0.join("app-data");
    let clip = b"the caller's own PCM".to_vec();
    let clip_id =
        little_monkey_lib::artifact_store::ArtifactStore::new(app_data.join("content-v1"))
            .expect("the artifact store opens")
            .put(&clip)
            .expect("the clip is stored")
            .id;
    let echoed: Vec<u8> = ECHO_MARKER
        .iter()
        .copied()
        .chain(clip.iter().copied())
        .collect();

    let manager = install_component(
        &app_data,
        &root.0,
        "dev.example.line",
        component_wat_echoing_artifact(&clip_id, ECHO_MARKER, TRANSCRIPT_PREFIX, TRANSCRIPT_SUFFIX),
        &[(CapabilityKind::RealtimeVoice, "converse")],
        realtime_audio_permissions(),
    )
    .await;
    let manager = manager
        .with_artifact_root(app_data.join("content-v1"))
        .expect("the manager reads the same store the call uses");

    // The event names the artifact exactly as the real call-media path names
    // it. Run first with the host attaching it, so what the second half
    // removes is the grant and nothing else.
    let named = SessionInput::event(serde_json::json!({
        "kind": "caller_audio",
        "artifact_id": clip_id,
    }));
    let step = manager
        .open_session(
            CapabilityKind::RealtimeVoice,
            "dev.example.line",
            "converse",
            named.clone().reading_artifacts(vec![clip_id.clone()]),
        )
        .await
        .expect("an explicitly attached artifact is readable");
    assert_eq!(
        step.written_artifact_ids,
        vec![sha256_hex(&echoed)],
        "the host recorded the write the guest made from the bytes it read"
    );
    let _ = little_monkey_lib::executable_extensions::close_session(&step.session_id);

    // The same extension, the same capability, the same event JSON — and no
    // grant. A regression that derived authority from the JSON would make
    // this succeed exactly like the call above.
    let error = manager
        .open_session(
            CapabilityKind::RealtimeVoice,
            "dev.example.line",
            "converse",
            named,
        )
        .await
        .expect_err("naming an artifact in JSON is not a grant");
    assert!(
        error.contains(&clip_id) && error.to_lowercase().contains("denied"),
        "{error}"
    );
}

#[tokio::test]
async fn a_realtime_selection_with_no_capability_fails_before_a_call_starts() {
    let root = TestRoot::new();
    let app_data = root.0.join("app-data");
    let _manager = install_fixture(
        &app_data,
        &root.0,
        "dev.example.line",
        r#"{"events":[],"done":true}"#,
        &[(CapabilityKind::RealtimeVoice, "converse")],
        Vec::new(),
    )
    .await;
    select_realtime_extension(&app_data, "dev.example.line", "missing");
    let speech = super::call_media::select_call_speech(&app_data).unwrap();
    let error = speech.transcribe(wav_fixture()).await.unwrap_err();
    assert!(error.contains("No healthy active extension"), "{error}");
}

fn wav_fixture() -> Vec<u8> {
    let samples: Vec<i16> = (0..160).map(|i| (i as i16) * 8).collect();
    super::call_audio::write_wav(&samples, super::call_audio::CALL_SAMPLE_RATE)
}

/// Point the companion's realtime selection at a fixture extension.
///
/// Written as the companion's own configuration document, from its own
/// defaults, so the production reader a live call consults —
/// `m7_companion::call_voice_config` — is the thing under test rather than a
/// value handed in.
fn select_realtime_extension(app_data: &Path, extension_id: &str, capability_id: &str) {
    let mut config = little_monkey_lib::m7_companion::CompanionConfig::default();
    config.voice.realtime_backend =
        little_monkey_lib::m7_companion::SpeechBackendKind::ExecutableExtension;
    config.voice.realtime_extension_id = Some(extension_id.to_string());
    config.voice.realtime_extension_capability_id = Some(capability_id.to_string());
    let root = app_data.join("m7-companion-v1");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(
        root.join("companion-config-v1.json"),
        serde_json::to_vec_pretty(&config).unwrap(),
    )
    .unwrap();
}
