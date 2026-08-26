use super::*;
use crate::run_protocol::RunSpec;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::process::Output;

const MAX_REMOTE_CLI_JSON_BYTES: usize = 16 * 1024 * 1024;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RemoteNodeRunRecord {
    run_id: String,
    submitted_run_id: String,
    node_run_id: String,
    workspace: WorkspaceHandle,
    base_transfer: WorkspaceTransfer,
}

#[derive(Clone)]
pub struct RemoteNodeTarget {
    snapshot: ExecutionTargetSnapshot,
}

impl RemoteNodeTarget {
    pub fn from_snapshot(snapshot: ExecutionTargetSnapshot) -> Result<Self, TargetError> {
        if snapshot.identity.kind != ExecutionTargetKind::RemoteNode {
            return Err(TargetError::invalid("snapshot is not a remote-node target"));
        }
        if !valid_id(&snapshot.identity.stable_id) {
            return Err(TargetError::invalid("remote-node alias is invalid"));
        }
        Ok(Self { snapshot })
    }

    fn alias(&self) -> &str {
        &self.snapshot.identity.stable_id
    }

    fn cli_binary() -> PathBuf {
        crate::cli_install::bundled_cli_path().unwrap_or_else(|| {
            PathBuf::from(if cfg!(windows) {
                "monkey-cli.exe"
            } else {
                "monkey-cli"
            })
        })
    }

    fn cli_output<I, S>(&self, args: I) -> Result<Output, TargetError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<std::ffi::OsStr>,
    {
        let output = Command::new(Self::cli_binary())
            .args(args)
            .output()
            .map_err(|error| {
                TargetError::target_unreachable(format!(
                    "could not start the existing remote control CLI: {error}"
                ))
            })?;
        if output.stdout.len() > MAX_REMOTE_CLI_JSON_BYTES
            || output.stderr.len() > MAX_REMOTE_CLI_JSON_BYTES
        {
            return Err(TargetError::result_retrieval_failed(
                "remote control CLI output exceeded the bounded transport limit",
            ));
        }
        if !output.status.success() {
            let detail = String::from_utf8_lossy(&output.stderr).trim().to_string();
            return Err(classify_remote_cli_error(if detail.is_empty() {
                format!("remote control CLI exited with {}", output.status)
            } else {
                detail
            }));
        }
        Ok(output)
    }

    fn cli_json<I, S>(&self, args: I) -> Result<Value, TargetError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<std::ffi::OsStr>,
    {
        let output = self.cli_output(args)?;
        serde_json::from_slice(&output.stdout).map_err(|error| {
            TargetError::protocol_incompatible(format!(
                "remote control CLI returned invalid JSON: {error}"
            ))
        })
    }

    fn state_dir(&self) -> Result<PathBuf, TargetError> {
        let root = crate::app_paths::data_dir()
            .or_else(|| std::env::current_dir().ok().map(|path| path.join(".little-monkey")))
            .ok_or_else(|| TargetError::Io("could not resolve execution-target state".into()))?;
        let path = root
            .join("execution-remote-node")
            .join(&digest(self.alias().as_bytes())[..24]);
        fs::create_dir_all(&path)?;
        Ok(path)
    }

    fn records_path(&self) -> Result<PathBuf, TargetError> {
        Ok(self.state_dir()?.join("runs.json"))
    }

    fn load_records(&self) -> Result<BTreeMap<String, RemoteNodeRunRecord>, TargetError> {
        let path = self.records_path()?;
        if !path.exists() {
            return Ok(BTreeMap::new());
        }
        serde_json::from_slice(&fs::read(path)?)
            .map_err(|error| TargetError::Io(format!("invalid remote-node run state: {error}")))
    }

