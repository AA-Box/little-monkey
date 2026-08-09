//! K21: the published conformance suite, run against a live node.
//!
//! # Why this file exists when `conformance.rs` already has unit tests
//!
//! Those tests grade the *verdict logic* — that a skipped optional section
//! does not block a claim, that an unrun required check does. None of them
//! puts a byte on a socket, so none of them can tell whether the suite
//! actually agrees with the server this repository ships. K21's acceptance is
//! explicit that a run must exercise "the live pipeline rather than a mirror
//! of it", and a suite that has only ever been run against a mock is exactly
//! the mirror it warns about.
//!
//! So every test here binds a real ephemeral port, starts the real
//! `run_cli_server_with_m3_hub_and_endpoints` accept loop with a real run
//! ledger behind it in a private temp app-data directory, and calls the same
//! `conformance::run_suite` a third party would call from
//! `monkey-cli conformance`. The only fakes are the two loopback *runtimes*
//! (llama-server, Ollama), following `legacy_route_compatibility.rs`'s
//! convention exactly: no real model process exists in CI, and the boundary
//! being mocked is a model, never the HTTP, auth, routing or ledger layers
//! the suite is grading.

use little_monkey_lib::conformance::{
    self, CheckStatus, SectionId, SectionStatus, SuiteOptions, Verdict,
};
use little_monkey_lib::http_route_registry::{
    classify_request, AuthFamily, ClassificationInput, ListenerExposure, RouteDecision,
    RouteDenial, RouteId,
};
use little_monkey_lib::m3_runtime_hub::{
    M3DownloadTransport, M3HardwareProbe, M3HubConfig, M3HubResult, M3RuntimeHub,
    M3RuntimeHubDependencies, ReqwestM3DownloadTransport, SystemM3Clock,
};
use little_monkey_lib::runtime_adapter::{HardwareSnapshot, PlatformCapabilities};
use little_monkey_lib::server::{
    run_cli_server_with_m3_hub_and_endpoints, save_config_impl, ApiServerConfig, Backend,
    CliRuntimeEndpoints, Scope, TokenEntry,
};
use sha2::{Digest, Sha256};
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// The token every test authenticates with. `require_token` is deliberately
/// left **on**: a listener that serves loopback without a token makes
/// `contract.authentication` an honest skip, which leaves the required
/// section incomplete — correct behaviour, and not the posture a node
/// claiming compatibility should be tested in.
const TEST_TOKEN: &str = "lmk-conformance-suite-test-token";

const MODEL_ID: &str = "conformance-test-model";

const READY_BUDGET: Duration = Duration::from_secs(10);

fn next_test_id() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

async fn free_loopback_port() -> Option<u16> {
    match tokio::net::TcpListener::bind(("127.0.0.1", 0)).await {
        Ok(listener) => Some(listener.local_addr().expect("ephemeral address").port()),
        Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
            eprintln!("skipping the conformance suite: sandbox forbids local listeners");
            None
        }
        Err(error) => panic!("bind ephemeral test port: {error}"),
    }
}

fn token_digest(plaintext: &str) -> String {
    Sha256::digest(plaintext.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

struct TestHardware;

impl M3HardwareProbe for TestHardware {
    fn snapshot(&self) -> M3HubResult<HardwareSnapshot> {
        Ok(HardwareSnapshot {
            captured_at_ms: 1_000,
            total_ram_bytes: 16 * 1024 * 1024 * 1024,
            available_ram_bytes: 12 * 1024 * 1024 * 1024,
            logical_cpu_count: 8,
            platform: PlatformCapabilities::from_host("linux", "x86_64", Vec::new()),
        })
    }
}

fn test_m3_hub(root: &std::path::Path) -> Arc<M3RuntimeHub> {
    let download: Arc<dyn M3DownloadTransport> =
        Arc::new(ReqwestM3DownloadTransport::new().expect("test download transport"));
    Arc::new(
        M3RuntimeHub::new(
            root,
            M3HubConfig {
                storage_quota_bytes: 8 * 1024 * 1024 * 1024,
                storage_reserve_bytes: 1024 * 1024 * 1024,
                ..M3HubConfig::default()
            },
            M3RuntimeHubDependencies {
                clock: Arc::new(SystemM3Clock),
                hardware: Arc::new(TestHardware),
                download,
                catalogs: Vec::new(),
                runtimes: Vec::new(),
                runtime_reconciler: None,
                lan_factory: None,
            },
        )
        .expect("conformance test M3 hub"),
    )
}

/// A raw-TCP stand-in for a loopback runtime, same technique and same reason
/// as `legacy_route_compatibility.rs`'s.
fn spawn_fake_runtime(
    respond: impl Fn(&str) -> Vec<u8> + Send + 'static,
) -> Result<u16, std::io::Error> {
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0))?;
    let port = listener.local_addr()?.port();
    std::thread::spawn(move || {
        while let Ok((mut stream, _)) = listener.accept() {
            let mut buffer = vec![0u8; 64 * 1024];
            let read = stream.read(&mut buffer).unwrap_or(0);
            let head = String::from_utf8_lossy(&buffer[..read]).to_string();
            let _ = stream.write_all(&respond(&head));
            let _ = stream.flush();
        }
    });
    Ok(port)
}

