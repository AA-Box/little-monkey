//! The device plane exercised end to end, against the real store and the real
//! signed API.
//!
//! Everything here is `#[cfg(test)]`. The simulated executor below is a test
//! double for *hardware only* — it counts physical effects instead of opening a
//! camera — and it speaks the same protocol, in the same order, as the browser
//! client in `ui/app.js`: journal the command, mint an execution id, ask for a
//! start, act at most once, stage the result durably, and retry delivery until
//! the runner acknowledges it.
//!
//! The property every test here is about: **a physical effect happens at most
//! once, and its result is delivered at least once.** A photograph cannot be
//! taken twice by a reconnect, a reload or a restart; and one that was taken
//! cannot be lost because a reply went missing.

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet, HashMap};
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    use crate::daemon::remote::api::{ApiRequest, ApiResponse, RemoteApi};
    use crate::daemon::remote::protocol::{
        sha256_hex, sign_request, DeviceCapability, DeviceCommandState, DeviceConstraints,
        DeviceReadiness, DeviceSurface, OsPermission, RemoteAction, RemoteHostConfig, RemoteScopes,
        SignedRequestHeaders, REMOTE_PROTOCOL_VERSION,
    };
    use crate::daemon::remote::store::{DeviceCommandRequest, RemoteSecretStore, RemoteStore};

    /// Room for the fixture's artifacts and nothing dramatic. The pairing's own
    /// budget is what the runner enforces; this is only the number the test
    /// pairs with.
    const ARTIFACT_BUDGET: u64 = 64 * 1024;
    use crate::daemon::store::DaemonPaths;

    #[derive(Default)]
    struct FakeSecrets(Mutex<HashMap<String, Vec<u8>>>);

    impl RemoteSecretStore for FakeSecrets {
        fn get(&self, slot: &str) -> Result<Vec<u8>, String> {
            self.0
                .lock()
                .unwrap()
                .get(slot)
                .cloned()
                .ok_or_else(|| "missing secret".to_string())
        }
        fn set(&self, slot: &str, secret: &[u8]) -> Result<(), String> {
            self.0
                .lock()
                .unwrap()
                .insert(slot.to_string(), secret.to_vec());
            Ok(())
        }
        fn delete(&self, slot: &str) -> Result<(), String> {
            self.0.lock().unwrap().remove(slot);
            Ok(())
        }
    }

    struct Fixture {
        root: PathBuf,
        /// Where the runner actually writes artifacts — the daemon root under
        /// the temporary directory, which is not the temporary directory.
        daemon_root: PathBuf,
        api: RemoteApi,
        device_id: String,
        secret: Vec<u8>,
        secrets: Arc<FakeSecrets>,
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    fn fixture() -> Fixture {
        let root =
            std::env::temp_dir().join(format!("little-monkey-device-e2e-{}", uuid::Uuid::new_v4()));
        let paths = DaemonPaths::under(&root);
        paths.ensure().unwrap();
        let host = RemoteHostConfig {
            protocol_version: REMOTE_PROTOCOL_VERSION,
            runner_id: "runner-one".into(),
            listen: "127.0.0.1:1".into(),
            advertise_url: "https://runner.invalid".into(),
            certificate_path: "/tmp/cert".into(),
            private_key_path: "/tmp/key".into(),
            certificate_sha256: "a".repeat(64),
            enabled: true,
        };
        let mut store = RemoteStore::open(&paths.root).unwrap();
        let scopes = RemoteScopes {
            actions: BTreeSet::from([RemoteAction::ViewRuns]),
            run_ids: BTreeSet::from(["run-one".to_string()]),
            workspace_ids: BTreeSet::new(),
            max_artifact_bytes: ARTIFACT_BUDGET,
        };
        // Everything physical this suite exercises, granted by the operator.
        let capabilities = BTreeSet::from([
            DeviceCapability::ViewRuns,
            DeviceCapability::DeviceInfo,
            DeviceCapability::CameraCapture,
            DeviceCapability::MicrophoneCapture,
        ]);
        let secrets = Arc::new(FakeSecrets::default());
        let invite = store
            .create_invitation_with_capabilities(&scopes, &capabilities, 1_000, 3_000)
            .unwrap();
        let accepted = store
            .accept_invitation(
                &invite.pairing_id,
                &invite.token,
                "phone",
                "runner-one",
                1_100,
                secrets.as_ref(),
            )
            .unwrap();
        let secret = accepted.device_secret.as_bytes().to_vec();
        let daemon_root = paths.root.clone();
        let api = RemoteApi::injected(paths, host, store, secrets.clone());
        Fixture {
            root,
            daemon_root,
            api,
            device_id: accepted.device_id,
            secret,
            secrets,
        }
    }

    fn surface(ready: bool) -> DeviceSurface {
        let capabilities = BTreeSet::from([
            DeviceCapability::DeviceInfo,
            DeviceCapability::CameraCapture,
            DeviceCapability::MicrophoneCapture,
        ]);
        let permissions = BTreeMap::from([
            (DeviceCapability::DeviceInfo, OsPermission::NotRequired),
            (DeviceCapability::CameraCapture, OsPermission::Granted),
            (DeviceCapability::MicrophoneCapture, OsPermission::Granted),
        ]);
        let state = if ready {
            DeviceReadiness::Ready
        } else {
            DeviceReadiness::ForegroundRequired
        };
        DeviceSurface {
            protocol_version: REMOTE_PROTOCOL_VERSION,
            platform: "android".into(),
            platform_version: "15".into(),
            app_version: "1.3.0".into(),
            device_model: "Pixel".into(),
            capabilities: capabilities.clone(),
            permissions,
            readiness: capabilities
                .iter()
                .map(|capability| (*capability, state))
                .collect(),
            constraints: DeviceConstraints::default(),
            reported_at_ms: 0,
        }
    }

    /// One `dcmd-…` as this device's journal holds it. The same phases the
    /// browser client keeps, for the same reason: the phase is durable *before*
    /// the effect, and the bytes stay until the runner acknowledges them.
    #[derive(Debug, Clone, PartialEq, Eq)]
    enum Phase {
        StartAuthorized,
        ResultStaged,
        ResultAcked,
    }

    #[derive(Debug, Clone)]
    struct JournalEntry {
        execution_id: String,
        phase: Phase,
        outcome: DeviceCommandState,
        result: Option<serde_json::Value>,
        artifact: Option<Vec<u8>>,
    }

    /// The device's durable state — the journal and its request sequence.
    ///
    /// Held outside the executor on purpose: "restart the browser" is
    /// constructing a new executor over this same state, which is exactly what
    /// a reload does to a page backed by IndexedDB.
    #[derive(Default)]
    struct DurableDeviceState {
        journal: HashMap<String, JournalEntry>,
        sequence: u64,
    }

    /// A device that speaks the real protocol and counts physical effects
    /// instead of performing them.
    struct SimulatedDevice<'a> {
        api: &'a RemoteApi,
        device_id: String,
        secret: Vec<u8>,
        state: Arc<Mutex<DurableDeviceState>>,
        effects: Arc<AtomicUsize>,
        /// Delivery attempts to drop on the floor *after* the runner has
        /// processed them — the lost-response case, which is the one a device
        /// cannot distinguish from a lost request.
        swallow_acks: Arc<AtomicUsize>,
        now_ms: u64,
    }

    impl<'a> SimulatedDevice<'a> {
        fn new(
            api: &'a RemoteApi,
            device_id: &str,
            secret: &[u8],
            state: Arc<Mutex<DurableDeviceState>>,
            effects: Arc<AtomicUsize>,
        ) -> Self {
            Self {
                api,
                device_id: device_id.to_string(),
                secret: secret.to_vec(),
                state,
                effects,
                swallow_acks: Arc::new(AtomicUsize::new(0)),
                now_ms: 2_000,
            }
        }

        fn request(&self, method: &str, path: &str, body: &[u8]) -> ApiResponse {
            let sequence = {
                let mut state = self.state.lock().unwrap();
                state.sequence += 1;
                state.sequence
            };
            let mut auth = SignedRequestHeaders {
                device_id: self.device_id.clone(),
                secret_generation: 1,
                sequence,
                timestamp_ms: self.now_ms,
                nonce: format!("nonce-{sequence}-0123456789abcdef"),
                command_id: format!("cmd-{sequence}"),
                signature: String::new(),
            };
            auth.signature = sign_request(&self.secret, &auth, method, path, body);
            self.api.handle(
                ApiRequest {
                    method: method.into(),
                    path_and_query: path.into(),
                    body: body.to_vec(),
                    auth: Some(auth),
                },
                self.now_ms,
            )
        }

        fn json(
            &self,
            method: &str,
            path: &str,
            body: serde_json::Value,
        ) -> (u16, serde_json::Value) {
            let encoded = serde_json::to_vec(&body).unwrap();
            let response = self.request(method, path, &encoded);
            let value = serde_json::from_slice(&response.body).unwrap_or(serde_json::Value::Null);
            (response.status, value)
        }

        fn advertise(&self, ready: bool) {
            let (status, _) = self.json(
                "POST",
                "/v1/remote/device/surface",
                serde_json::to_value(surface(ready)).unwrap(),
            );
            assert_eq!(status, 200);
        }

        /// The browser client's loop, in the order that makes it safe:
        /// flush what is staged, reconcile what the runner still calls running,
        /// and only then take new work.
        fn tick(&self) {
            self.flush_outbox();
            self.reconcile();
            self.lease_and_execute();
        }

        fn flush_outbox(&self) {
            let staged = self
                .state
                .lock()
                .unwrap()
                .journal
                .iter()
                .filter(|(_, entry)| entry.phase == Phase::ResultStaged)
                .map(|(command_id, entry)| (command_id.clone(), entry.clone()))
                .collect::<Vec<_>>();
            for (command_id, entry) in staged {
                self.deliver(&command_id, &entry);
            }
        }

        fn deliver(&self, command_id: &str, entry: &JournalEntry) -> u16 {
            let artifact = entry.artifact.as_ref();
            let (status, _) = self.json(
                "POST",
                &format!("/v1/remote/device/commands/{command_id}/result"),
                serde_json::json!({
                    "protocol_version": REMOTE_PROTOCOL_VERSION,
                    "outcome": entry.outcome,
                    "result": entry.result,
                    "artifact_base64": artifact.map(|bytes| {
                        use base64::Engine;
                        base64::engine::general_purpose::STANDARD.encode(bytes)
                    }),
                    "artifact_media_type": artifact.map(|_| "image/jpeg"),
                    "artifact_sha256": artifact.map(|bytes| sha256_hex(bytes)),
                    "error": null,
                    "execution_id": entry.execution_id,
                }),
            );
            // A response the device never sees. The runner has already stored
            // the result; the device still owes a delivery and must retry.
            if self.swallow_acks.load(Ordering::SeqCst) > 0 {
                self.swallow_acks.fetch_sub(1, Ordering::SeqCst);
                return 0;
            }
            if status == 200 || status == 409 {
                let mut state = self.state.lock().unwrap();
                if let Some(entry) = state.journal.get_mut(command_id) {
                    entry.phase = Phase::ResultAcked;
                    // Only now. Dropping the bytes before the acknowledgement
                    // is the failure this whole ordering exists to prevent.
                    entry.artifact = None;
                }
            }
            status
        }

        fn reconcile(&self) {
            let (status, value) = self.json(
                "GET",
                "/v1/remote/device/commands/recover",
                serde_json::Value::Null,
            );
            if status != 200 {
                return;
            }
            let commands = value["commands"].as_array().cloned().unwrap_or_default();
            for command in commands {
                let command_id = command["command_id"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string();
                let entry = self.state.lock().unwrap().journal.get(&command_id).cloned();
                match entry {
                    Some(entry) if entry.phase == Phase::ResultStaged => {
                        self.deliver(&command_id, &entry);
                    }
                    Some(entry) if entry.phase == Phase::ResultAcked => {}
                    // Started, and nothing survived to say what happened. The
                    // effect is NOT repeated; the outcome is reported unknown.
                    other => {
                        let execution_id = other
                            .map(|entry| entry.execution_id)
                            .unwrap_or_else(|| "exec-unknown-0000".to_string());
                        self.json(
                            "POST",
                            &format!("/v1/remote/device/commands/{command_id}/result"),
                            serde_json::json!({
                                "protocol_version": REMOTE_PROTOCOL_VERSION,
                                "outcome": "failed",
                                "result": null,
                                "artifact_base64": null,
                                "artifact_media_type": null,
                                "artifact_sha256": null,
                                "error": "execution_outcome_unknown_after_restart: the action may \
                                          have happened and was not repeated",
                                "execution_id": execution_id,
                            }),
                        );
                    }
                }
            }
        }

        fn lease_and_execute(&self) -> Option<String> {
            let response = self.request("GET", "/v1/remote/device/commands/next", b"");
            if response.status != 200 {
                return None;
            }
            let command: serde_json::Value = serde_json::from_slice(&response.body).unwrap();
            let command_id = command["command_id"].as_str().unwrap().to_string();
            let capability = command["capability"].as_str().unwrap().to_string();
            self.start_and_perform(&command_id, &capability);
            Some(command_id)
        }

        /// Journal, authorize, act once, stage, deliver — in that order.
        fn start_and_perform(&self, command_id: &str, capability: &str) {
            let execution_id = format!("exec-{command_id}");
            let (status, started) = self.json(
                "POST",
                &format!("/v1/remote/device/commands/{command_id}/start"),
                serde_json::json!({ "execution_id": execution_id }),
            );
            if status != 200 || started["started"] != serde_json::json!(true) {
                return;
            }
            // Durable BEFORE the effect. A crash on the next line is recoverable
            // as "unknown", never as "do it again".
            self.state.lock().unwrap().journal.insert(
                command_id.to_string(),
                JournalEntry {
                    execution_id: execution_id.clone(),
                    phase: Phase::StartAuthorized,
                    outcome: DeviceCommandState::Succeeded,
                    result: None,
                    artifact: None,
                },
            );
            // The physical effect. Counted, never performed.
            self.effects.fetch_add(1, Ordering::SeqCst);
            let artifact = (capability == "camera_capture").then(|| b"jpeg-bytes".to_vec());
            let entry = JournalEntry {
                execution_id,
                phase: Phase::ResultStaged,
                outcome: DeviceCommandState::Succeeded,
                result: Some(serde_json::json!({ "width": 4, "height": 3 })),
                artifact,
            };
            self.state
                .lock()
                .unwrap()
                .journal
                .insert(command_id.to_string(), entry.clone());
            self.deliver(command_id, &entry);
        }
    }

    fn queue(fixture: &Fixture, capability: DeviceCapability, invocation: Option<&str>) -> String {
        fixture
            .api
            .store_for_tests()
            .lock()
            .unwrap()
            .enqueue_device_command(
                &DeviceCommandRequest {
                    device_id: fixture.device_id.clone(),
                    capability,
                    arguments: serde_json::json!({ "position": "back" }),
                    source_run_id: Some("run-one".into()),
                    source_session_id: None,
                    source_tool_call_id: Some("tool-1-1".into()),
                    invocation_id: invocation.map(str::to_string),
                    expires_at_ms: 900_000,
                },
                2_000,
            )
            .unwrap()
            .command_id
    }

    fn command_state(fixture: &Fixture, command_id: &str) -> DeviceCommandState {
        fixture
            .api
            .store_for_tests()
            .lock()
            .unwrap()
            .device_command(command_id)
            .unwrap()
            .unwrap()
            .state
    }

    /// **The scenario this whole design exists for.**
    ///
    /// One camera command, one capture, a lost result response, a restarted
    /// device-side executor, reconciliation, and the same artifact delivered —
    /// with the physical effect still having happened exactly once.
    #[test]
    fn a_lost_result_response_is_redelivered_without_a_second_physical_effect() {
        let fixture = fixture();
        let durable = Arc::new(Mutex::new(DurableDeviceState::default()));
        let effects = Arc::new(AtomicUsize::new(0));
        let device = SimulatedDevice::new(
            &fixture.api,
            &fixture.device_id,
            &fixture.secret,
            durable.clone(),
            effects.clone(),
        );
        device.advertise(true);
        let command_id = queue(&fixture, DeviceCapability::CameraCapture, None);

        // The runner's answer to the first delivery never reaches the device.
        device.swallow_acks.store(1, Ordering::SeqCst);
        device.tick();
        assert_eq!(effects.load(Ordering::SeqCst), 1, "one capture");
        assert_eq!(
            durable.lock().unwrap().journal[&command_id].phase,
            Phase::ResultStaged,
            "an unacknowledged result stays staged, bytes and all"
        );
        assert!(
            durable.lock().unwrap().journal[&command_id]
                .artifact
                .is_some(),
            "the artifact must survive until the runner acknowledges it"
        );

        // The browser is reloaded: a new executor over the same durable state.
        drop(device);
        let reconnected = SimulatedDevice::new(
            &fixture.api,
            &fixture.device_id,
            &fixture.secret,
            durable.clone(),
            effects.clone(),
        );
        reconnected.tick();

        assert_eq!(
            effects.load(Ordering::SeqCst),
            1,
            "reconnecting must never take a second photograph"
        );
        assert_eq!(
            command_state(&fixture, &command_id),
            DeviceCommandState::Succeeded
        );
        let stored = fixture
            .api
            .store_for_tests()
            .lock()
            .unwrap()
            .device_command(&command_id)
            .unwrap()
            .unwrap();
        let artifact = stored.artifact.expect("the artifact reached the runner");
        assert_eq!(artifact.sha256, sha256_hex(b"jpeg-bytes"));
        assert_eq!(artifact.bytes, 10);
        // The bytes on disk are the bytes the device sent, byte for byte.
        let path = fixture
            .daemon_root
            .join("device-artifacts")
            .join(&command_id);
        assert_eq!(std::fs::read(&path).unwrap(), b"jpeg-bytes");
        assert_eq!(
            durable.lock().unwrap().journal[&command_id].phase,
            Phase::ResultAcked
        );
        assert!(
            durable.lock().unwrap().journal[&command_id]
                .artifact
                .is_none(),
            "the bytes may be dropped only after the acknowledgement"
        );
    }

    /// Restart while the command is still queued: it is executed once, later.
    #[test]
    fn a_restart_while_queued_leaves_the_command_executable_exactly_once() {
        let fixture = fixture();
        let durable = Arc::new(Mutex::new(DurableDeviceState::default()));
        let effects = Arc::new(AtomicUsize::new(0));
        let first = SimulatedDevice::new(
            &fixture.api,
            &fixture.device_id,
            &fixture.secret,
            durable.clone(),
            effects.clone(),
        );
        first.advertise(true);
        let command_id = queue(&fixture, DeviceCapability::CameraCapture, None);
        // The device never gets as far as leasing it.
        drop(first);
        assert_eq!(effects.load(Ordering::SeqCst), 0);
        assert_eq!(
            command_state(&fixture, &command_id),
            DeviceCommandState::Queued
        );

        let second = SimulatedDevice::new(
            &fixture.api,
            &fixture.device_id,
            &fixture.secret,
            durable,
            effects.clone(),
        );
        second.tick();
        second.tick();
        assert_eq!(effects.load(Ordering::SeqCst), 1);
        assert_eq!(
            command_state(&fixture, &command_id),
            DeviceCommandState::Succeeded
        );
    }

    /// Restart in the uncertainty window: after the runner authorized a start
    /// and before any result was staged. The effect may have happened; it must
    /// not be repeated, and the outcome must be reported as unproven.
    #[test]
    fn a_restart_after_start_reports_an_unknown_outcome_and_never_repeats_the_effect() {
        let fixture = fixture();
        let durable = Arc::new(Mutex::new(DurableDeviceState::default()));
        let effects = Arc::new(AtomicUsize::new(0));
        let device = SimulatedDevice::new(
            &fixture.api,
            &fixture.device_id,
            &fixture.secret,
            durable.clone(),
            effects.clone(),
        );
        device.advertise(true);
        let command_id = queue(&fixture, DeviceCapability::CameraCapture, None);

        // Lease and authorize, then die before anything is staged.
        let response = device.request("GET", "/v1/remote/device/commands/next", b"");
        assert_eq!(response.status, 200);
        let (status, started) = device.json(
            "POST",
            &format!("/v1/remote/device/commands/{command_id}/start"),
            serde_json::json!({ "execution_id": format!("exec-{command_id}") }),
        );
        assert_eq!(status, 200);
        assert_eq!(started["started"], serde_json::json!(true));
        durable.lock().unwrap().journal.insert(
            command_id.clone(),
            JournalEntry {
                execution_id: format!("exec-{command_id}"),
                phase: Phase::StartAuthorized,
                outcome: DeviceCommandState::Succeeded,
                result: None,
                artifact: None,
            },
        );
        drop(device);

        let reconnected = SimulatedDevice::new(
            &fixture.api,
            &fixture.device_id,
            &fixture.secret,
            durable,
            effects.clone(),
        );
        reconnected.tick();
        assert_eq!(
            effects.load(Ordering::SeqCst),
            0,
            "a command that crossed the start line is never performed on recovery"
        );
        let stored = fixture
            .api
            .store_for_tests()
            .lock()
            .unwrap()
            .device_command(&command_id)
            .unwrap()
            .unwrap();
        assert_eq!(stored.state, DeviceCommandState::Failed);
        assert!(
            stored
                .error
                .unwrap()
                .contains("execution_outcome_unknown_after_restart"),
            "the outcome must be reported as unproven, not as a failure that definitely did nothing"
        );
    }

    /// A different device offering a different execution for a running command
    /// is refused rather than authorized.
    #[test]
    fn a_second_execution_of_a_running_command_is_never_authorized() {
        let fixture = fixture();
        let durable = Arc::new(Mutex::new(DurableDeviceState::default()));
        let effects = Arc::new(AtomicUsize::new(0));
        let device = SimulatedDevice::new(
            &fixture.api,
            &fixture.device_id,
            &fixture.secret,
            durable,
            effects,
        );
        device.advertise(true);
        let command_id = queue(&fixture, DeviceCapability::CameraCapture, None);
        assert_eq!(
            device
                .request("GET", "/v1/remote/device/commands/next", b"")
                .status,
            200
        );
        let path = format!("/v1/remote/device/commands/{command_id}/start");
        let (status, _) = device.json(
            "POST",
            &path,
            serde_json::json!({ "execution_id": "exec-one-000" }),
        );
        assert_eq!(status, 200);
        // The same execution reconnecting: told not to act, told it may recover.
        let (status, again) = device.json(
            "POST",
            &path,
            serde_json::json!({ "execution_id": "exec-one-000" }),
        );
        assert_eq!(status, 200);
        assert_eq!(again["started"], serde_json::json!(false));
        assert_eq!(again["recoverable"], serde_json::json!(true));
        // A different execution: refused outright.
        let (status, _) = device.json(
            "POST",
            &path,
            serde_json::json!({ "execution_id": "exec-two-000" }),
        );
        assert_eq!(
            status, 409,
            "authorizing a second execution is how two photographs get taken"
        );
    }

    /// The same result twice is a retry. A different result is a contradiction.
    #[test]
    fn a_conflicting_terminal_replay_is_refused_and_the_first_result_stays_authoritative() {
        let fixture = fixture();
        let durable = Arc::new(Mutex::new(DurableDeviceState::default()));
        let effects = Arc::new(AtomicUsize::new(0));
        let device = SimulatedDevice::new(
            &fixture.api,
            &fixture.device_id,
            &fixture.secret,
            durable.clone(),
            effects,
        );
        device.advertise(true);
        let command_id = queue(&fixture, DeviceCapability::CameraCapture, None);
        device.tick();
        assert_eq!(
            command_state(&fixture, &command_id),
            DeviceCommandState::Succeeded
        );

        // The identical report again — the ordinary lost-acknowledgement case.
        let entry = JournalEntry {
            execution_id: format!("exec-{command_id}"),
            phase: Phase::ResultStaged,
            outcome: DeviceCommandState::Succeeded,
            result: Some(serde_json::json!({ "width": 4, "height": 3 })),
            artifact: Some(b"jpeg-bytes".to_vec()),
        };
        assert_eq!(
            device.deliver(&command_id, &entry),
            200,
            "a replay is accepted"
        );

        // A different artifact for the same command.
        let contradiction = JournalEntry {
            artifact: Some(b"different-bytes".to_vec()),
            ..entry
        };
        assert_eq!(
            device.deliver(&command_id, &contradiction),
            409,
            "a different answer must not replace an authoritative one"
        );
        let stored = fixture
            .api
            .store_for_tests()
            .lock()
            .unwrap()
            .device_command(&command_id)
            .unwrap()
            .unwrap();
        assert_eq!(stored.artifact.unwrap().sha256, sha256_hex(b"jpeg-bytes"));
        assert_eq!(
            std::fs::read(
                fixture
                    .daemon_root
                    .join("device-artifacts")
                    .join(&command_id)
            )
            .unwrap(),
            b"jpeg-bytes",
            "the rejected bytes must never have reached the artifact file"
        );
    }

    /// One durable tool invocation, delivered twice, is one command and one
    /// effect.
    #[test]
    fn a_redelivered_tool_invocation_produces_one_command_and_one_effect() {
        let fixture = fixture();
        let durable = Arc::new(Mutex::new(DurableDeviceState::default()));
        let effects = Arc::new(AtomicUsize::new(0));
        let device = SimulatedDevice::new(
            &fixture.api,
            &fixture.device_id,
            &fixture.secret,
            durable,
            effects.clone(),
        );
        device.advertise(true);
        let first = queue(
            &fixture,
            DeviceCapability::CameraCapture,
            Some("job-7:tool-1-1"),
        );
        let second = queue(
            &fixture,
            DeviceCapability::CameraCapture,
            Some("job-7:tool-1-1"),
        );
        assert_eq!(first, second);
        device.tick();
        device.tick();
        assert_eq!(effects.load(Ordering::SeqCst), 1);
    }

    /// Readiness withdrawn between queueing and leasing stops the command with
    /// a reason an operator can act on, and no effect happens.
    #[test]
    fn readiness_lost_before_the_lease_stops_the_command_without_an_effect() {
        let fixture = fixture();
        let durable = Arc::new(Mutex::new(DurableDeviceState::default()));
        let effects = Arc::new(AtomicUsize::new(0));
        let device = SimulatedDevice::new(
            &fixture.api,
            &fixture.device_id,
            &fixture.secret,
            durable,
            effects.clone(),
        );
        device.advertise(true);
        let command_id = queue(&fixture, DeviceCapability::CameraCapture, None);
        // The page goes to the background and re-advertises honestly.
        device.advertise(false);
        device.tick();
        assert_eq!(effects.load(Ordering::SeqCst), 0);
        let stored = fixture
            .api
            .store_for_tests()
            .lock()
            .unwrap()
            .device_command(&command_id)
            .unwrap()
            .unwrap();
        assert_eq!(stored.state, DeviceCommandState::Failed);
        assert!(stored.error.unwrap().contains("foreground"));
    }

    /// A grant withdrawn between the lease and the start stops the effect at
    /// the last boundary before hardware.
    #[test]
    fn a_grant_withdrawn_between_lease_and_start_stops_the_effect() {
        let fixture = fixture();
        let durable = Arc::new(Mutex::new(DurableDeviceState::default()));
        let effects = Arc::new(AtomicUsize::new(0));
        let device = SimulatedDevice::new(
            &fixture.api,
            &fixture.device_id,
            &fixture.secret,
            durable,
            effects.clone(),
        );
        device.advertise(true);
        let command_id = queue(&fixture, DeviceCapability::CameraCapture, None);
        assert_eq!(
            device
                .request("GET", "/v1/remote/device/commands/next", b"")
                .status,
            200
        );
        // The operator withdraws the camera grant while the lease is held.
        fixture
            .api
            .store_for_tests()
            .lock()
            .unwrap()
            .set_device_capabilities(
                &fixture.device_id,
                &BTreeSet::from([DeviceCapability::ViewRuns, DeviceCapability::DeviceInfo]),
                2_500,
            )
            .unwrap();
        let (status, _) = device.json(
            "POST",
            &format!("/v1/remote/device/commands/{command_id}/start"),
            serde_json::json!({ "execution_id": "exec-late-0000" }),
        );
        assert_ne!(status, 200, "a withdrawn grant must not authorize a start");
        assert_eq!(effects.load(Ordering::SeqCst), 0);
        assert!(
            command_state(&fixture, &command_id).terminal(),
            "withdrawing the grant resolves the command rather than leaving it leased"
        );
    }

    /// Readiness lost between the lease and the start — the last boundary
    /// before hardware, and the one a lease-time check cannot cover.
    #[test]
    fn readiness_lost_between_lease_and_start_stops_the_effect_at_the_last_boundary() {
        let fixture = fixture();
        let durable = Arc::new(Mutex::new(DurableDeviceState::default()));
        let effects = Arc::new(AtomicUsize::new(0));
        let device = SimulatedDevice::new(
            &fixture.api,
            &fixture.device_id,
            &fixture.secret,
            durable,
            effects.clone(),
        );
        device.advertise(true);
        let command_id = queue(&fixture, DeviceCapability::CameraCapture, None);
        assert_eq!(
            device
                .request("GET", "/v1/remote/device/commands/next", b"")
                .status,
            200
        );
        // The page goes to the background while the lease is held.
        device.advertise(false);
        let (status, _) = device.json(
            "POST",
            &format!("/v1/remote/device/commands/{command_id}/start"),
            serde_json::json!({ "execution_id": "exec-bg-000000" }),
        );
        assert_eq!(status, 403, "authority is re-checked at the start boundary");
        assert_eq!(effects.load(Ordering::SeqCst), 0);
        assert_eq!(
            command_state(&fixture, &command_id),
            DeviceCommandState::Failed
        );
    }

    /// Readiness lost *after* the start is not a reason to strand the effect.
    ///
    /// The mirror of the test above, and the boundary between them is the whole
    /// point: the start check exists to stop a *new* physical effect, so it
    /// belongs to `leased` → `running` and to nothing else. Once an execution
    /// holds the command the camera may already have fired, and the device's own
    /// retry — because the reply was lost and the page then went to the
    /// background — has to be answered as the recovery it is. Failing it there
    /// would turn a glance at another app into a revocation of work already
    /// authorized, and leave a staged result with nowhere to go.
    #[test]
    fn readiness_lost_after_the_start_still_lets_the_same_execution_recover() {
        let fixture = fixture();
        let durable = Arc::new(Mutex::new(DurableDeviceState::default()));
        let effects = Arc::new(AtomicUsize::new(0));
        let device = SimulatedDevice::new(
            &fixture.api,
            &fixture.device_id,
            &fixture.secret,
            durable,
            effects.clone(),
        );
        device.advertise(true);
        let command_id = queue(&fixture, DeviceCapability::CameraCapture, None);
        assert_eq!(
            device
                .request("GET", "/v1/remote/device/commands/next", b"")
                .status,
            200
        );
        let start_path = format!("/v1/remote/device/commands/{command_id}/start");
        let (status, started) = device.json(
            "POST",
            &start_path,
            serde_json::json!({ "execution_id": "exec-recover-01" }),
        );
        assert_eq!(status, 200);
        assert_eq!(started["started"], serde_json::json!(true));

        // The reply was lost, and by the time the device asks again the page has
        // gone to the background: every readiness axis now says no.
        device.advertise(false);
        let (status, again) = device.json(
            "POST",
            &start_path,
            serde_json::json!({ "execution_id": "exec-recover-01" }),
        );
        assert_eq!(
            status, 200,
            "the same execution's retry is a recovery: {again}"
        );
        assert_eq!(
            again["started"],
            serde_json::json!(false),
            "a recovery must never authorize a second physical effect"
        );
        assert_eq!(again["recoverable"], serde_json::json!(true));
        assert_eq!(
            command_state(&fixture, &command_id),
            DeviceCommandState::Running,
            "a readiness check at a boundary it already passed must not end the command"
        );

        // A *different* execution asking for the same command is still refused,
        // background or not.
        let (status, _) = device.json(
            "POST",
            &start_path,
            serde_json::json!({ "execution_id": "exec-intruder-1" }),
        );
        assert_eq!(status, 409);
        assert_eq!(
            command_state(&fixture, &command_id),
            DeviceCommandState::Running
        );

        // And the result it staged before the interruption is still deliverable.
        let (status, _) = device.json(
            "POST",
            &format!("/v1/remote/device/commands/{command_id}/result"),
            serde_json::json!({
                "protocol_version": REMOTE_PROTOCOL_VERSION,
                "outcome": "succeeded",
                "result": { "width": 4, "height": 3 },
                "artifact_base64": null,
                "artifact_media_type": null,
                "artifact_sha256": null,
                "error": null,
                "execution_id": "exec-recover-01",
            }),
        );
        assert_eq!(status, 200);
        assert_eq!(
            command_state(&fixture, &command_id),
            DeviceCommandState::Succeeded
        );
    }

    /// The control channel reports lost *authority*, not lost readiness.
    ///
    /// A page that goes to the background loses readiness for a moment. Telling
    /// a recording already in progress that it was revoked would cut it short
    /// over a glance at another app — while an operator actually withdrawing the
    /// grant must reach it at once.
    #[test]
    fn the_control_channel_separates_lost_authority_from_lost_readiness() {
        let fixture = fixture();
        let durable = Arc::new(Mutex::new(DurableDeviceState::default()));
        let effects = Arc::new(AtomicUsize::new(0));
        let device = SimulatedDevice::new(
            &fixture.api,
            &fixture.device_id,
            &fixture.secret,
            durable,
            effects,
        );
        device.advertise(true);
        let command_id = queue(&fixture, DeviceCapability::MicrophoneCapture, None);
        assert_eq!(
            device
                .request("GET", "/v1/remote/device/commands/next", b"")
                .status,
            200
        );
        device.json(
            "POST",
            &format!("/v1/remote/device/commands/{command_id}/start"),
            serde_json::json!({ "execution_id": "exec-mic-00001" }),
        );
        let control = |device: &SimulatedDevice<'_>| {
            device
                .json(
                    "GET",
                    &format!("/v1/remote/device/commands/{command_id}/control"),
                    serde_json::Value::Null,
                )
                .1
        };
        // Backgrounded mid-recording: readiness is gone, authority is not.
        device.advertise(false);
        assert_eq!(control(&device)["revoked"], serde_json::json!(false));

        // The operator withdrawing the grant is a different thing entirely.
        fixture
            .api
            .store_for_tests()
            .lock()
            .unwrap()
            .set_device_capabilities(
                &fixture.device_id,
                &BTreeSet::from([DeviceCapability::ViewRuns]),
                2_600,
            )
            .unwrap();
        let answer = control(&device);
        // Withdrawing the grant also resolves the command, so the device learns
        // both facts from the one request it was already making.
        assert!(
            answer["revoked"] == serde_json::json!(true)
                || answer["cancel_requested"] == serde_json::json!(true)
                || answer["state"] == serde_json::json!("cancelled"),
            "a withdrawn grant must reach a running command: {answer}"
        );
    }

    /// A revoked device can neither take new work nor reconcile old work.
    #[test]
    fn a_revoked_device_can_neither_lease_nor_recover() {
        let fixture = fixture();
        let durable = Arc::new(Mutex::new(DurableDeviceState::default()));
        let effects = Arc::new(AtomicUsize::new(0));
        let device = SimulatedDevice::new(
            &fixture.api,
            &fixture.device_id,
            &fixture.secret,
            durable,
            effects.clone(),
        );
        device.advertise(true);
        queue(&fixture, DeviceCapability::CameraCapture, None);
        fixture
            .api
            .store_for_tests()
            .lock()
            .unwrap()
            .revoke_device(
                &fixture.device_id,
                "revoked by the operator",
                2_500,
                fixture.secrets.as_ref(),
                None,
            )
            .unwrap();
        assert_eq!(
            device
                .request("GET", "/v1/remote/device/commands/next", b"")
                .status,
            401
        );
        assert_eq!(
            device
                .request("GET", "/v1/remote/device/commands/recover", b"")
                .status,
            401
        );
        assert_eq!(effects.load(Ordering::SeqCst), 0);
    }

    /// A running command's cancellation reaches the device through the control
    /// channel, and the honest terminal result says the effect was not undone.
    #[test]
    fn cancelling_a_running_command_reaches_the_device_and_is_reported_truthfully() {
        let fixture = fixture();
        let durable = Arc::new(Mutex::new(DurableDeviceState::default()));
        let effects = Arc::new(AtomicUsize::new(0));
        let device = SimulatedDevice::new(
            &fixture.api,
            &fixture.device_id,
            &fixture.secret,
            durable,
            effects.clone(),
        );
        device.advertise(true);
        let command_id = queue(&fixture, DeviceCapability::MicrophoneCapture, None);
        assert_eq!(
            device
                .request("GET", "/v1/remote/device/commands/next", b"")
                .status,
            200
        );
        device.json(
            "POST",
            &format!("/v1/remote/device/commands/{command_id}/start"),
            serde_json::json!({ "execution_id": "exec-mic-00000" }),
        );
        // Nothing is asked for yet, and the watcher says so.
        let (status, control) = device.json(
            "GET",
            &format!("/v1/remote/device/commands/{command_id}/control"),
            serde_json::Value::Null,
        );
        assert_eq!(status, 200);
        assert_eq!(control["cancel_requested"], serde_json::json!(false));
        assert_eq!(control["state"], serde_json::json!("running"));

        fixture
            .api
            .store_for_tests()
            .lock()
            .unwrap()
            .request_device_cancel(&command_id, 2_600)
            .unwrap();
        let (_, control) = device.json(
            "GET",
            &format!("/v1/remote/device/commands/{command_id}/control"),
            serde_json::Value::Null,
        );
        assert_eq!(
            control["cancel_requested"],
            serde_json::json!(true),
            "a running device action has to be able to observe its own cancellation"
        );
        // The device stops recording and reports what actually happened: the
        // microphone did open, and it was cut short.
        device.json(
            "POST",
            &format!("/v1/remote/device/commands/{command_id}/result"),
            serde_json::json!({
                "protocol_version": REMOTE_PROTOCOL_VERSION,
                "outcome": "cancelled",
                "result": { "cancellation": "cancelled_during_effect", "duration_ms": 600 },
                "artifact_base64": null,
                "artifact_media_type": null,
                "artifact_sha256": null,
                "error": "Stopped part-way through; what had already happened was not undone",
                "execution_id": "exec-mic-00000",
            }),
        );
        let stored = fixture
            .api
            .store_for_tests()
            .lock()
            .unwrap()
            .device_command(&command_id)
            .unwrap()
            .unwrap();
        assert_eq!(stored.state, DeviceCommandState::Cancelled);
        assert_eq!(
            stored.result.unwrap()["cancellation"],
            serde_json::json!("cancelled_during_effect"),
            "the three cancellation outcomes must stay distinguishable"
        );
    }
}