    fn save_records(
        &self,
        records: &BTreeMap<String, RemoteNodeRunRecord>,
    ) -> Result<(), TargetError> {
        let path = self.records_path()?;
        let temporary = path.with_extension("json.tmp");
        fs::write(
            &temporary,
            serde_json::to_vec_pretty(records)
                .map_err(|error| TargetError::Io(error.to_string()))?,
        )?;
        fs::rename(temporary, path)?;
        Ok(())
    }

    fn record_for(&self, handle: &TargetRunHandle) -> Result<RemoteNodeRunRecord, TargetError> {
        self.load_records()?
            .get(&handle.run_id)
            .cloned()
            .ok_or_else(|| {
                TargetError::runner_lost(format!(
                    "remote-node run '{}' is not registered locally",
                    handle.run_id
                ))
            })
    }

    fn placements(&self) -> Result<Vec<Value>, TargetError> {
        // Reuse K17's reconciliation path. It refreshes signed node state and
        // applies the existing liveness semantics; this target never invents a
        // second source of truth for placed runs.
        let _ = self.cli_output(["daemon", "remote", "placement-sync"])?;
        let value = self.cli_json(["daemon", "remote", "placements", "--json"])?;
        Ok(value
            .get("placements")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default())
    }

    fn placement_for(&self, submitted_run_id: &str) -> Result<Value, TargetError> {
        self.placements()?
            .into_iter()
            .find(|row| {
                row.get("submitted_run_id").and_then(Value::as_str) == Some(submitted_run_id)
                    && row.get("alias").and_then(Value::as_str) == Some(self.alias())
            })
            .ok_or_else(|| {
                TargetError::runner_lost(format!(
                    "K17 placement '{}' is no longer visible on '{}'",
                    submitted_run_id,
                    self.alias()
                ))
            })
    }

    fn fetch_artifact_bytes(
        &self,
        submitted_run_id: &str,
        artifact_id: &str,
    ) -> Result<Vec<u8>, TargetError> {
        if !valid_id(submitted_run_id) || !valid_id(artifact_id) {
            return Err(TargetError::invalid("remote artifact identity is invalid"));
        }
        let directory = self.state_dir()?.join("artifacts");
        fs::create_dir_all(&directory)?;
        let destination = directory.join(format!(
            "{}-{}",
            &digest(submitted_run_id.as_bytes())[..16],
            &digest(artifact_id.as_bytes())[..16]
        ));
        let output_arg = destination.to_string_lossy().into_owned();
        self.cli_output([
            "daemon",
            "remote",
            "artifact",
            self.alias(),
            submitted_run_id,
            artifact_id,
            "--output",
            output_arg.as_str(),
        ])?;
        let bytes = fs::read(&destination).map_err(|error| {
            TargetError::result_retrieval_failed(format!(
                "remote artifact was not published locally: {error}"
            ))
        })?;
        let _ = fs::remove_file(destination);
        Ok(bytes)
    }

    fn placed_result(&self, submitted_run_id: &str) -> Result<Value, TargetError> {
        self.placement_for(submitted_run_id)?
            .get("result")
            .cloned()
            .filter(|value| !value.is_null())
            .ok_or_else(|| {
                TargetError::result_retrieval_failed(
                    "remote placement has no terminal result payload",
                )
            })
    }

    fn patch_artifact_id(result: &Value) -> Option<&str> {
        result
            .get("artifacts")
            .and_then(Value::as_array)?
            .iter()
            .find(|artifact| artifact.get("kind").and_then(Value::as_str) == Some("patch"))?
            .get("artifactId")
            .and_then(Value::as_str)
    }