fn raw_http_response(content_type: &str, body: &[u8]) -> Vec<u8> {
    let mut out = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    )
    .into_bytes();
    out.extend_from_slice(body);
    out
}

const COMPLETION_BODY: &[u8] = br#"{"id":"chatcmpl-conformance","object":"chat.completion","created":1,"model":"conformance-test-model","choices":[{"index":0,"message":{"role":"assistant","content":"conformance."},"finish_reason":"stop"}],"usage":{"prompt_tokens":9,"completion_tokens":2,"total_tokens":11}}"#;

const STREAM_BODY: &[u8] = b"data: {\"id\":\"chatcmpl-conformance\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"conformance-test-model\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"conformance.\"}}]}\n\ndata: [DONE]\n\n";

/// A loopback llama-server that serves one model, one completion and one
/// stream — the smallest upstream the contract section can be exercised
/// against.
fn serving_runtime_endpoints() -> CliRuntimeEndpoints {
    runtime_endpoints_listing(&format!(
        r#"{{"object":"list","data":[{{"id":"{MODEL_ID}","object":"model"}}]}}"#
    ))
}

/// The same runtime with an empty catalogue, for the "nothing to exercise the
/// inference contract with" case.
fn empty_runtime_endpoints() -> CliRuntimeEndpoints {
    runtime_endpoints_listing(r#"{"object":"list","data":[]}"#)
}

fn runtime_endpoints_listing(models: &str) -> CliRuntimeEndpoints {
    let models = models.to_string();
    let llama_port = spawn_fake_runtime(move |head| {
        if head.starts_with("GET /v1/models") {
            raw_http_response("application/json", models.as_bytes())
        } else if head.starts_with("POST /v1/chat/completions") {
            if head.contains("\"stream\":true") {
                raw_http_response("text/event-stream", STREAM_BODY)
            } else {
                raw_http_response("application/json", COMPLETION_BODY)
            }
        } else {
            raw_http_response("application/json", br#"{"status":"ok"}"#)
        }
    })
    .expect("bind the ephemeral llama-server stand-in");

    // Ollama is switched off in the config; reserve a dead port so a
    // developer's real daemon can never become test input.
    let dead = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("reserve an unused port");
    let ollama_port = dead.local_addr().expect("unused address").port();
    drop(dead);

    CliRuntimeEndpoints {
        llama_port,
        ollama_base_url: format!("http://127.0.0.1:{ollama_port}"),
    }
}

struct Node {
    base: String,
    data_dir: PathBuf,
    accept_loop: tokio::task::JoinHandle<Result<(), String>>,
}

impl Node {
    async fn start(label: &str, endpoints: CliRuntimeEndpoints) -> Option<Self> {
        let port = free_loopback_port().await?;
        let data_dir = std::env::temp_dir().join(format!(
            "conformance-suite-{label}-{}-{}",
            std::process::id(),
            next_test_id()
        ));
        std::fs::create_dir(&data_dir).expect("create the private fixture app-data directory");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&data_dir, std::fs::Permissions::from_mode(0o700))
                .expect("make the fixture app-data directory private");
        }

        let config = ApiServerConfig {
            require_token: true,
            expose_ollama: false,
            expose_providers: false,
            tokens: vec![TokenEntry {
                id: "conformance-suite-token".to_string(),
                label: "conformance suite".to_string(),
                sha256: token_digest(TEST_TOKEN),
                created_at: 0,
                last_used_at: None,
                scopes: vec![Scope::Chat, Scope::Models, Scope::Embeddings],
                backends: vec![Backend::Local],
                expires_at: None,
                bound_local_app_id: None,
            }],
            ..ApiServerConfig::default()
        };
        let config_path = data_dir.join("api_server.json");
        save_config_impl(&config_path, &config).expect("write the node's config");
        let m3_hub = test_m3_hub(&data_dir.join("m3-test-hub"));

        let accept_loop = tokio::spawn(run_cli_server_with_m3_hub_and_endpoints(
            port,
            config_path,
            m3_hub,
            endpoints,
            Vec::new,
        ));

        let mut node = Node {
            base: format!("http://127.0.0.1:{port}"),
            data_dir,
            accept_loop,
        };

        let client = reqwest::Client::builder()
            .no_proxy()
            .timeout(READY_BUDGET)
            .build()
            .expect("readiness client");
        for _ in 0..200 {
            if client
                .get(format!("{}/health", node.base))
                .send()
                .await
                .is_ok()
            {
                return Some(node);
            }
            if node.accept_loop.is_finished() {
                let outcome = (&mut node.accept_loop).await;
                panic!("the node exited before readiness: {outcome:?}");
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("the node never answered /health");
    }

    fn options(&self) -> SuiteOptions {
        SuiteOptions {
            base_url: self.base.clone(),
            token: Some(TEST_TOKEN.to_string()),
            sections: Vec::new(),
            model: None,
        }
    }
}

impl Drop for Node {
    fn drop(&mut self) {
        self.accept_loop.abort();
        let _ = std::fs::remove_dir_all(&self.data_dir);
    }
}

fn section<'report>(
    report: &'report conformance::ConformanceReport,
    id: SectionId,
) -> &'report conformance::SectionReport {
    report
        .sections
        .iter()
        .find(|section| section.id == id)
        .unwrap_or_else(|| panic!("the report omits the {} section", id.code()))
}

