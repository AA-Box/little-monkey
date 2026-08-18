//! The peer plane over a real TLS socket.
//!
//! Everything in `peer_e2e.rs` hands requests to [`RemoteApi::handle`]
//! directly, which is the right trade for a test that runs on every commit:
//! it proves what a peer may cause without binding a port. What it cannot
//! prove is the layer underneath — that the certificate pin, the TLS
//! handshake, the HTTP framing and the body limit all agree with what the API
//! above them expects.
//!
//! This file closes that gap on one machine, over loopback, with a real
//! self-signed certificate the test mints itself.
//!
//! # Opt in
//!
//! Ignored by default, and deliberately so. It binds a listener, and a test
//! that binds a listener is a test that can time out its neighbours when a
//! dozen test binaries run at once — that has cost this repository a red
//! Windows leg before. It also shells out to `openssl`, which is not something
//! every runner has.
//!
//! ```text
//! cargo test --manifest-path src-tauri/Cargo.toml --bin monkey-cli -- --ignored peer_live
//! ```
//!
//! # What is still not covered
//!
//! The *client* half. `remote::client::call` reads its signing secret from the
//! OS keychain with no seam for anything else, so driving it here would write
//! into the developer's real keychain — an unacceptable side effect for a test,
//! and a refactor of production code that exists only to serve one. The
//! requests below are therefore signed in the test with the same
//! `sign_request` the client uses, and sent with a `reqwest` client pinned to
//! the same certificate the same way. What is left after that — two separate
//! machines, a LAN, a firewall, and a human comparing fingerprints out of band
//! — is in `docs/peer-live-validation.md`.

#![cfg(test)]

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};

use little_monkey_lib::peers::{PeerArtifactRef, PeerEnvelope, PeerMessageKind};

use super::remote::api::RemoteApi;
use super::remote::protocol::{
    sha256_hex, sign_request, DeviceCapability, PeerArtifactStored, PeerArtifactUpload,
    PeerHelloRequest, PeerHelloResponse, RemoteHostConfig, RemoteScopes, SignedRequestHeaders,
    REMOTE_PROTOCOL_VERSION,
};
use super::remote::store::{RemoteSecretStore, RemoteStore};
use super::store::{DaemonConfig, DaemonPaths};

/// Real wall-clock time, because the server this talks to uses its own — the
/// signed-request skew window and the envelope's expiry are both judged against
/// the clock on the receiving side, not against a constant a test chose.
fn now_ms() -> u64 {
    u64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("a clock after 1970")
            .as_millis(),
    )
    .expect("a timestamp that fits")
}

/// Big enough that the transfer is a real streamed body rather than a header,
/// small enough not to spend four seconds hashing on a laptop. The 32 MiB
/// ceiling itself is asserted arithmetically in `peer_e2e.rs`.
const ARTIFACT_BYTES: usize = 4 * 1024 * 1024;

#[derive(Default)]
struct MemorySecrets(Mutex<HashMap<String, Vec<u8>>>);

impl RemoteSecretStore for MemorySecrets {
    fn set(&self, slot: &str, secret: &[u8]) -> Result<(), String> {
        self.0
            .lock()
            .unwrap()
            .insert(slot.to_string(), secret.to_vec());
        Ok(())
    }

    fn get(&self, slot: &str) -> Result<Vec<u8>, String> {
        self.0
            .lock()
            .unwrap()
            .get(slot)
            .cloned()
            .ok_or_else(|| format!("no secret in slot {slot}"))
    }

    fn delete(&self, slot: &str) -> Result<(), String> {
        self.0.lock().unwrap().remove(slot);
        Ok(())
    }
}

#[derive(Default)]
struct FakeRuns {
    submitted: Mutex<Vec<little_monkey_lib::channels::ingress::ConversationIngress>>,
}

impl super::channel_worker::RunQueue for FakeRuns {
    fn freeze_execution(
        &self,
        ingress: &little_monkey_lib::channels::ingress::ConversationIngress,
    ) -> Result<little_monkey_lib::channels::ingress::FrozenExecutionContext, String> {
        Ok(super::channel_worker::test_frozen_execution(ingress))
    }

    fn submit(
        &self,
        ingress: &little_monkey_lib::channels::ingress::ConversationIngress,
        _params: Vec<String>,
    ) -> Result<String, String> {
        self.submitted.lock().unwrap().push(ingress.clone());
        Ok(ingress.deterministic_job_id())
    }
}

/// A directory that removes itself.
struct TempRoot(PathBuf);