    fn artifact_descriptors(
        &self,
        submitted_run_id: &str,
        result: &Value,
    ) -> Result<Vec<ArtifactDescriptor>, TargetError> {
        let mut descriptors = Vec::new();
        for artifact in result
            .get("artifacts")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let Some(artifact_id) = artifact.get("artifactId").and_then(Value::as_str) else {
                continue;
            };
            if !valid_id(artifact_id) {
                return Err(TargetError::result_retrieval_failed(
                    "remote result contains an invalid artifact id",
                ));
            }
            let bytes = self.fetch_artifact_bytes(submitted_run_id, artifact_id)?;
            let kind = artifact
                .get("kind")
                .and_then(Value::as_str)
                .unwrap_or("artifact");
            descriptors.push(ArtifactDescriptor {
                artifact_id: artifact_id.to_string(),
                label: artifact
                    .get("label")
                    .and_then(Value::as_str)
                    .unwrap_or(kind)
                    .to_string(),
                media_type: if kind == "patch" {
                    "text/x-diff".to_string()
                } else {
                    "application/octet-stream".to_string()
                },
                sha256: digest(&bytes),
                size_bytes: bytes.len() as u64,
            });
        }
        Ok(descriptors)
    }

    fn verification_evidence(result: &Value) -> Vec<VerificationEvidence> {
        result
            .get("evidence")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .map(|evidence| VerificationEvidence {
                label: evidence
                    .get("label")
                    .and_then(Value::as_str)
                    .unwrap_or("remote verification")
                    .to_string(),
                command: evidence
                    .get("command")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                passed: evidence
                    .get("status")
                    .and_then(Value::as_str)
                    .is_none_or(|status| matches!(status, "passed" | "succeeded" | "success")),
                detail: evidence
                    .get("detail")
                    .or_else(|| evidence.get("summary"))
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
            })
            .collect()
    }

    fn write_spec(&self, spec: &RunSpec) -> Result<PathBuf, TargetError> {
        spec.validate()
            .map_err(|error| TargetError::invalid(error.to_string()))?;
        let directory = self.state_dir()?.join("specs");
        fs::create_dir_all(&directory)?;
        let path = directory.join(format!("{}.json", &digest(spec.run_id.as_bytes())[..32]));
        let temporary = path.with_extension("json.tmp");
        fs::write(
            &temporary,
            serde_json::to_vec_pretty(spec).map_err(|error| TargetError::invalid(error.to_string()))?,
        )?;
        fs::rename(temporary, &path)?;
        Ok(path)
    }
}

impl ExecutionTarget for RemoteNodeTarget {
    fn probe(&self) -> Result<ExecutionTargetSnapshot, TargetError> {
        self.cli_output(["daemon", "remote", "node-refresh", self.alias()])?;
        let value = self.cli_json(["daemon", "remote", "node-list", "--json"])?;
        let node = value
            .get("nodes")
            .and_then(Value::as_array)
            .and_then(|nodes| {
                nodes
                    .iter()
                    .find(|node| node.get("alias").and_then(Value::as_str) == Some(self.alias()))
            })
            .ok_or_else(|| {
                TargetError::target_unreachable(format!(
                    "paired remote node '{}' disappeared from the K17 registry",
                    self.alias()
                ))
            })?;
        if node.get("liveness").and_then(Value::as_str) != Some("alive") {
            return Err(TargetError::target_unreachable(format!(
                "paired remote node '{}' is not alive",
                self.alias()
            )));
        }
        if node.get("accepting").and_then(Value::as_bool) == Some(false) {
            return Err(TargetError::capability_unavailable(format!(
                "paired remote node '{}' is not accepting work",
                self.alias()
            )));
        }
        let runner_id = node
            .get("runner_id")
            .and_then(Value::as_str)
            .ok_or_else(|| TargetError::protocol_incompatible("node list omitted runner identity"))?;
        if let Some(expected) = self.snapshot.identity.verified_identity.as_deref() {
            if !expected.is_empty() && expected != runner_id {
                return Err(TargetError::TargetIdentityChanged(format!(
                    "paired alias '{}' now answers as runner '{}' instead of '{}'",
                    self.alias(),
                    runner_id,
                    expected
                )));
            }
        }
        let mut identity = self.snapshot.identity.clone();
        identity.display_name = node
            .get("node_name")
            .and_then(Value::as_str)
            .filter(|value| !value.trim().is_empty())
            .unwrap_or(&identity.display_name)
            .to_string();
        identity.verified_identity = Some(runner_id.to_string());
        identity.platform = if identity.platform.trim().is_empty() {
            "remote-node".to_string()
        } else {
            identity.platform
        };
        identity.runner_version = if identity.runner_version.trim().is_empty() {
            "k17".to_string()
        } else {
            identity.runner_version
        };
        // These are capabilities of the existing K17 node protocol itself.
        // Hardware/model-specific capabilities already frozen in the configured
        // target are preserved rather than guessed from a thin liveness row.
        identity.capabilities.durable_background_execution = true;
        identity.capabilities.shell = true;
        identity.capabilities.git = true;
        identity.capabilities.disposable_workspace = true;
        identity.capabilities.persistent_workspace = true;
        identity.capabilities.suspend = true;
        identity.capabilities.migration = true;
        identity.trust_state = TargetTrustState::Verified;
        ExecutionTargetSnapshot::freeze(identity, now_ms())
    }

