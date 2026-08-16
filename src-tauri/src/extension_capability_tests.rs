//! End-to-end proof that each capability an executable extension declares is
//! actually consumed by the subsystem that owns it.
//!
//! Every test here installs a real Component Model component through the real
//! install/enable/start lifecycle and then calls the *subsystem's own* entry
//! point — `embed_batch_under`, `synthesize_speech_to_wav`,
//! `providers::run_extension_chat`, `collect_extension_source`, and so on.
//! None of them calls `ExtensionManager::invoke` directly, because that would
//! prove only that the runtime runs a component, which the runtime's own tests
//! already prove. What is under test here is the wiring: that a healthy
//! extension is discovered by the registry the feature actually reads, that
//! its answer is normalized into the shape the feature actually consumes, and
//! that a dishonest answer is refused before it reaches anything.

use std::path::Path;

use crate::executable_extensions::test_fixtures::{
    component_wat, component_wat_writing_artifact, fixture_wav, manifest_for, runtime_guard,
    write_manifest_bundle, TestRoot,
};
use crate::executable_extensions::{
    CapabilityDeclaration, CapabilityKind, ExtensionManager, ExtensionManifest, PermissionGrant,
};
use crate::package_ecosystem::SemanticVersion;

/// Build and install one fixture extension that declares exactly `capabilities`
/// and answers every call with `component`.
async fn install_fixture(
    app_data: &Path,
    source_root: &Path,
    extension_id: &str,
    component: Vec<u8>,
    capabilities: Vec<(CapabilityKind, &str)>,
    permissions: Vec<crate::executable_extensions::PermissionDeclaration>,
) -> ExtensionManager {
    let source = source_root.join(extension_id);
    let mut manifest: ExtensionManifest = manifest_for(
        extension_id,
        &source,
        &component,
        SemanticVersion::new(1, 0, 0),
    );
    manifest.capabilities = capabilities
        .into_iter()
        .map(|(kind, capability_id)| CapabilityDeclaration {
            capability_id: capability_id.to_string(),
            kind,
            display_name: format!("Fixture {capability_id}"),
            description: "Fixture capability".to_string(),
            input_schema: serde_json::json!({"type": "object"}),
        })
        .collect();
    manifest.permissions = permissions;
    write_manifest_bundle(&source, &component, &manifest);
    let manager = ExtensionManager::new(app_data).unwrap();
    install_running_with_grants(&manager, &source, extension_id).await;
    manager
}

