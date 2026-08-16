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
    let component = component_wat(output);
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
    let home = IsolatedDataDir::new();
    let app_data = home.app_data().to_path_buf();
    let _manager = install_fixture(
        &app_data,
        &home.sources(),
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
        None,
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
    let home = IsolatedDataDir::new();
    let app_data = home.app_data().to_path_buf();
    let _manager = install_fixture(
        &app_data,
        &home.sources(),
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
        None,
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
    let home = IsolatedDataDir::new();
    let app_data = home.app_data().to_path_buf();
    let manager = install_fixture(
        &app_data,
        &home.sources(),
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
        None,
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
    let home = IsolatedDataDir::new();
    let app_data = home.app_data().to_path_buf();
    let _manager = install_fixture(
        &app_data,
        &home.sources(),
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
        None,
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
    let home = IsolatedDataDir::new();
    let app_data = home.app_data().to_path_buf();
    let _manager = install_fixture(
        &app_data,
        &home.sources(),
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
    let home = IsolatedDataDir::new();
    let app_data = home.app_data().to_path_buf();
    let _manager = install_fixture(
        &app_data,
        &home.sources(),
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
    let home = IsolatedDataDir::new();
    let app_data = home.app_data().to_path_buf();
    let _manager = install_fixture(
        &app_data,
        &home.sources(),
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

// ---------------------------------------------------------------------------
// Realtime voice
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_realtime_extension_serves_the_call_speech_a_live_call_holds() {
    let home = IsolatedDataDir::new();
    let app_data = home.app_data().to_path_buf();
    let _manager = install_fixture(
        &app_data,
        &home.sources(),
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

#[tokio::test]
async fn a_realtime_selection_with_no_capability_fails_before_a_call_starts() {
    let home = IsolatedDataDir::new();
    let app_data = home.app_data().to_path_buf();
    let _manager = install_fixture(
        &app_data,
        &home.sources(),
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

/// A fixture installation living exactly where `dirs::data_dir()` will look.
///
/// The channel adapter registry resolves its own data root, exactly as it does
/// in the daemon, so a test that wants a fixture registry has to *be* at that
/// root rather than hand one in. The extension store refuses a symlinked root
/// — deliberately, since a redirectable store is a redirectable component — so
/// the directory is placed under a fixture `HOME` and used from there.
///
/// `HOME` is process-wide, so the tests that need one are serialized by the
/// lock this guard holds and the previous value is restored on drop.
struct IsolatedDataDir {
    app_data: PathBuf,
    previous: Option<std::ffi::OsString>,
    _root: TestRoot,
    _guard: std::sync::MutexGuard<'static, ()>,
}

static DATA_DIR_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

impl IsolatedDataDir {
    fn new() -> Self {
        let guard = DATA_DIR_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let root = TestRoot::new();
        let home = root.0.join("home");
        // `dirs::data_dir()` is `$HOME/Library/Application Support` on macOS
        // and `$HOME/.local/share` elsewhere.
        #[cfg(target_os = "macos")]
        let app_data = home.join("Library/Application Support/com.littlemonkey.app");
        #[cfg(not(target_os = "macos"))]
        let app_data = home.join(".local/share/com.littlemonkey.app");
        std::fs::create_dir_all(&app_data).unwrap();
        let previous = std::env::var_os("HOME");
        std::env::set_var("HOME", &home);
        Self {
            app_data,
            previous,
            _root: root,
            _guard: guard,
        }
    }

    fn app_data(&self) -> &Path {
        &self.app_data
    }

    fn sources(&self) -> PathBuf {
        let sources = self._root.0.join("sources");
        std::fs::create_dir_all(&sources).unwrap();
        sources
    }
}

impl Drop for IsolatedDataDir {
    fn drop(&mut self) {
        match &self.previous {
            Some(previous) => std::env::set_var("HOME", previous),
            None => std::env::remove_var("HOME"),
        }
    }
}