    fn capabilities(&self) -> Result<TargetCapabilities, TargetError> {
        Ok(self.probe()?.identity.capabilities)
    }

    fn prepare_workspace(
        &self,
        transfer: &WorkspaceTransfer,
        policy: WorkspacePolicy,
    ) -> Result<WorkspaceHandle, TargetError> {
        transfer.validate()?;
        Ok(WorkspaceHandle {
            workspace_id: transfer.workspace_id.clone(),
            snapshot_id: transfer.snapshot_id.clone(),
            // K17 materializes this transfer into an app-owned path on the
            // selected node. The origin must never supply an arbitrary remote
            // filesystem path.
            path: PathBuf::from("."),
            policy,
            base_snapshot_digest: transfer.base_snapshot_digest.clone(),
            base_transfer: Some(transfer.clone()),
        })
    }

    fn submit_run(&self, request: RunRequest) -> Result<TargetRunHandle, TargetError> {
        request.target.require(&request.required_capabilities)?;
        if request.target.identity.kind != ExecutionTargetKind::RemoteNode
            || request.target.identity.stable_id != self.alias()
        {
            return Err(TargetError::TargetIdentityChanged(
                "run was frozen for a different execution target".to_string(),
            ));
        }
        let transfer = request
            .workspace_transfer
            .clone()
            .or_else(|| request.workspace.base_transfer.clone())
            .ok_or_else(|| {
                TargetError::workspace_transfer_failed("remote-node run omitted workspace transfer")
            })?;
        transfer.validate()?;
        let mut spec = request.run_spec.clone().ok_or_else(|| {
            TargetError::invalid(
                "remote-node submission requires the frozen RunSpec used by the K17 placement plane",
            )
        })?;
        if spec.run_id != request.run_id {
            return Err(TargetError::invalid(
                "remote-node RunRequest and RunSpec ids do not match",
            ));
        }
        spec.execution_target = Some(request.target.clone());
        spec.workspace_transfer = Some(transfer.clone());
        spec.validate()
            .map_err(|error| TargetError::invalid(error.to_string()))?;
        let spec_path = self.write_spec(&spec)?;
        let spec_arg = spec_path.to_string_lossy().into_owned();
        let response = self.cli_json([
            "daemon",
            "remote",
            "place",
            "--spec",
            spec_arg.as_str(),
            "--alias",
            self.alias(),
            "--json",
        ])?;
        let placement = response
            .get("placement")
            .ok_or_else(|| TargetError::protocol_incompatible("K17 placement omitted response"))?;
        let submitted_run_id = placement
            .get("submitted_run_id")
            .and_then(Value::as_str)
            .ok_or_else(|| TargetError::protocol_incompatible("placement omitted submitted run id"))?;
        let node_run_id = placement
            .get("node_run_id")
            .and_then(Value::as_str)
            .ok_or_else(|| TargetError::protocol_incompatible("placement omitted node run id"))?;
        if submitted_run_id != request.run_id || !valid_id(node_run_id) {
            return Err(TargetError::protocol_incompatible(
                "K17 placement returned inconsistent run identity",
            ));
        }
        let record = RemoteNodeRunRecord {
            run_id: request.run_id.clone(),
            submitted_run_id: submitted_run_id.to_string(),
            node_run_id: node_run_id.to_string(),
            workspace: request.workspace.clone(),
            base_transfer: transfer,
        };
        let mut records = self.load_records()?;
        records.insert(request.run_id.clone(), record);
        self.save_records(&records)?;
        Ok(TargetRunHandle {
            run_id: request.run_id,
            remote_id: submitted_run_id.to_string(),
            target_id: self.alias().to_string(),
        })
    }