async fn install_running_with_grants(
    manager: &ExtensionManager,
    source: &Path,
    extension_id: &str,
) {
    let preview = manager.discover(source).unwrap();
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
            source,
            crate::executable_extensions::Approval {
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
}

fn artifact_write_permission() -> crate::executable_extensions::PermissionDeclaration {
    crate::executable_extensions::PermissionDeclaration {
        permission_id: "artifact-write".to_string(),
        kind: crate::executable_extensions::PermissionKind::ArtifactWrite,
        scope: "content_v1".to_string(),
        reason: "Fixture writes bounded output".to_string(),
    }
}

// ---------------------------------------------------------------------------
// Embedding provider
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_extension_embedding_provider_answers_the_normal_embedding_call() {
    let _runtime = runtime_guard();
    let root = TestRoot::new();
    let app_data = root.0.join("app-data");
    // Two unit-length-ish vectors of the stack's declared dimension. The
    // embedding core L2-normalizes whatever comes back, so the assertion below
    // is about shape and provenance, not about the exact numbers.
    let _manager = install_fixture(
        &app_data,
        &root.0,
        "dev.example.embed",
        component_wat(r#"{"vectors":[[3.0,4.0],[0.0,1.0]]}"#, ""),
        vec![(CapabilityKind::EmbeddingProvider, "vectors")],
        Vec::new(),
    )
    .await;

    let spec = crate::knowledge_core::EmbeddingSpec {
        backend: crate::knowledge_core::EmbeddingBackend::Extension,
        model_id_or_tag: "vectors".to_string(),
        dim: 2,
        query_prefix: String::new(),
        doc_prefix: String::new(),
        extension_id: Some("dev.example.embed".to_string()),
    };
    spec.validate().expect("an owned extension spec is valid");

    let vectors = crate::knowledge_core::embed_batch_under(
        &app_data,
        &spec,
        &["one".to_string(), "two".to_string()],
        false,
    )
    .await
    .expect("the normal embedding path reaches the extension");

    assert_eq!(vectors.len(), 2);
    assert_eq!(vectors[0].len(), 2);
    // Normalized by the shared embedding core, exactly as a local server's
    // vectors are: 3/5, 4/5.
    assert!((vectors[0][0] - 0.6).abs() < 1e-5, "{:?}", vectors[0]);
    assert!((vectors[0][1] - 0.8).abs() < 1e-5, "{:?}", vectors[0]);
}

#[tokio::test]
async fn an_embedding_provider_that_answers_the_wrong_dimension_is_refused() {
    let _runtime = runtime_guard();
    let root = TestRoot::new();
    let app_data = root.0.join("app-data");
    let _manager = install_fixture(
        &app_data,
        &root.0,
        "dev.example.embed",
        component_wat(r#"{"vectors":[[1.0,2.0,3.0]]}"#, ""),
        vec![(CapabilityKind::EmbeddingProvider, "vectors")],
        Vec::new(),
    )
    .await;
    let spec = crate::knowledge_core::EmbeddingSpec {
        backend: crate::knowledge_core::EmbeddingBackend::Extension,
        model_id_or_tag: "vectors".to_string(),
        dim: 2,
        query_prefix: String::new(),
        doc_prefix: String::new(),
        extension_id: Some("dev.example.embed".to_string()),
    };
    let error =
        crate::knowledge_core::embed_batch_under(&app_data, &spec, &["one".to_string()], false)
            .await
            .expect_err("a wrong-dimension answer never reaches an index");
    assert!(error.contains("expects 2"), "{error}");
}

#[tokio::test]
async fn a_stack_bound_to_one_publisher_is_not_embedded_by_another() {
    let _runtime = runtime_guard();
    let root = TestRoot::new();
    let app_data = root.0.join("app-data");
    let _manager = install_fixture(
        &app_data,
        &root.0,
        "dev.example.embed",
        component_wat(r#"{"vectors":[[1.0,0.0]]}"#, ""),
        vec![(CapabilityKind::EmbeddingProvider, "vectors")],
        Vec::new(),
    )
    .await;
    let spec = crate::knowledge_core::EmbeddingSpec {
        backend: crate::knowledge_core::EmbeddingBackend::Extension,
        model_id_or_tag: "vectors".to_string(),
        dim: 2,
        query_prefix: String::new(),
        doc_prefix: String::new(),
        extension_id: Some("dev.other.embed".to_string()),
    };
    let error =
        crate::knowledge_core::embed_batch_under(&app_data, &spec, &["one".to_string()], false)
            .await
            .expect_err("a stack's recorded owner is enforced");
    assert!(error.contains("owner changed"), "{error}");
}

#[test]
fn an_embedding_spec_must_match_its_backend() {
    let mut spec = crate::knowledge_core::EmbeddingSpec {
        backend: crate::knowledge_core::EmbeddingBackend::Extension,
        model_id_or_tag: "vectors".to_string(),
        dim: 2,
        query_prefix: String::new(),
        doc_prefix: String::new(),
        extension_id: None,
    };
    assert!(spec.validate().unwrap_err().contains("owning extension"));
    spec.backend = crate::knowledge_core::EmbeddingBackend::Llama;
    spec.extension_id = Some("dev.example.embed".to_string());
    assert!(spec
        .validate()
        .unwrap_err()
        .contains("Only an executable embedding provider"));
}

// ---------------------------------------------------------------------------
// Text to speech
// ---------------------------------------------------------------------------

async fn companion_with_extension_speech(
    app_data: &Path,
    field: &str,
    extension_id: &str,
    capability_id: &str,
) {
    let state = crate::m7_companion::M7CompanionState::production(app_data).unwrap();
    let mut config = state.config_for_test();
    match field {
        "tts" => {
            config.voice.tts_backend = crate::m7_companion::SpeechBackendKind::ExecutableExtension;
            config.voice.tts_extension_id = Some(extension_id.to_string());
            config.voice.tts_extension_capability_id = Some(capability_id.to_string());
        }
        _ => {
            config.voice.realtime_backend =
                crate::m7_companion::SpeechBackendKind::ExecutableExtension;
            config.voice.realtime_extension_id = Some(extension_id.to_string());
            config.voice.realtime_extension_capability_id = Some(capability_id.to_string());
        }
    }
    state.save_config_for_test(config);
}

#[tokio::test]
async fn an_extension_tts_provider_produces_the_audio_the_normal_path_writes() {
    let _runtime = runtime_guard();
    let root = TestRoot::new();
    let app_data = root.0.join("app-data");
    let wav = fixture_wav(8_000, &[0, 512, -512, 0]);
    let _manager = install_fixture(
        &app_data,
        &root.0,
        "dev.example.voice",
        component_wat_writing_artifact(
            &wav,
            r#"{"artifact_id":""#,
            r#"","media_type":"audio/wav"}"#,
        ),
        vec![(CapabilityKind::Tts, "speak")],
        vec![artifact_write_permission()],
    )
    .await;
    companion_with_extension_speech(&app_data, "tts", "dev.example.voice", "speak").await;

    let destination = root.0.join("spoken.wav");
    crate::m7_companion::synthesize_speech_to_wav(&app_data, "hello", &destination)
        .await
        .expect("the normal synthesis path reaches the extension");

    let written = std::fs::read(&destination).expect("the clip was published");
    assert_eq!(written, wav, "the consumer received the guest's own audio");
}

#[tokio::test]
async fn a_tts_provider_that_names_audio_it_did_not_write_is_refused() {
    let _runtime = runtime_guard();
    let root = TestRoot::new();
    let app_data = root.0.join("app-data");
    // A plausible-looking but fabricated content id. It is a well-formed
    // sha256, so nothing but the ownership check stands between it and the
    // speaker.
    let _manager = install_fixture(
        &app_data,
        &root.0,
        "dev.example.voice",
        component_wat(
            r#"{"artifact_id":"0000000000000000000000000000000000000000000000000000000000000000"}"#,
            "",
        ),
        vec![(CapabilityKind::Tts, "speak")],
        Vec::new(),
    )
    .await;
    companion_with_extension_speech(&app_data, "tts", "dev.example.voice", "speak").await;

    let destination = root.0.join("spoken.wav");
    let error = crate::m7_companion::synthesize_speech_to_wav(&app_data, "hello", &destination)
        .await
        .expect_err("a fabricated artifact id is refused");
    assert!(error.contains("did not write"), "{error}");
    assert!(!destination.exists());
}

#[tokio::test]
async fn a_disabled_tts_provider_stops_serving_the_normal_path() {
    let _runtime = runtime_guard();
    let root = TestRoot::new();
    let app_data = root.0.join("app-data");
    let wav = fixture_wav(8_000, &[0, 128]);
    let manager = install_fixture(
        &app_data,
        &root.0,
        "dev.example.voice",
        component_wat_writing_artifact(&wav, r#"{"artifact_id":""#, r#""}"#),
        vec![(CapabilityKind::Tts, "speak")],
        vec![artifact_write_permission()],
    )
    .await;
    companion_with_extension_speech(&app_data, "tts", "dev.example.voice", "speak").await;
    manager
        .set_enabled("dev.example.voice", false)
        .await
        .unwrap();

    let destination = root.0.join("spoken.wav");
    let error = crate::m7_companion::synthesize_speech_to_wav(&app_data, "hello", &destination)
        .await
        .expect_err("a disabled provider serves nothing");
    assert!(error.contains("No healthy active extension"), "{error}");
}

// ---------------------------------------------------------------------------
// Speech to text
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_extension_stt_provider_answers_the_normal_transcription_path() {
    let _runtime = runtime_guard();
    let root = TestRoot::new();
    let app_data = root.0.join("app-data");
    let _manager = install_fixture(
        &app_data,
        &root.0,
        "dev.example.ears",
        component_wat(r#"{"text":"a fixture transcript"}"#, ""),
        vec![(CapabilityKind::Stt, "listen")],
        Vec::new(),
    )
    .await;
    let state = crate::m7_companion::M7CompanionState::production(&app_data).unwrap();
    let mut config = state.config_for_test();
    config.voice.backend = crate::m7_companion::TranscriptionBackendKind::ExecutableExtension;
    config.voice.extension_id = Some("dev.example.ears".to_string());
    config.voice.extension_capability_id = Some("listen".to_string());
    state.save_config_for_test(config);

    let audio = root.0.join("call.wav");
    std::fs::write(&audio, fixture_wav(8_000, &[0, 64, -64])).unwrap();
    let transcript = crate::m7_companion::transcribe_call_audio(&app_data, &audio)
        .await
        .expect("the normal transcription path reaches the extension");
    assert_eq!(transcript, "a fixture transcript");
}

// ---------------------------------------------------------------------------
// Connector
// ---------------------------------------------------------------------------

fn write_connector_catalog(app_data: &Path, extension_id: &str, capability_id: &str) -> String {
    std::fs::create_dir_all(app_data).unwrap();
    let account_id = "connector-1".to_string();
    let catalog = serde_json::json!({
        "version": 1,
        "accounts": [{
            "id": account_id,
            "provider": "extension",
            "label": "Fixture connector",
            "scopes": ["read"],
            "credential_ref": null,
            "identity": format!("{extension_id}:{capability_id}"),
            "created_at": 1,
            "last_verified_at": 1,
            "connection": {
                "extension_id": extension_id,
                "capability_id": capability_id,
                "version": "1.0.0",
            },
        }],
    });
    std::fs::write(
        app_data.join("connectors.json"),
        serde_json::to_vec_pretty(&catalog).unwrap(),
    )
    .unwrap();
    account_id
}

#[tokio::test]
async fn an_extension_connector_feeds_documents_into_the_normal_sync_pipeline() {
    let _runtime = runtime_guard();
    let root = TestRoot::new();
    let app_data = root.0.join("app-data");
    let document = b"the fixture document body".to_vec();
    let _manager = install_fixture(
        &app_data,
        &root.0,
        "dev.example.docs",
        component_wat_writing_artifact(
            &document,
            r#"{"documents":[{"id":"doc-1","artifact_id":""#,
            r#"","media_type":"text/plain"}],"cursor":"cursor-1"}"#,
        ),
        vec![(CapabilityKind::Connector, "docs")],
        vec![artifact_write_permission()],
    )
    .await;
    let account_id = write_connector_catalog(&app_data, "dev.example.docs", "docs");

    let (objects, cursor) = crate::knowledge_service::collect_extension_source_for_test(
        &app_data,
        &account_id,
        Some("everything"),
    )
    .await
    .expect("the normal collector reaches the extension");

    assert_eq!(objects.len(), 1);
    assert_eq!(objects[0].bytes, document);
    assert_eq!(objects[0].metadata.object_id, "extension-doc-1");
    assert_eq!(cursor.as_deref(), Some("cursor-1"));
}

#[tokio::test]
async fn a_connector_that_names_a_document_it_did_not_write_is_refused() {
    let _runtime = runtime_guard();
    let root = TestRoot::new();
    let app_data = root.0.join("app-data");
    let _manager = install_fixture(
        &app_data,
        &root.0,
        "dev.example.docs",
        component_wat(
            r#"{"documents":[{"id":"doc-1","artifact_id":"0000000000000000000000000000000000000000000000000000000000000000"}]}"#,
            "",
        ),
        vec![(CapabilityKind::Connector, "docs")],
        Vec::new(),
    )
    .await;
    let account_id = write_connector_catalog(&app_data, "dev.example.docs", "docs");
    let error =
        crate::knowledge_service::collect_extension_source_for_test(&app_data, &account_id, None)
            .await
            .expect_err("a fabricated document id is refused");
    assert!(error.contains("did not write"), "{error}");
}

// ---------------------------------------------------------------------------
// Model provider
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_extension_model_provider_is_listed_and_answers_a_model_query() {
    let _runtime = runtime_guard();
    let root = TestRoot::new();
    let app_data = root.0.join("app-data");
    let _manager = install_fixture(
        &app_data,
        &root.0,
        "dev.example.llm",
        component_wat(
            r#"{"models":[{"id":"fixture-small","context_length":4096}]}"#,
            "",
        ),
        vec![(CapabilityKind::ModelProvider, "chat")],
        Vec::new(),
    )
    .await;

    let models = crate::providers::extension_models(&app_data, "dev.example.llm", "chat")
        .await
        .expect("the normal model listing reaches the extension");
    assert_eq!(models.len(), 1);
    assert_eq!(models[0].id, "fixture-small");
    assert_eq!(models[0].context_length, Some(4096));
}

#[test]
fn an_extension_provider_id_round_trips_and_never_looks_like_an_endpoint() {
    let id = "extension:dev.example.llm:chat";
    assert_eq!(
        crate::providers::extension_provider_target(id),
        Some(("dev.example.llm".to_string(), "chat".to_string()))
    );
    assert_eq!(crate::providers::extension_provider_target("openai"), None);
    let error = crate::providers::resolve_base_url(id, &[]).unwrap_err();
    assert!(error.contains("secret slots"), "{error}");
}

// ---------------------------------------------------------------------------
// Sessions — the mechanism behind streaming completions and realtime voice
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_session_is_pinned_to_the_version_it_opened_against() {
    let _runtime = runtime_guard();
    let root = TestRoot::new();
    let app_data = root.0.join("app-data");
    let manager = install_fixture(
        &app_data,
        &root.0,
        "dev.example.llm",
        component_wat(
            r#"{"events":[{"kind":"text_delta","payload":{"text":"hi"}}],"done":false}"#,
            "",
        ),
        vec![(CapabilityKind::ModelProvider, "chat")],
        Vec::new(),
    )
    .await;

    let opened = manager
        .open_session(
            CapabilityKind::ModelProvider,
            "dev.example.llm",
            "chat",
            serde_json::json!({"model": "fixture-small"}),
        )
        .await
        .expect("a session opens against a healthy provider");
    assert_eq!(opened.events.len(), 1);
    assert_eq!(opened.events[0].kind, "text_delta");
    assert!(!opened.done);
    let binding = crate::executable_extensions::session_binding(&opened.session_id)
        .unwrap()
        .expect("the session is open");
    assert_eq!(binding.version, "1.0.0");
    assert_eq!(binding.kind, CapabilityKind::ModelProvider);

    // Disabling the extension mid-session must not be survivable: the next
    // step has nothing legitimate to run against.
    manager.set_enabled("dev.example.llm", false).await.unwrap();
    let error = manager
        .session_send(&opened.session_id, serde_json::json!({"kind": "pull"}))
        .await
        .expect_err("a disabled extension cannot continue a session");
    assert!(error.contains("enabled"), "{error}");
    assert!(
        crate::executable_extensions::session_binding(&opened.session_id)
            .unwrap()
            .is_none(),
        "a failed step ends the session rather than leaving it resumable"
    );
}

#[tokio::test]
async fn a_session_that_reports_done_is_closed_by_the_host() {
    let _runtime = runtime_guard();
    let root = TestRoot::new();
    let app_data = root.0.join("app-data");
    let manager = install_fixture(
        &app_data,
        &root.0,
        "dev.example.llm",
        component_wat(r#"{"events":[],"done":true}"#, ""),
        vec![(CapabilityKind::ModelProvider, "chat")],
        Vec::new(),
    )
    .await;
    let opened = manager
        .open_session(
            CapabilityKind::ModelProvider,
            "dev.example.llm",
            "chat",
            serde_json::json!({}),
        )
        .await
        .unwrap();
    assert!(opened.done);
    assert!(
        crate::executable_extensions::session_binding(&opened.session_id)
            .unwrap()
            .is_none()
    );
}

// ---------------------------------------------------------------------------
// Capability ownership
// ---------------------------------------------------------------------------

#[tokio::test]
async fn two_extensions_cannot_both_own_one_capability_id() {
    let _runtime = runtime_guard();
    let root = TestRoot::new();
    let app_data = root.0.join("app-data");
    let _first = install_fixture(
        &app_data,
        &root.0,
        "dev.example.first",
        component_wat("{}", ""),
        vec![(CapabilityKind::Tts, "speak")],
        Vec::new(),
    )
    .await;

    let source = root.0.join("dev.example.second");
    let component = component_wat("{}", "");
    let mut manifest = manifest_for(
        "dev.example.second",
        &source,
        &component,
        SemanticVersion::new(1, 0, 0),
    );
    manifest.capabilities = vec![CapabilityDeclaration {
        capability_id: "speak".to_string(),
        kind: CapabilityKind::Tts,
        display_name: "Speak".to_string(),
        description: "Collides on purpose".to_string(),
        input_schema: serde_json::json!({"type": "object"}),
    }];
    write_manifest_bundle(&source, &component, &manifest);
    let manager = ExtensionManager::new(&app_data).unwrap();
    let preview = manager.discover(&source).unwrap();
    let error = manager
        .install(
            &source,
            crate::executable_extensions::Approval {
                approval_digest: preview.approval_digest,
                grants: Vec::new(),
                allow_unsigned: true,
                allow_untrusted: false,
                allow_high_risk: false,
            },
        )
        .await
        .expect_err("a second owner for one capability id is refused");
    assert!(error.to_lowercase().contains("capab"), "{error}");
}

#[tokio::test]
async fn a_capability_of_the_wrong_kind_is_not_reachable_through_another_kinds_registry() {
    let _runtime = runtime_guard();
    let root = TestRoot::new();
    let app_data = root.0.join("app-data");
    let manager = install_fixture(
        &app_data,
        &root.0,
        "dev.example.tool",
        component_wat("{}", ""),
        vec![(CapabilityKind::Tool, "helper")],
        Vec::new(),
    )
    .await;
    assert!(manager
        .resolve_active_capability(CapabilityKind::ModelProvider, "helper")
        .is_err());
    assert!(manager
        .resolve_active_capability(CapabilityKind::Tool, "helper")
        .is_ok());
}