/// Every check that did not pass, rendered for a failure message. A bare
/// "expected Compatible" tells you nothing about which of a dozen checks
/// disagreed.
fn failures(report: &conformance::ConformanceReport) -> String {
    let mut out = String::new();
    for section in &report.sections {
        for check in &section.checks {
            if check.status != CheckStatus::Passed {
                out.push_str(&format!(
                    "\n  {:?} {} — {}",
                    check.status, check.id, check.detail
                ));
            }
        }
    }
    out
}

/// The headline: a real node, a real socket, the published suite, a verdict.
#[tokio::test]
async fn a_live_node_passes_the_published_suite() {
    let Some(node) = Node::start("passes", serving_runtime_endpoints()).await else {
        return;
    };
    let client = conformance::client().expect("conformance client");
    let report = conformance::run_suite(&client, &node.options()).await;

    assert!(
        report.is_compatible(),
        "a live node did not pass its own suite:{}\n{}",
        failures(&report),
        report.to_summary()
    );
    assert_eq!(report.suite_revision, conformance::SUITE_REVISION);
    assert_eq!(
        report.node_suite_revision.as_deref(),
        Some(conformance::SUITE_REVISION)
    );

    // The required section must be complete, not merely un-failed.
    assert_eq!(
        section(&report, SectionId::Contract).status,
        SectionStatus::Passed,
        "{}",
        report.to_summary()
    );
    // K4/K5 and K12 are checked against the live listener, so they must pass
    // here rather than skip — this build has both a body cap and a ledger.
    assert_eq!(
        section(&report, SectionId::Limits).status,
        SectionStatus::Passed,
        "{}",
        report.to_summary()
    );
    assert_eq!(
        section(&report, SectionId::Ledger).status,
        SectionStatus::Passed,
        "{}",
        report.to_summary()
    );

    // K3 is the one section whose availability is a property of the *kernel*
    // this test runs on, not of the code. A host without an enforceable
    // boundary must report a named skip, never a silent pass.
    let isolation = section(&report, SectionId::Isolation);
    match isolation.status {
        SectionStatus::Passed => {
            assert!(isolation
                .checks
                .iter()
                .any(|check| check.id == "isolation.denied_surfaces"));
        }
        SectionStatus::Skipped => {
            assert!(
                isolation.skip_reason.is_some(),
                "a skipped section must say why"
            );
            assert!(report
                .skipped_optional_sections
                .contains(&"isolation".to_string()));
        }
        other => panic!(
            "unexpected isolation status {other:?}\n{}",
            report.to_summary()
        ),
    }
}