    fn attach_run(&self, run_id: &str) -> Result<TargetRunHandle, TargetError> {
        if !valid_id(run_id) {
            return Err(TargetError::invalid("run id is invalid"));
        }
        let record = self
            .load_records()?
            .get(run_id)
            .cloned()
            .ok_or_else(|| TargetError::runner_lost("remote-node run record was not found"))?;
        // Do an authoritative reconciliation before reporting successful attach.
        let _ = self.placement_for(&record.submitted_run_id)?;
        Ok(TargetRunHandle {
            run_id: run_id.to_string(),
            remote_id: record.submitted_run_id,
            target_id: self.alias().to_string(),
        })
    }

    fn events(
        &self,
        handle: &TargetRunHandle,
        after_sequence: u64,
    ) -> Result<Vec<TargetEvent>, TargetError> {
        let record = self.record_for(handle)?;
        let after = after_sequence.to_string();
        let value = self.cli_json([
            "daemon",
            "remote",
            "events",
            self.alias(),
            record.node_run_id.as_str(),
            "--after",
            after.as_str(),
        ])?;
        Ok(value
            .get("events")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .enumerate()
            .map(|(index, event)| TargetEvent {
                sequence: event
                    .get("sequence")
                    .or_else(|| event.get("seq"))
                    .and_then(Value::as_u64)
                    .unwrap_or(after_sequence.saturating_add(index as u64 + 1)),
                run_id: handle.run_id.clone(),
                kind: event
                    .get("kind")
                    .or_else(|| event.get("type"))
                    .and_then(Value::as_str)
                    .unwrap_or("remote_event")
                    .to_string(),
                message: event
                    .get("message")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .unwrap_or_else(|| event.to_string()),
                at_ms: event
                    .get("at_ms")
                    .or_else(|| event.get("atMs"))
                    .or_else(|| event.get("timestamp_ms"))
                    .and_then(Value::as_u64)
                    .unwrap_or_else(now_ms),
            })
            .collect())
    }

    fn status(&self, handle: &TargetRunHandle) -> Result<TargetRunStatus, TargetError> {
        let state = self
            .placement_for(&handle.remote_id)?
            .get("state")
            .and_then(Value::as_str)
            .unwrap_or("lost")
            .to_ascii_lowercase();
        Ok(match state.as_str() {
            "accepted" | "queued" => TargetRunStatus::Queued,
            "running" | "paused" => TargetRunStatus::Running,
            "succeeded" => TargetRunStatus::Succeeded,
            "failed" | "denied" => TargetRunStatus::Failed,
            "cancelled" | "canceled" => TargetRunStatus::Cancelled,
            _ => TargetRunStatus::Lost,
        })
    }

    fn cancel(&self, handle: &TargetRunHandle) -> Result<(), TargetError> {
        let record = self.record_for(handle)?;
        self.cli_json([
            "daemon",
            "remote",
            "cancel",
            self.alias(),
            record.node_run_id.as_str(),
            "--reason",
            "cancelled by execution-target client",
        ])?;
        Ok(())
    }