impl Drop for TempRoot {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A self-signed certificate valid for `127.0.0.1`, minted by the `openssl`
/// CLI. Returns `None` when openssl is not on this machine, so the test says
/// why it did nothing rather than failing for an unrelated reason.
fn self_signed_certificate(directory: &Path) -> Option<(PathBuf, PathBuf)> {
    let certificate = directory.join("cert.pem");
    let key = directory.join("key.pem");
    let output = Command::new("openssl")
        .args([
            "req",
            "-x509",
            "-newkey",
            "rsa:2048",
            "-nodes",
            "-days",
            "1",
            "-subj",
            "/CN=127.0.0.1",
            "-addext",
            "subjectAltName=IP:127.0.0.1",
            // Deliberately not a CA: rustls refuses a certificate marked as
            // one when it is presented as the end entity, which is exactly
            // what a self-signed host certificate is.
            "-addext",
            "basicConstraints=critical,CA:FALSE",
            "-keyout",
        ])
        .arg(&key)
        .arg("-out")
        .arg(&certificate)
        .output()
        .ok()?;
    if !output.status.success() {
        eprintln!(
            "openssl could not mint a test certificate: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        return None;
    }
    Some((certificate, key))
}

/// One signed request, built exactly as `remote::client::call` builds it.
async fn signed(
    client: &reqwest::Client,
    base: &str,
    secret: &[u8],
    device_id: &str,
    sequence: u64,
    method: reqwest::Method,
    path: &str,
    body: Vec<u8>,
) -> reqwest::Response {
    let mut auth = SignedRequestHeaders {
        device_id: device_id.to_string(),
        secret_generation: 1,
        sequence,
        timestamp_ms: now_ms(),
        nonce: format!("nonce-{sequence}-{}", uuid::Uuid::new_v4().simple()),
        command_id: format!("cmd-{sequence}-{}", uuid::Uuid::new_v4().simple()),
        signature: String::new(),
    };
    auth.signature = sign_request(secret, &auth, method.as_str(), path, &body);
    client
        .request(method, format!("{base}{path}"))
        .header("x-little-monkey-device", &auth.device_id)
        .header(
            "x-little-monkey-key-generation",
            auth.secret_generation.to_string(),
        )
        .header("x-little-monkey-sequence", auth.sequence.to_string())
        .header(
            "x-little-monkey-timestamp-ms",
            auth.timestamp_ms.to_string(),
        )
        .header("x-little-monkey-nonce", &auth.nonce)
        .header("x-little-monkey-command", &auth.command_id)
        .header("x-little-monkey-signature", &auth.signature)
        .header("content-type", "application/json")
        .body(body)
        .send()
        .await
        .expect("the peer answered")
}

#[test]
#[ignore = "opt-in live validation: binds a real TLS socket and needs the openssl CLI"]
fn the_whole_peer_exchange_survives_a_real_tls_socket() {
    let root = TempRoot(
        std::env::temp_dir().join(format!("little-monkey-peer-live-{}", uuid::Uuid::new_v4())),
    );
    std::fs::create_dir_all(&root.0).expect("test root");
    let Some((certificate_path, key_path)) = self_signed_certificate(&root.0) else {
        eprintln!("skipped: no usable `openssl` on this machine");
        return;
    };
    let certificate_pem = std::fs::read(&certificate_path).expect("certificate");
    let certificate_sha256 =
        super::remote::protocol::certificate_fingerprint(&certificate_pem).expect("fingerprint");

    let paths = DaemonPaths::under(&root.0.join("bob"));
    paths.ensure().expect("paths");
    DaemonConfig::default().save(&paths).expect("config");

    // The real pairing flow, so the credential the requests below carry is one
    // the receiving store actually issued.
    let secrets = Arc::new(MemorySecrets::default());
    let grants = BTreeSet::from([
        DeviceCapability::PeerMessage,
        DeviceCapability::PeerTaskRequest,
        DeviceCapability::PeerArtifact,
    ]);
    let mut store = RemoteStore::open(&paths.root).expect("remote store");
    let invitation = store
        .create_invitation_with_capabilities(
            &RemoteScopes {
                actions: BTreeSet::new(),
                run_ids: BTreeSet::new(),
                workspace_ids: BTreeSet::new(),
                max_artifact_bytes: 1_024,
            },
            &grants,
            now_ms(),
            now_ms() + 600_000,
        )
        .expect("invitation");
    let pairing = store
        .accept_invitation_with_capabilities(
            &invitation.pairing_id,
            &invitation.token,
            "alice",
            "instance-bob",
            None,
            now_ms() + 1,
            secrets.as_ref(),
        )
        .expect("accept");
    let device_secret = pairing.device_secret.as_bytes().to_vec();
    drop(store);

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("runtime");

    runtime.block_on(async move {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("loopback listener");
        let port = listener.local_addr().expect("port").port();
        let host = RemoteHostConfig {
            protocol_version: REMOTE_PROTOCOL_VERSION,
            runner_id: "instance-bob".into(),
            listen: format!("127.0.0.1:{port}"),
            advertise_url: format!("https://127.0.0.1:{port}"),
            certificate_path: certificate_path.to_string_lossy().to_string(),
            private_key_path: key_path.to_string_lossy().to_string(),
            certificate_sha256: certificate_sha256.clone(),
            enabled: true,
        };
        let runs = Arc::new(FakeRuns::default());
        let api = RemoteApi::injected(
            paths.clone(),
            host.clone(),
            RemoteStore::open(&paths.root).expect("remote store"),
            secrets.clone(),
        )
        .with_peer_runs(runs.clone());
        let served = host.clone();
        tokio::spawn(async move {
            let _ = super::remote::serve_listener_for_test(listener, &served, api).await;
        });

        // Pinned exactly as the production client pins: this certificate and
        // nothing else is a trust anchor, so a substituted certificate fails
        // the handshake rather than being reported later.
        let client = reqwest::Client::builder()
            .tls_certs_only([reqwest::tls::Certificate::from_pem(&certificate_pem).expect("pem")])
            .build()
            .expect("pinned client");
        let base = format!("https://127.0.0.1:{port}");
        let device_id = pairing.device_id.clone();

        // 1. Hello, over TLS.
        let hello = PeerHelloRequest {
            protocol_version: REMOTE_PROTOCOL_VERSION,
            instance_id: "instance-alice".into(),
            advertised: grants.clone(),
            requested: grants.clone(),
        };
        let response = signed(
            &client,
            &base,
            &device_secret,
            &device_id,
            1,
            reqwest::Method::POST,
            "/v1/remote/peer/hello",
            serde_json::to_vec(&hello).unwrap(),
        )
        .await;
        assert_eq!(response.status(), 200);
        let answered: PeerHelloResponse = response.json().await.expect("hello");
        assert_eq!(answered.instance_id, "instance-bob");
        assert_eq!(answered.granted, grants);

        // 2. A multi-megabyte artifact, through the real body limit and the
        //    real base64 decode.
        let bytes: Vec<u8> = (0..ARTIFACT_BYTES)
            .map(|index| (index % 251) as u8)
            .collect();
        let digest = sha256_hex(&bytes);
        use base64::Engine as _;
        let upload = PeerArtifactUpload {
            protocol_version: REMOTE_PROTOCOL_VERSION,
            sha256: digest.clone(),
            content_base64: base64::engine::general_purpose::STANDARD.encode(&bytes),
            filename: Some("live.bin".into()),
            media_type: Some("application/octet-stream".into()),
        };
        let response = signed(
            &client,
            &base,
            &device_secret,
            &device_id,
            2,
            reqwest::Method::POST,
            "/v1/remote/peer/artifacts",
            serde_json::to_vec(&upload).unwrap(),
        )
        .await;
        assert_eq!(response.status(), 201);
        let stored: PeerArtifactStored = response.json().await.expect("stored");
        assert_eq!(stored.artifact_id, digest);
        assert_eq!(stored.size_bytes, ARTIFACT_BYTES as u64);

        // 3. A task request referencing it.
        let mut envelope = PeerEnvelope::new(
            "msg-live-1",
            "thread-live",
            PeerMessageKind::TaskRequest,
            "instance-alice",
            "look at the attached capture",
            i64::try_from(now_ms()).unwrap(),
            600_000,
        );
        envelope.correlation_id = Some("corr-live".into());
        envelope.artifacts.push(PeerArtifactRef {
            artifact_id: digest.clone(),
            sha256: digest.clone(),
            filename: None,
            media_type: None,
            size_bytes: Some(ARTIFACT_BYTES as u64),
        });
        let response = signed(
            &client,
            &base,
            &device_secret,
            &device_id,
            3,
            reqwest::Method::POST,
            "/v1/remote/peer/messages",
            serde_json::to_vec(&envelope).unwrap(),
        )
        .await;
        assert_eq!(response.status(), 202);

        // The receiver queued one turn, with the attachment resolved from the
        // admission its own upload route recorded.
        let queued = runs.submitted.lock().unwrap().clone();
        assert_eq!(queued.len(), 1);
        assert_eq!(
            queued[0].attachments[0].stored_artifact_id.as_deref(),
            Some(digest.as_str())
        );
        assert_eq!(
            queued[0].attachments[0].filename.as_deref(),
            Some("live.bin")
        );

        // 4. Reading the thread back over the same socket.
        let response = signed(
            &client,
            &base,
            &device_secret,
            &device_id,
            4,
            reqwest::Method::GET,
            "/v1/remote/peer/threads/thread-live",
            Vec::new(),
        )
        .await;
        assert_eq!(response.status(), 200);
        let thread: serde_json::Value = response.json().await.expect("thread");
        let messages = thread["messages"].as_array().cloned().unwrap_or_default();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0]["correlation_id"], "corr-live");

        // 5. An unsigned request reaches nothing, over the same socket that
        //    just answered four signed ones.
        let bare = client
            .get(format!("{base}/v1/remote/peer/threads/thread-live"))
            .send()
            .await
            .expect("answered");
        assert!(
            bare.status() == 401 || bare.status() == 403,
            "an unsigned request must not be admitted, got {}",
            bare.status()
        );
    });
}
