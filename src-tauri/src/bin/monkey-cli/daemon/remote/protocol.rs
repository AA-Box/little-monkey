use std::collections::{BTreeMap, BTreeSet};

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use little_monkey_lib::run_ledger::StoredRun;
use ring::rand::SecureRandom;
use ring::{hmac, rand as ring_rand};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub const REMOTE_PROTOCOL_VERSION: u32 = 1;
pub const DEFAULT_REQUEST_SKEW_MS: u64 = 5 * 60 * 1_000;
pub const MAX_REMOTE_BODY_BYTES: usize = 1024 * 1024;
pub const MAX_REMOTE_ARTIFACT_BYTES: u64 = 32 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RemoteAction {
    ViewRuns,
    ViewEvents,
    ReadArtifacts,
    Approve,
    Cancel,
    Kill,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RemoteScopes {
    pub actions: BTreeSet<RemoteAction>,
    /// Exact durable run ids visible to the controller.
    #[serde(default)]
    pub run_ids: BTreeSet<String>,
    /// Workspace ids visible to the controller. This permits future runs only
    /// inside an already-declared workspace; canonical filesystem paths are
    /// never accepted as scope selectors.
    #[serde(default)]
    pub workspace_ids: BTreeSet<String>,
    #[serde(default = "default_artifact_budget")]
    pub max_artifact_bytes: u64,
}

fn default_artifact_budget() -> u64 {
    MAX_REMOTE_ARTIFACT_BYTES
}

impl RemoteScopes {
    pub fn validate(&self) -> Result<(), String> {
        if self.actions.is_empty() {
            return Err("Remote pairing requires at least one action".to_string());
        }
        if self.run_ids.is_empty() && self.workspace_ids.is_empty() {
            return Err(
                "Remote pairing requires an exact run id or declared workspace id".to_string(),
            );
        }
        if self.run_ids.len() > 1_024 || self.workspace_ids.len() > 128 {
            return Err("Remote pairing scope is too large".to_string());
        }
        for value in self.run_ids.iter().chain(self.workspace_ids.iter()) {
            validate_id(value)?;
        }
        if self.max_artifact_bytes == 0 || self.max_artifact_bytes > MAX_REMOTE_ARTIFACT_BYTES {
            return Err(format!(
                "Remote artifact budget must be between 1 and {MAX_REMOTE_ARTIFACT_BYTES} bytes"
            ));
        }
        if self.actions.contains(&RemoteAction::Approve)
            && !self.actions.contains(&RemoteAction::ViewRuns)
        {
            return Err("Approve scope also requires view_runs".to_string());
        }
        if self.actions.contains(&RemoteAction::ReadArtifacts)
            && !self.actions.contains(&RemoteAction::ViewRuns)
        {
            return Err("Artifact scope also requires view_runs".to_string());
        }
        Ok(())
    }

    pub fn permits(&self, action: RemoteAction) -> bool {
        self.actions.contains(&action)
    }

    pub fn permits_run(&self, run: &StoredRun) -> bool {
        self.run_ids.contains(&run.spec.run_id)
            || run
                .spec
                .workspace
                .as_ref()
                .is_some_and(|workspace| self.workspace_ids.contains(&workspace.workspace_id))
    }

    pub fn is_subset_of(&self, parent: &Self) -> bool {
        self.actions.is_subset(&parent.actions)
            && self.run_ids.is_subset(&parent.run_ids)
            && self.workspace_ids.is_subset(&parent.workspace_ids)
            && self.max_artifact_bytes <= parent.max_artifact_bytes
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PairingInvitation {
    pub protocol_version: u32,
    pub runner_id: String,
    pub runner_url: String,
    pub server_certificate_pem: String,
    pub server_certificate_sha256: String,
    pub pairing_id: String,
    pub pairing_token: String,
    pub expires_at_ms: u64,
    pub scopes: RemoteScopes,
}

impl PairingInvitation {
    pub fn validate(&self, now_ms: u64) -> Result<(), String> {
        if self.protocol_version != REMOTE_PROTOCOL_VERSION {
            return Err(format!(
                "Unsupported remote protocol version {}",
                self.protocol_version
            ));
        }
        validate_id(&self.runner_id)?;
        validate_id(&self.pairing_id)?;
        if self.pairing_token.len() < 32 || self.pairing_token.len() > 512 {
            return Err("Pairing token has an invalid length".to_string());
        }
        if self.expires_at_ms <= now_ms {
            return Err("Pairing invitation has expired".to_string());
        }
        if !self.runner_url.starts_with("https://") {
            return Err("Remote runner URL must use HTTPS".to_string());
        }
        validate_sha256(&self.server_certificate_sha256)?;
        if self.server_certificate_pem.len() > 128 * 1024
            || !self.server_certificate_pem.contains("BEGIN CERTIFICATE")
        {
            return Err("Pairing invitation has no valid pinned certificate".to_string());
        }
        self.scopes.validate()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PairAcceptRequest {
    pub protocol_version: u32,
    pub pairing_id: String,
    pub pairing_token: String,
    pub device_name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PairAcceptResponse {
    pub protocol_version: u32,
    pub runner_id: String,
    pub device_id: String,
    pub secret_generation: u64,
    /// Returned exactly once through the pinned TLS connection, then stored
    /// in the controller keychain. It is never written to either SQLite DB.
    pub device_secret: String,
    pub scopes: RemoteScopes,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RotationBundle {
    pub protocol_version: u32,
    pub runner_id: String,
    pub device_id: String,
    pub secret_generation: u64,
    pub device_secret: String,
    pub runner_url: String,
    pub server_certificate_pem: String,
    pub server_certificate_sha256: String,
    pub scopes: RemoteScopes,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RemoteHostConfig {
    pub protocol_version: u32,
    pub runner_id: String,
    pub listen: String,
    pub advertise_url: String,
    pub certificate_path: String,
    pub private_key_path: String,
    pub certificate_sha256: String,
    pub enabled: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ControllerProfile {
    pub protocol_version: u32,
    pub alias: String,
    pub runner_id: String,
    pub runner_url: String,
    pub server_certificate_pem: String,
    pub server_certificate_sha256: String,
    pub device_id: String,
    pub secret_generation: u64,
    pub scopes: RemoteScopes,
    pub next_sequence: u64,
    #[serde(default)]
    pub event_cursors: BTreeMap<String, u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SignedRequestHeaders {
    pub device_id: String,
    pub secret_generation: u64,
    pub sequence: u64,
    pub timestamp_ms: u64,
    pub nonce: String,
    pub command_id: String,
    pub signature: String,
}

impl SignedRequestHeaders {
    pub fn new(
        device_id: String,
        secret_generation: u64,
        sequence: u64,
        timestamp_ms: u64,
        method: &str,
        path_and_query: &str,
        body: &[u8],
        secret: &[u8],
    ) -> Result<Self, String> {
        let nonce = random_token(18)?;
        let command_id = format!("cmd-{}", random_token(18)?);
        let mut value = Self {
            device_id,
            secret_generation,
            sequence,
            timestamp_ms,
            nonce,
            command_id,
            signature: String::new(),
        };
        value.signature = sign_request(secret, &value, method, path_and_query, body);
        Ok(value)
    }

    pub fn validate_shape(&self, now_ms: u64) -> Result<(), String> {
        validate_id(&self.device_id)?;
        validate_id(&self.command_id)?;
        if self.secret_generation == 0 || self.sequence == 0 {
            return Err("Remote generation and sequence must be positive".to_string());
        }
        if now_ms.abs_diff(self.timestamp_ms) > DEFAULT_REQUEST_SKEW_MS {
            return Err("Remote request timestamp is outside the accepted window".to_string());
        }
        if self.nonce.len() < 16 || self.nonce.len() > 256 {
            return Err("Remote request nonce has an invalid length".to_string());
        }
        if self.signature.len() != 64
            || !self.signature.bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            return Err("Remote request signature must be a SHA-256 hex digest".to_string());
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunSummary {
    pub run_id: String,
    pub status: String,
    pub kind: String,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
    pub last_sequence: u64,
    pub workspace_id: Option<String>,
    pub model_label: String,
    pub pending_approval_count: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApprovalRequestBody {
    pub request_id: String,
    pub operation_sha256: String,
    pub decision: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CancelRequestBody {
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuditEntry {
    pub audit_id: u64,
    pub occurred_at_ms: u64,
    pub device_id: Option<String>,
    pub action: String,
    pub target: Option<String>,
    pub outcome: String,
    pub request_sha256: Option<String>,
}

pub fn canonical_request(
    headers: &SignedRequestHeaders,
    method: &str,
    path_and_query: &str,
    body: &[u8],
) -> Vec<u8> {
    format!(
        "{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}",
        REMOTE_PROTOCOL_VERSION,
        method.to_ascii_uppercase(),
        path_and_query,
        headers.device_id,
        headers.secret_generation,
        headers.sequence,
        headers.timestamp_ms,
        headers.nonce,
        sha256_hex(body)
    )
    .into_bytes()
}

pub fn sign_request(
    secret: &[u8],
    headers: &SignedRequestHeaders,
    method: &str,
    path_and_query: &str,
    body: &[u8],
) -> String {
    let key = hmac::Key::new(hmac::HMAC_SHA256, secret);
    hex_encode(hmac::sign(
        &key,
        &canonical_request(headers, method, path_and_query, body),
    ))
}

pub fn verify_request(
    secret: &[u8],
    headers: &SignedRequestHeaders,
    method: &str,
    path_and_query: &str,
    body: &[u8],
) -> bool {
    let Ok(signature) = hex_decode(&headers.signature) else {
        return false;
    };
    let key = hmac::Key::new(hmac::HMAC_SHA256, secret);
    hmac::verify(
        &key,
        &canonical_request(headers, method, path_and_query, body),
        &signature,
    )
    .is_ok()
}

pub fn random_token(bytes: usize) -> Result<String, String> {
    let mut value = vec![0u8; bytes];
    ring_rand::SystemRandom::new()
        .fill(&mut value)
        .map_err(|_| "Operating system random generator failed".to_string())?;
    Ok(URL_SAFE_NO_PAD.encode(value))
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

pub fn certificate_fingerprint(pem: &[u8]) -> Result<String, String> {
    let der = first_pem_block(pem, "CERTIFICATE")?;
    Ok(sha256_hex(&der))
}

pub fn first_pem_block(pem: &[u8], label: &str) -> Result<Vec<u8>, String> {
    let text = std::str::from_utf8(pem).map_err(|_| "PEM file is not UTF-8".to_string())?;
    let begin = format!("-----BEGIN {label}-----");
    let end = format!("-----END {label}-----");
    let start = text
        .find(&begin)
        .ok_or_else(|| format!("PEM block '{label}' is missing"))?
        + begin.len();
    let finish = text[start..]
        .find(&end)
        .ok_or_else(|| format!("PEM block '{label}' is incomplete"))?
        + start;
    let encoded = text[start..finish]
        .chars()
        .filter(|character| !character.is_whitespace())
        .collect::<String>();
    base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .map_err(|error| format!("PEM block '{label}' is invalid: {error}"))
}

pub fn validate_id(value: &str) -> Result<(), String> {
    if value.is_empty()
        || value.len() > 256
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':'))
    {
        return Err(format!("Invalid remote identifier '{value}'"));
    }
    Ok(())
}

pub fn validate_sha256(value: &str) -> Result<(), String> {
    if value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        Err("Expected a SHA-256 hex digest".to_string())
    } else {
        Ok(())
    }
}

fn hex_encode(value: impl AsRef<[u8]>) -> String {
    value
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn hex_decode(value: &str) -> Result<Vec<u8>, String> {
    if value.len() % 2 != 0 {
        return Err("hex input has odd length".to_string());
    }
    (0..value.len())
        .step_by(2)
        .map(|index| {
            u8::from_str_radix(&value[index..index + 2], 16)
                .map_err(|_| "hex input is invalid".to_string())
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signed_request_binds_method_path_sequence_nonce_and_body() {
        let secret = b"0123456789abcdef0123456789abcdef";
        let mut headers = SignedRequestHeaders {
            device_id: "device-one".into(),
            secret_generation: 2,
            sequence: 9,
            timestamp_ms: 10_000,
            nonce: "nonce-0123456789abcdef".into(),
            command_id: "cmd-one".into(),
            signature: String::new(),
        };
        headers.signature = sign_request(secret, &headers, "POST", "/v1/runs/a/cancel", b"{}");
        assert!(verify_request(
            secret,
            &headers,
            "POST",
            "/v1/runs/a/cancel",
            b"{}"
        ));
        assert!(!verify_request(
            secret,
            &headers,
            "POST",
            "/v1/runs/b/cancel",
            b"{}"
        ));
        assert!(!verify_request(
            secret,
            &headers,
            "POST",
            "/v1/runs/a/cancel",
            b"{\"reason\":\"expanded\"}"
        ));
        headers.sequence += 1;
        assert!(!verify_request(
            secret,
            &headers,
            "POST",
            "/v1/runs/a/cancel",
            b"{}"
        ));
    }

    #[test]
    fn scope_subset_cannot_expand_actions_runs_or_artifact_budget() {
        let parent = RemoteScopes {
            actions: BTreeSet::from([RemoteAction::ViewRuns, RemoteAction::Cancel]),
            run_ids: BTreeSet::from(["run-one".into()]),
            workspace_ids: BTreeSet::new(),
            max_artifact_bytes: 1_024,
        };
        let narrowed = RemoteScopes {
            actions: BTreeSet::from([RemoteAction::ViewRuns]),
            run_ids: BTreeSet::from(["run-one".into()]),
            workspace_ids: BTreeSet::new(),
            max_artifact_bytes: 512,
        };
        assert!(narrowed.is_subset_of(&parent));
        let mut expanded = narrowed.clone();
        expanded.actions.insert(RemoteAction::Approve);
        assert!(!expanded.is_subset_of(&parent));
        expanded = narrowed.clone();
        expanded.run_ids.insert("run-two".into());
        assert!(!expanded.is_subset_of(&parent));
        expanded = narrowed;
        expanded.max_artifact_bytes = 2_048;
        assert!(!expanded.is_subset_of(&parent));
    }
}