/// K21's "reports which optional sections an implementation skipped", as a
/// caller-driven skip rather than a node-driven one.
#[tokio::test]
async fn a_partial_run_still_names_every_section_it_did_not_run() {
    let Some(node) = Node::start("partial", serving_runtime_endpoints()).await else {
        return;
    };
    let client = conformance::client().expect("conformance client");
    let options = SuiteOptions {
        sections: vec![SectionId::Contract],
        ..node.options()
    };
    let report = conformance::run_suite(&client, &options).await;

    assert!(report.is_compatible(), "{}", report.to_summary());
    let mut skipped = report.skipped_optional_sections.clone();
    skipped.sort();
    assert_eq!(skipped, vec!["isolation", "ledger", "limits"]);
    for id in [SectionId::Isolation, SectionId::Limits, SectionId::Ledger] {
        let section = section(&report, id);
        assert_eq!(section.status, SectionStatus::Skipped);
        assert!(section.checks.is_empty());
        assert_eq!(
            section.skip_reason.as_deref(),
            Some("not selected by the caller")
        );
    }
}

/// A node that cannot demonstrate inference has not demonstrated the
/// contract. The failure mode this guards is a suite that reports "all
/// checks green" over a set of checks that mostly never ran.
#[tokio::test]
async fn a_node_with_no_models_cannot_claim_compatibility() {
    let Some(node) = Node::start("no-models", empty_runtime_endpoints()).await else {
        return;
    };
    let client = conformance::client().expect("conformance client");
    let report = conformance::run_suite(&client, &node.options()).await;

    assert!(!report.is_compatible(), "{}", report.to_summary());
    let contract = section(&report, SectionId::Contract);
    assert_eq!(contract.status, SectionStatus::Incomplete);
    let skipped: Vec<&str> = contract
        .checks
        .iter()
        .filter(|check| check.status == CheckStatus::Skipped)
        .map(|check| check.id.as_str())
        .collect();
    assert_eq!(
        skipped,
        vec!["contract.chat_completion", "contract.chat_stream"]
    );
    match &report.verdict {
        Verdict::NotCompatible { reasons } => assert!(
            reasons
                .iter()
                .any(|reason| reason.contains("contract.chat_completion")),
            "{reasons:?}"
        ),
        other => panic!("expected a refusal, got {other:?}"),
    }
    // Everything that *could* be checked still was: a node failing one
    // requirement must not stop reporting on the rest.
    assert!(contract
        .checks
        .iter()
        .any(|check| check.id == "contract.authentication" && check.status == CheckStatus::Passed));
}

/// A node that will not answer is a conformance result, not a runner crash.
#[tokio::test]
async fn an_unreachable_target_reports_a_refusal_rather_than_erroring() {
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("reserve a dead port");
    let port = listener.local_addr().expect("dead address").port();
    drop(listener);

    let client = conformance::client().expect("conformance client");
    let report = conformance::run_suite(
        &client,
        &SuiteOptions::new(format!("http://127.0.0.1:{port}")),
    )
    .await;

    assert!(!report.is_compatible());
    assert_eq!(report.node_suite_revision, None);
    let contract = section(&report, SectionId::Contract);
    assert_eq!(contract.status, SectionStatus::Failed);
    assert!(contract
        .checks
        .iter()
        .any(|check| check.id == "contract.attestation" && check.status == CheckStatus::Failed));
    // The optional sections cannot be graded without an attestation, and say
    // so rather than passing by default.
    for id in [SectionId::Isolation, SectionId::Limits, SectionId::Ledger] {
        assert_eq!(section(&report, id).status, SectionStatus::Skipped);
    }
}

/// The attestation carries this machine's isolation posture and the head
/// hashes of its event chain. Nothing about a conformance run needs to be
/// readable from the network, and the route table is where that is decided.
#[test]
fn the_attestation_is_unreachable_from_a_lan_exposure() {
    for family in [
        AuthFamily::LegacyToken,
        AuthFamily::PairedLanToken,
        AuthFamily::Internal,
    ] {
        let decision = classify_request(
            &hyper::Method::GET,
            conformance::ATTESTATION_PATH,
            ClassificationInput::new(ListenerExposure::Lan, family),
        );
        // A typed loopback-only denial, not a bare 404 — the registry knows
        // the route exists and knows this exposure may not have it.
        assert!(
            matches!(
                decision,
                RouteDecision::Denied(RouteDenial::LoopbackOnly(RouteId::Conformance))
            ),
            "a LAN listener classified {} as {decision:?}",
            conformance::ATTESTATION_PATH
        );
    }

    // …and is reachable on loopback, so the refusal above is the exposure
    // rule and not a typo in the path.
    assert!(matches!(
        classify_request(
            &hyper::Method::GET,
            conformance::ATTESTATION_PATH,
            ClassificationInput::new(ListenerExposure::Loopback, AuthFamily::LegacyToken),
        ),
        RouteDecision::Allowed(_)
    ));
}