    fn pause(&self, handle: &TargetRunHandle) -> Result<(), TargetError> {
        let record = self.record_for(handle)?;
        self.cli_json([
            "daemon",
            "remote",
            "pause",
            self.alias(),
            record.node_run_id.as_str(),
        ])?;
        Ok(())
    }

    fn resume(&self, handle: &TargetRunHandle) -> Result<(), TargetError> {
        let record = self.record_for(handle)?;
        self.cli_json([
            "daemon",
            "remote",
            "resume",
            self.alias(),
            record.node_run_id.as_str(),
        ])?;
        Ok(())
    }

    fn artifacts(&self, handle: &TargetRunHandle) -> Result<Vec<ArtifactDescriptor>, TargetError> {
        let result = self.placed_result(&handle.remote_id)?;
        self.artifact_descriptors(&handle.remote_id, &result)
    }

    fn workspace_result(&self, handle: &TargetRunHandle) -> Result<WorkspaceResult, TargetError> {
        if self.status(handle)? != TargetRunStatus::Succeeded {
            return Err(TargetError::result_retrieval_failed(
                "remote-node run has not completed successfully",
            ));
        }
        let record = self.record_for(handle)?;
        let result = self.placed_result(&record.submitted_run_id)?;
        let artifacts = self.artifact_descriptors(&record.submitted_run_id, &result)?;
        let patch = if let Some(artifact_id) = Self::patch_artifact_id(&result) {
            self.fetch_artifact_bytes(&record.submitted_run_id, artifact_id)?
        } else {
            Vec::new()
        };
        if result.get("mutation").is_some() && patch.is_empty() {
            return Err(TargetError::result_retrieval_failed(
                "remote mutation result omitted its verified patch artifact",
            ));
        }
        let resulting_snapshot_digest = if patch.is_empty() {
            record.base_transfer.base_snapshot_digest.clone()
        } else {
            let mut hasher = Sha256::new();
            hasher.update(record.base_transfer.base_snapshot_digest.as_bytes());
            hasher.update(&patch);
            format!("{:x}", hasher.finalize())
        };
        let workspace_result = WorkspaceResult {
            base_snapshot_digest: record.base_transfer.base_snapshot_digest,
            resulting_snapshot_digest,
            git_diff: patch,
            new_files: Vec::new(),
            deleted_files: Vec::new(),
            binary_changes: Vec::new(),
            artifacts,
            verification_evidence: Self::verification_evidence(&result),
        };
        workspace_result.validate()?;
        Ok(workspace_result)
    }

    fn cleanup(&self, workspace: &WorkspaceHandle) -> Result<(), TargetError> {
        // App-owned remote workspace lifetime remains owned by K17. There is no
        // unsafe arbitrary-path cleanup call. Remove only local correlation
        // records once no active record refers to this transfer.
        let mut records = self.load_records()?;
        records.retain(|_, record| {
            record.workspace.workspace_id != workspace.workspace_id
                || record.workspace.snapshot_id != workspace.snapshot_id
        });
        self.save_records(&records)
    }
}

fn classify_remote_cli_error(detail: String) -> TargetError {
    let lower = detail.to_ascii_lowercase();
    if lower.contains("runner id") && (lower.contains("changed") || lower.contains("re-pair")) {
        TargetError::TargetIdentityChanged(detail)
    } else if lower.contains("certificate") && lower.contains("pin") {
        TargetError::TargetIdentityChanged(detail)
    } else if lower.contains("unreachable")
        || lower.contains("did not answer")
        || lower.contains("connection")
        || lower.contains("timed out")
    {
        TargetError::target_unreachable(detail)
    } else if lower.contains("residency")
        || lower.contains("cannot take this run")
        || lower.contains("refusing work")
    {
        TargetError::capability_unavailable(detail)
    } else if lower.contains("workspace") && lower.contains("conflict") {
        TargetError::workspace_conflict(detail)
    } else {
        TargetError::runner_lost(detail)
    }
}
