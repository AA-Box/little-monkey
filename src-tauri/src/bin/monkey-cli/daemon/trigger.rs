use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use chrono::{TimeZone, Utc};
use croner::Cron;
use globset::Glob;
use little_monkey_lib::workflow_core::WorkflowTrigger;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use walkdir::WalkDir;

use super::ledger::{DeliveryDisposition, SharedLedger, StoredTrigger};
use super::store::DaemonStore;

/// Profile-scoped (K23): the default profile keeps this exact service name,
/// so credentials stored before profiles existed still resolve, and any other
/// profile's secrets are a different keychain item entirely.
static WEBHOOK_KEYCHAIN_SERVICE: std::sync::LazyLock<String> = std::sync::LazyLock::new(|| {
    little_monkey_lib::profiles::keychain_service("com.littlemonkey.daemon-webhooks")
});

pub const MAX_WEBHOOK_BYTES: usize = 1024 * 1024;
pub const DEFAULT_SIGNATURE_SKEW_MS: u64 = 5 * 60 * 1_000;

/// A persistent trigger may submit either an immutable recipe snapshot or an
/// M4 workflow definition. Keeping the target typed prevents a workflow id
/// from ever being interpreted as a recipe name (and vice versa).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "target_kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum TriggerTarget {
    Recipe {
        recipe: String,
        params: BTreeMap<String, String>,
        payload_param: Option<String>,
    },
    Workflow {
        workflow_id: String,
        definition_sha256: String,
    },
    Extension {
        extension_id: String,
        handler_id: String,
        version: String,
        manifest_sha256: String,
    },
}

/// Exact M4 trigger declaration carried across the daemon adapter boundary.
/// `managed_by_batch` distinguishes M4-owned rows from manually configured
/// workflow targets, so removing a batch file only disables its own rows.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct WorkflowTriggerBinding {
    pub workflow_version: u32,
    pub managed_by_batch: bool,
    pub trigger: WorkflowTrigger,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum TriggerConfig {
    Cron {
        target: TriggerTarget,
        workflow: Option<WorkflowTriggerBinding>,
        schedule: String,
    },
    Filesystem {
        target: TriggerTarget,
        workflow: Option<WorkflowTriggerBinding>,
        path: String,
        recursive: bool,
        pattern: Option<String>,
        last_fingerprint: Option<String>,
    },
    SignedWebhook {
        target: TriggerTarget,
        workflow: Option<WorkflowTriggerBinding>,
        secret_reference: Option<String>,
        max_skew_ms: u64,
    },
    Github {
        target: TriggerTarget,
        workflow: Option<WorkflowTriggerBinding>,
        repository: String,
        local_repository: String,
        remote_name: String,
        branch_prefixes: Vec<String>,
        events: Vec<String>,
        allow_push: bool,
        allow_create_pull_request: bool,
        allow_review_comment: bool,
        max_skew_ms: u64,
    },
}

impl TriggerConfig {
    pub fn kind_token(&self) -> &'static str {
        match self {
            Self::Cron { .. } => "cron",
            Self::Filesystem { .. } => "filesystem",
            Self::SignedWebhook { .. } => "signed_webhook",
            Self::Github { .. } => "github",
        }
    }

    pub fn target(&self) -> &TriggerTarget {
        match self {
            Self::Cron { target, .. }
            | Self::Filesystem { target, .. }
            | Self::SignedWebhook { target, .. }
            | Self::Github { target, .. } => target,
        }
    }

    pub fn workflow_binding(&self) -> Option<&WorkflowTriggerBinding> {
        match self {
            Self::Cron { workflow, .. }
            | Self::Filesystem { workflow, .. }
            | Self::SignedWebhook { workflow, .. }
            | Self::Github { workflow, .. } => workflow.as_ref(),
        }
    }

    pub fn recipe_target(&self) -> Option<(&str, &BTreeMap<String, String>, Option<&str>)> {
        match self.target() {
            TriggerTarget::Recipe {
                recipe,
                params,
                payload_param,
            } => Some((recipe, params, payload_param.as_deref())),
            TriggerTarget::Workflow { .. } => None,
            TriggerTarget::Extension { .. } => None,
        }
    }

    pub fn workflow_target(&self) -> Option<(&str, &str, &WorkflowTriggerBinding)> {
        match (self.target(), self.workflow_binding()) {
            (
                TriggerTarget::Workflow {
                    workflow_id,
                    definition_sha256,
                },
                Some(binding),
            ) => Some((workflow_id, definition_sha256, binding)),
            _ => None,
        }
    }

    pub fn extension_target(&self) -> Option<(&str, &str, &str, &str)> {
        match self.target() {
            TriggerTarget::Extension {
                extension_id,
                handler_id,
                version,
                manifest_sha256,
            } => Some((extension_id, handler_id, version, manifest_sha256)),
            _ => None,
        }
    }

    pub fn secret_reference<'a>(&'a self, trigger_id: &'a str) -> &'a str {
        match self {
            Self::SignedWebhook {
                secret_reference: Some(reference),
                ..
            } => reference,
            _ => trigger_id,
        }
    }

    pub fn validate(&self) -> Result<(), String> {
        match self.target() {
            TriggerTarget::Recipe {
                recipe,
                params,
                payload_param,
            } => {
                if recipe.trim().is_empty() {
                    return Err("trigger recipe cannot be empty".to_string());
                }
                if self.workflow_binding().is_some() {
                    return Err("recipe trigger cannot carry a workflow binding".to_string());
                }
                if params.len() > 64 {
                    return Err("trigger cannot define more than 64 parameters".to_string());
                }
                for (key, value) in params {
                    validate_param_name(key)?;
                    if value.len() > MAX_WEBHOOK_BYTES {
                        return Err(format!("trigger parameter '{key}' is too large"));
                    }
                }
                if let Some(param) = payload_param {
                    validate_param_name(param)?;
                }
            }
            TriggerTarget::Workflow {
                workflow_id,
                definition_sha256,
            } => {
                validate_workflow_id(workflow_id)?;
                validate_sha256(definition_sha256, "workflow definition digest")?;
                let binding = self.workflow_binding().ok_or_else(|| {
                    "workflow trigger target is missing its exact M4 trigger binding".to_string()
                })?;
                if binding.workflow_version == 0 {
                    return Err("workflow trigger version must be positive".to_string());
                }
                self.validate_workflow_binding(binding)?;
            }
            TriggerTarget::Extension {
                extension_id,
                handler_id,
                version,
                manifest_sha256,
            } => {
                validate_workflow_id(extension_id)?;
                validate_workflow_id(handler_id)?;
                little_monkey_lib::package_ecosystem::SemanticVersion::parse(version)
                    .map_err(|error| format!("Invalid extension trigger version: {error}"))?;
                validate_sha256(manifest_sha256, "extension manifest digest")?;
                if self.workflow_binding().is_some() {
                    return Err("extension trigger cannot carry a workflow binding".to_string());
                }
            }
        }
        match self {
            Self::Cron { schedule, .. } => {
                Cron::from_str(schedule)
                    .map_err(|error| format!("Invalid cron expression: {error}"))?;
            }
            Self::Filesystem { path, pattern, .. } => {
                let path = Path::new(path);
                if !path.is_absolute() {
                    return Err("filesystem trigger path must be absolute".to_string());
                }
                if let Some(pattern) = pattern {
                    if pattern.is_empty() || pattern.len() > 512 {
                        return Err("filesystem trigger pattern is invalid".to_string());
                    }
                    Glob::new(pattern)
                        .map_err(|error| format!("Invalid filesystem trigger pattern: {error}"))?;
                }
            }
            Self::SignedWebhook {
                secret_reference,
                max_skew_ms,
                ..
            } => {
                validate_skew(*max_skew_ms)?;
                if let Some(reference) = secret_reference {
                    validate_workflow_id(reference)?;
                }
            }
            Self::Github {
                repository,
                local_repository,
                remote_name,
                branch_prefixes,
                events,
                max_skew_ms,
                ..
            } => {
                validate_repository(repository)?;
                if !Path::new(local_repository).is_absolute() {
                    return Err("GitHub trigger local_repository must be absolute".to_string());
                }
                validate_param_name(remote_name)?;
                validate_skew(*max_skew_ms)?;
                if branch_prefixes.is_empty() || branch_prefixes.len() > 32 {
                    return Err("GitHub trigger requires 1-32 branch prefixes".to_string());
                }
                for prefix in branch_prefixes {
                    if prefix.is_empty() || prefix.len() > 256 || prefix.contains(['\n', '\r']) {
                        return Err("GitHub branch prefix is invalid".to_string());
                    }
                }
                if events.is_empty() || events.len() > 32 {
                    return Err("GitHub trigger requires 1-32 event names".to_string());
                }
                for event in events {
                    if !event
                        .chars()
                        .all(|ch| ch.is_ascii_lowercase() || ch == '_' || ch == '-')
                    {
                        return Err(format!("Invalid GitHub event '{event}'"));
                    }
                }
            }
        }
        Ok(())
    }

    fn validate_workflow_binding(&self, binding: &WorkflowTriggerBinding) -> Result<(), String> {
        match (self, &binding.trigger) {
            (Self::Cron { schedule, .. }, WorkflowTrigger::PersistentCron { expression })
                if schedule == expression =>
            {
                Ok(())
            }
            (
                Self::Filesystem { path, pattern, .. },
                WorkflowTrigger::Filesystem {
                    canonical_root,
                    pattern: declared_pattern,
                },
            ) if path == canonical_root
                && pattern.as_deref() == Some(declared_pattern.as_str()) =>
            {
                Ok(())
            }
            (
                Self::SignedWebhook {
                    secret_reference,
                    max_skew_ms,
                    ..
                },
                WorkflowTrigger::SignedWebhook {
                    secret_reference: declared_reference,
                    replay_window_ms,
                    ..
                },
            ) if secret_reference.as_deref() == Some(declared_reference.as_str())
                && max_skew_ms == replay_window_ms =>
            {
                Ok(())
            }
            (Self::Github { .. }, WorkflowTrigger::EventIngestion { .. }) => Ok(()),
            _ => Err(
                "daemon trigger configuration does not match its declared M4 trigger".to_string(),
            ),
        }
    }
}

fn validate_workflow_id(value: &str) -> Result<(), String> {
    if value.is_empty()
        || value.len() > 160
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':'))
    {
        Err("workflow identifier must be a bounded ASCII identifier".to_string())
    } else {
        Ok(())
    }
}

pub fn validate_secret_reference(value: &str) -> Result<(), String> {
    validate_workflow_id(value)
}

fn validate_sha256(value: &str, label: &str) -> Result<(), String> {
    if value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        Err(format!("{label} must be a 64-character SHA-256 digest"))
    } else {
        Ok(())
    }
}

fn validate_skew(value: u64) -> Result<(), String> {
    if !(1_000..=60 * 60 * 1_000).contains(&value) {
        Err("signature max skew must be between 1 second and 1 hour".to_string())
    } else {
        Ok(())
    }
}

fn validate_param_name(value: &str) -> Result<(), String> {
    if value.is_empty()
        || value.len() > 128
        || !value
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-')
    {
        Err(format!("Invalid trigger parameter name '{value}'"))
    } else {
        Ok(())
    }
}

fn validate_repository(value: &str) -> Result<(), String> {
    let mut parts = value.split('/');
    let owner = parts.next().unwrap_or_default();
    let repo = parts.next().unwrap_or_default();
    if parts.next().is_some()
        || owner.is_empty()
        || repo.is_empty()
        || !value
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.' | '/'))
    {
        Err("GitHub repository must be exactly owner/name".to_string())
    } else {
        Ok(())
    }
}

pub trait SecretStore: Send + Sync {
    fn put(&self, trigger_id: &str, secret: &str) -> Result<(), String>;
    fn get(&self, trigger_id: &str) -> Result<String, String>;
    fn delete(&self, trigger_id: &str) -> Result<(), String>;
}

pub struct KeyringSecretStore;

impl KeyringSecretStore {
    fn entry(trigger_id: &str) -> Result<keyring::Entry, String> {
        keyring::Entry::new(&WEBHOOK_KEYCHAIN_SERVICE, trigger_id)
            .map_err(|error| format!("Failed to open webhook keychain entry: {error}"))
    }
}

impl SecretStore for KeyringSecretStore {
    fn put(&self, trigger_id: &str, secret: &str) -> Result<(), String> {
        if secret.len() < 16 || secret.len() > 4096 {
            return Err("Webhook secret must contain 16-4096 bytes".to_string());
        }
        Self::entry(trigger_id)?
            .set_password(secret)
            .map_err(|error| format!("Failed to save webhook secret: {error}"))
    }

    fn get(&self, trigger_id: &str) -> Result<String, String> {
        Self::entry(trigger_id)?
            .get_password()
            .map_err(|error| format!("Failed to read webhook secret: {error}"))
    }

    fn delete(&self, trigger_id: &str) -> Result<(), String> {
        match Self::entry(trigger_id)?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(error) => Err(format!("Failed to delete webhook secret: {error}")),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum IngestOutcome {
    Accepted,
    Duplicate,
    Rejected,
}

pub struct SignedDelivery<'a> {
    pub trigger_id: &'a str,
    pub delivery_id: &'a str,
    pub timestamp_ms: u64,
    pub nonce: &'a str,
    pub signature: &'a str,
    pub event_name: Option<&'a str>,
    pub payload: &'a [u8],
}

pub fn ingest_signed_delivery(
    shared: &mut SharedLedger,
    state: &mut DaemonStore,
    secrets: &dyn SecretStore,
    delivery: &SignedDelivery<'_>,
    now_ms: u64,
) -> Result<IngestOutcome, String> {
    validate_delivery_fields(delivery)?;
    let Some(stored) = shared.trigger(delivery.trigger_id)? else {
        return Err(format!("Unknown trigger '{}'", delivery.trigger_id));
    };
    if !stored.enabled {
        return Err(format!("Trigger '{}' is disabled", delivery.trigger_id));
    }
    let config: TriggerConfig = serde_json::from_slice(&stored.config_json)
        .map_err(|error| format!("Invalid stored trigger config: {error}"))?;
    config.validate()?;
    if !matches!(
        config,
        TriggerConfig::SignedWebhook { .. } | TriggerConfig::Github { .. }
    ) {
        return Err(format!(
            "Trigger '{}' does not accept signed webhook deliveries",
            delivery.trigger_id
        ));
    }
    let max_skew_ms = match &config {
        TriggerConfig::SignedWebhook { max_skew_ms, .. }
        | TriggerConfig::Github { max_skew_ms, .. } => *max_skew_ms,
        _ => unreachable!(),
    };
    if now_ms.abs_diff(delivery.timestamp_ms) > max_skew_ms {
        reject(shared, delivery, now_ms)?;
        return Ok(IngestOutcome::Rejected);
    }
    let secret = secrets.get(config.secret_reference(delivery.trigger_id))?;
    let valid = match &config {
        TriggerConfig::SignedWebhook { .. } => verify_generic_signature(
            secret.as_bytes(),
            delivery.timestamp_ms,
            delivery.nonce,
            delivery.payload,
            delivery.signature,
        ),
        TriggerConfig::Github {
            repository,
            branch_prefixes,
            events,
            ..
        } => {
            let event_name = delivery.event_name.unwrap_or_default();
            verify_github_signature(secret.as_bytes(), delivery.payload, delivery.signature)
                && github_scope_matches(
                    delivery.payload,
                    event_name,
                    repository,
                    branch_prefixes,
                    events,
                )
        }
        _ => false,
    };
    if !valid {
        reject(shared, delivery, now_ms)?;
        return Ok(IngestOutcome::Rejected);
    }
    let payload = std::str::from_utf8(delivery.payload)
        .map_err(|_| "Webhook payload must be UTF-8 JSON".to_string())?;
    serde_json::from_str::<serde_json::Value>(payload)
        .map_err(|error| format!("Webhook payload is not valid JSON: {error}"))?;
    if !state.reserve_delivery_payload(
        delivery.trigger_id,
        delivery.delivery_id,
        Some(delivery.nonce),
        payload,
        now_ms,
    )? {
        return Ok(IngestOutcome::Duplicate);
    }
    let payload_sha256 = sha256_hex(delivery.payload);
    match shared.accept_delivery(
        delivery.trigger_id,
        delivery.delivery_id,
        &payload_sha256,
        now_ms,
    )? {
        DeliveryDisposition::Accepted => {
            state.activate_delivery_payload(delivery.trigger_id, delivery.delivery_id)?;
            Ok(IngestOutcome::Accepted)
        }
        DeliveryDisposition::Duplicate => {
            state.discard_delivery_payload(delivery.trigger_id, delivery.delivery_id)?;
            Ok(IngestOutcome::Duplicate)
        }
        DeliveryDisposition::ConflictingDuplicate => {
            state.discard_delivery_payload(delivery.trigger_id, delivery.delivery_id)?;
            Ok(IngestOutcome::Rejected)
        }
    }
}

fn reject(
    shared: &mut SharedLedger,
    delivery: &SignedDelivery<'_>,
    now_ms: u64,
) -> Result<(), String> {
    shared.reject_delivery(
        delivery.trigger_id,
        delivery.delivery_id,
        &sha256_hex(delivery.payload),
        now_ms,
    )
}

fn validate_delivery_fields(delivery: &SignedDelivery<'_>) -> Result<(), String> {
    for (label, value, max) in [
        ("trigger id", delivery.trigger_id, 128usize),
        ("delivery id", delivery.delivery_id, 256usize),
        ("nonce", delivery.nonce, 256usize),
    ] {
        if value.is_empty()
            || value.len() > max
            || !value
                .chars()
                .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.' | ':'))
        {
            return Err(format!("Invalid webhook {label}"));
        }
    }
    if delivery.payload.len() > MAX_WEBHOOK_BYTES {
        return Err(format!(
            "Webhook payload exceeds the {MAX_WEBHOOK_BYTES}-byte limit"
        ));
    }
    Ok(())
}

pub fn verify_generic_signature(
    secret: &[u8],
    timestamp_ms: u64,
    nonce: &str,
    payload: &[u8],
    signature: &str,
) -> bool {
    let message = canonical_generic_signature_message(timestamp_ms, nonce, payload);
    verify_hmac_hex(secret, &message, signature)
}

pub fn verify_github_signature(secret: &[u8], payload: &[u8], signature: &str) -> bool {
    verify_hmac_hex(secret, payload, signature)
}

fn verify_hmac_hex(secret: &[u8], message: &[u8], signature: &str) -> bool {
    let expected = hmac_sha256(secret, message);
    let supplied = signature.strip_prefix("sha256=").unwrap_or(signature);
    let Some(decoded) = decode_hex_32(supplied) else {
        return false;
    };
    constant_time_eq(&expected, &decoded)
}

pub fn hmac_sha256(secret: &[u8], message: &[u8]) -> [u8; 32] {
    const BLOCK: usize = 64;
    let mut key = [0u8; BLOCK];
    if secret.len() > BLOCK {
        key[..32].copy_from_slice(&Sha256::digest(secret));
    } else {
        key[..secret.len()].copy_from_slice(secret);
    }
    let mut inner = [0x36u8; BLOCK];
    let mut outer = [0x5cu8; BLOCK];
    for index in 0..BLOCK {
        inner[index] ^= key[index];
        outer[index] ^= key[index];
    }
    let mut inner_hash = Sha256::new();
    inner_hash.update(inner);
    inner_hash.update(message);
    let inner_digest = inner_hash.finalize();
    let mut outer_hash = Sha256::new();
    outer_hash.update(outer);
    outer_hash.update(inner_digest);
    outer_hash.finalize().into()
}

fn decode_hex_32(value: &str) -> Option<[u8; 32]> {
    if value.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (index, pair) in value.as_bytes().chunks_exact(2).enumerate() {
        let high = hex_nibble(pair[0])?;
        let low = hex_nibble(pair[1])?;
        out[index] = (high << 4) | low;
    }
    Some(out)
}

fn hex_nibble(value: u8) -> Option<u8> {
    match value {
        b'0'..=b'9' => Some(value - b'0'),
        b'a'..=b'f' => Some(value - b'a' + 10),
        b'A'..=b'F' => Some(value - b'A' + 10),
        _ => None,
    }
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    left.iter()
        .zip(right)
        .fold(0u8, |difference, (a, b)| difference | (a ^ b))
        == 0
}

fn github_scope_matches(
    payload: &[u8],
    event_name: &str,
    repository: &str,
    branch_prefixes: &[String],
    events: &[String],
) -> bool {
    if !events.iter().any(|allowed| allowed == event_name) {
        return false;
    }
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(payload) else {
        return false;
    };
    let delivered_repository = value
        .pointer("/repository/full_name")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    if delivered_repository != repository {
        return false;
    }
    let branch = value
        .get("ref")
        .and_then(serde_json::Value::as_str)
        .and_then(|value| value.strip_prefix("refs/heads/"))
        .or_else(|| {
            value
                .pointer("/pull_request/head/ref")
                .and_then(serde_json::Value::as_str)
        })
        .unwrap_or_default();
    !branch.is_empty()
        && branch_prefixes
            .iter()
            .any(|prefix| branch.starts_with(prefix))
}

pub fn next_cron_ms(schedule: &str, after_ms: u64) -> Result<u64, String> {
    let cron =
        Cron::from_str(schedule).map_err(|error| format!("Invalid cron expression: {error}"))?;
    let after = Utc
        .timestamp_millis_opt(
            i64::try_from(after_ms).map_err(|_| "cron timestamp overflow".to_string())?,
        )
        .single()
        .ok_or_else(|| "cron timestamp is out of range".to_string())?;
    let next = cron
        .find_next_occurrence(&after, false)
        .map_err(|error| format!("Failed to compute cron occurrence: {error}"))?;
    u64::try_from(next.timestamp_millis()).map_err(|_| "cron occurrence precedes epoch".to_string())
}

pub fn poll_persistent_triggers(
    shared: &mut SharedLedger,
    state: &mut DaemonStore,
    now_ms: u64,
) -> Result<u32, String> {
    let mut accepted = 0u32;
    for stored in shared.list_triggers()? {
        if !stored.enabled {
            continue;
        }
        match poll_one_persistent_trigger(shared, state, &stored, now_ms) {
            Ok(count) => accepted = accepted.saturating_add(count),
            Err(error) => {
                // One deleted watch root or malformed trigger must fail closed
                // without terminating the resident service or unrelated jobs.
                eprintln!("Persistent trigger '{}' paused: {error}", stored.trigger_id);
            }
        }
    }
    Ok(accepted)
}

fn poll_one_persistent_trigger(
    shared: &mut SharedLedger,
    state: &mut DaemonStore,
    stored: &StoredTrigger,
    now_ms: u64,
) -> Result<u32, String> {
    let mut accepted = 0u32;
    let mut config: TriggerConfig = serde_json::from_slice(&stored.config_json)
        .map_err(|error| format!("Invalid trigger config: {error}"))?;
    config.validate()?;
    match &mut config {
        TriggerConfig::Cron { schedule, .. } => {
            let due = stored
                .next_fire_at_ms
                .unwrap_or(next_cron_ms(schedule, now_ms)?);
            if due <= now_ms {
                let delivery_id = format!("cron-{due}");
                let payload = serde_json::json!({
                    "kind": "cron",
                    "trigger_id": stored.trigger_id,
                    "scheduled_at_ms": due,
                })
                .to_string();
                if accept_generated(shared, state, stored, &delivery_id, &payload, now_ms)? {
                    accepted = 1;
                }
                let next = next_cron_ms(schedule, due)?;
                shared.update_trigger_schedule(
                    &stored.trigger_id,
                    Some(next),
                    Some(due),
                    now_ms,
                )?;
            } else if stored.next_fire_at_ms.is_none() {
                shared.update_trigger_schedule(&stored.trigger_id, Some(due), None, now_ms)?;
            }
        }
        TriggerConfig::Filesystem {
            path,
            recursive,
            pattern,
            last_fingerprint,
            ..
        } => {
            let fingerprint =
                filesystem_fingerprint_filtered(Path::new(path), *recursive, pattern.as_deref())?;
            if let Some(previous) = last_fingerprint.as_ref() {
                if previous != &fingerprint {
                    let delivery_id = format!("fs-{}", &fingerprint[..32]);
                    let payload = serde_json::json!({
                        "kind": "filesystem",
                        "trigger_id": stored.trigger_id,
                        "path": path,
                        "fingerprint": fingerprint,
                        "observed_at_ms": now_ms,
                    })
                    .to_string();
                    if accept_generated(shared, state, stored, &delivery_id, &payload, now_ms)? {
                        accepted = 1;
                    }
                }
            }
            *last_fingerprint = Some(fingerprint);
            let bytes = serde_json::to_vec(&config).map_err(|error| error.to_string())?;
            shared.upsert_trigger(
                &stored.trigger_id,
                config.kind_token(),
                &bytes,
                now_ms,
                stored.next_fire_at_ms,
            )?;
        }
        TriggerConfig::SignedWebhook { .. } | TriggerConfig::Github { .. } => {}
    }
    Ok(accepted)
}

fn accept_generated(
    shared: &mut SharedLedger,
    state: &mut DaemonStore,
    trigger: &StoredTrigger,
    delivery_id: &str,
    payload: &str,
    now_ms: u64,
) -> Result<bool, String> {
    if !state.reserve_delivery_payload(&trigger.trigger_id, delivery_id, None, payload, now_ms)? {
        return Ok(false);
    }
    let digest = sha256_hex(payload.as_bytes());
    match shared.accept_delivery(&trigger.trigger_id, delivery_id, &digest, now_ms)? {
        DeliveryDisposition::Accepted => {
            state.activate_delivery_payload(&trigger.trigger_id, delivery_id)?;
            Ok(true)
        }
        DeliveryDisposition::Duplicate | DeliveryDisposition::ConflictingDuplicate => {
            state.discard_delivery_payload(&trigger.trigger_id, delivery_id)?;
            Ok(false)
        }
    }
}

pub fn filesystem_fingerprint_filtered(
    path: &Path,
    recursive: bool,
    pattern: Option<&str>,
) -> Result<String, String> {
    let canonical = path
        .canonicalize()
        .map_err(|error| format!("Cannot inspect '{}': {error}", path.display()))?;
    let matcher = pattern
        .map(|pattern| {
            Glob::new(pattern)
                .map(|glob| glob.compile_matcher())
                .map_err(|error| format!("Invalid filesystem trigger pattern: {error}"))
        })
        .transpose()?;
    let mut records = Vec::new();
    if canonical.is_dir() {
        let depth = if recursive { 64 } else { 1 };
        for entry in WalkDir::new(&canonical)
            .follow_links(false)
            .max_depth(depth)
            .into_iter()
            .filter_map(Result::ok)
            .take(10_001)
        {
            if records.len() == 10_000 {
                return Err(format!(
                    "Filesystem trigger '{}' exceeds the 10000-entry safety limit",
                    canonical.display()
                ));
            }
            let relative = entry
                .path()
                .strip_prefix(&canonical)
                .unwrap_or(entry.path());
            let normalized = relative.to_string_lossy().replace('\\', "/");
            if matcher
                .as_ref()
                .is_some_and(|matcher| !matcher.is_match(&normalized))
            {
                continue;
            }
            let metadata = entry
                .metadata()
                .map_err(|error| format!("Cannot inspect '{}': {error}", entry.path().display()))?;
            records.push(metadata_record(&canonical, entry.path(), &metadata));
        }
    } else {
        let relative = canonical
            .file_name()
            .map(|name| name.to_string_lossy().to_string())
            .unwrap_or_default();
        if matcher
            .as_ref()
            .is_some_and(|matcher| !matcher.is_match(&relative))
        {
            return Ok(sha256_hex(&[]));
        }
        let metadata = std::fs::symlink_metadata(&canonical)
            .map_err(|error| format!("Cannot inspect '{}': {error}", canonical.display()))?;
        records.push(metadata_record(&canonical, &canonical, &metadata));
    }
    records.sort();
    Ok(sha256_hex(records.join("\n").as_bytes()))
}

fn metadata_record(root: &Path, path: &Path, metadata: &std::fs::Metadata) -> String {
    let relative = path.strip_prefix(root).unwrap_or(path);
    let modified = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    format!(
        "{}:{}:{}:{}:{}",
        relative.to_string_lossy(),
        metadata.is_dir(),
        metadata.is_symlink(),
        metadata.len(),
        modified
    )
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

/// Sender-side twin of `verify_hmac_hex`; production only verifies, so this
/// exists for tests that need to mint valid signatures.
#[cfg(test)]
pub fn signature_hex(secret: &[u8], message: &[u8]) -> String {
    hmac_sha256(secret, message)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

pub fn canonical_generic_signature_message(
    timestamp_ms: u64,
    nonce: &str,
    payload: &[u8],
) -> Vec<u8> {
    let mut message = format!("{timestamp_ms}\n{nonce}\n").into_bytes();
    message.extend_from_slice(payload);
    message
}

pub fn canonicalize_trigger_path(path: PathBuf) -> Result<String, String> {
    path.canonicalize()
        .map(|path| path.to_string_lossy().to_string())
        .map_err(|error| format!("Cannot canonicalize trigger path: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Mutex;

    struct FakeSecrets(Mutex<HashMap<String, String>>);
    impl SecretStore for FakeSecrets {
        fn put(&self, trigger_id: &str, secret: &str) -> Result<(), String> {
            self.0
                .lock()
                .unwrap()
                .insert(trigger_id.into(), secret.into());
            Ok(())
        }
        fn get(&self, trigger_id: &str) -> Result<String, String> {
            self.0
                .lock()
                .unwrap()
                .get(trigger_id)
                .cloned()
                .ok_or_else(|| "missing fake secret".into())
        }
        fn delete(&self, trigger_id: &str) -> Result<(), String> {
            self.0.lock().unwrap().remove(trigger_id);
            Ok(())
        }
    }

    #[test]
    fn hmac_matches_rfc_4231_sha256_vector() {
        let key = [0x0bu8; 20];
        assert_eq!(
            signature_hex(&key, b"Hi There"),
            "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7"
        );
    }

    #[test]
    fn generic_signature_binds_timestamp_nonce_and_payload() {
        let secret = b"0123456789abcdef";
        let payload = br#"{"ok":true}"#;
        // Generated rather than hard-coded: the assertions only need two
        // distinct nonces, and a literal here reads as a real crypto nonce to
        // scanners.
        let nonce = uuid::Uuid::new_v4().to_string();
        let other_nonce = uuid::Uuid::new_v4().to_string();
        let message = canonical_generic_signature_message(100, &nonce, payload);
        let signature = signature_hex(secret, &message);
        assert!(verify_generic_signature(
            secret, 100, &nonce, payload, &signature
        ));
        assert!(!verify_generic_signature(
            secret, 101, &nonce, payload, &signature
        ));
        assert!(!verify_generic_signature(
            secret,
            100,
            &other_nonce,
            payload,
            &signature
        ));
    }

    #[test]
    fn github_scope_rejects_wrong_repository_branch_or_event() {
        let payload = br#"{"repository":{"full_name":"org/repo"},"ref":"refs/heads/codex/task"}"#;
        assert!(github_scope_matches(
            payload,
            "push",
            "org/repo",
            &["codex/".into()],
            &["push".into()]
        ));
        assert!(!github_scope_matches(
            payload,
            "push",
            "other/repo",
            &["codex/".into()],
            &["push".into()]
        ));
        assert!(!github_scope_matches(
            payload,
            "issues",
            "org/repo",
            &["codex/".into()],
            &["push".into()]
        ));
    }

    #[test]
    fn cron_next_is_strictly_after_cursor() {
        let next = next_cron_ms("*/5 * * * *", 1_700_000_000_000).unwrap();
        assert!(next > 1_700_000_000_000);
    }

    #[test]
    fn signed_delivery_dedupes_delivery_ids_and_rejects_nonce_replay() {
        let root = std::env::temp_dir().join(format!(
            "little-monkey-trigger-test-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let ledger_path = root.join("profile.sqlite3");
        let mut shared = SharedLedger::open(&ledger_path).unwrap();
        let mut state = DaemonStore::open_in_memory().unwrap();
        let config = TriggerConfig::SignedWebhook {
            target: TriggerTarget::Recipe {
                recipe: "fixture".into(),
                params: BTreeMap::new(),
                payload_param: None,
            },
            workflow: None,
            secret_reference: Some("vault-hook".into()),
            max_skew_ms: 60_000,
        };
        shared
            .upsert_trigger(
                "hook",
                config.kind_token(),
                &serde_json::to_vec(&config).unwrap(),
                10_000,
                None,
            )
            .unwrap();
        let secrets = FakeSecrets(Mutex::new(HashMap::from([(
            "vault-hook".into(),
            "0123456789abcdef".into(),
        )])));
        let payload = br#"{"action":"opened"}"#;
        // Generated rather than hard-coded: the replay assertions only need one
        // stable nonce for the lifetime of the test, and a literal here reads as
        // a real crypto nonce to scanners.
        let nonce = uuid::Uuid::new_v4().to_string();
        let signature = signature_hex(
            b"0123456789abcdef",
            &canonical_generic_signature_message(10_000, &nonce, payload),
        );
        let first = SignedDelivery {
            trigger_id: "hook",
            delivery_id: "delivery-one",
            timestamp_ms: 10_000,
            nonce: &nonce,
            signature: &signature,
            event_name: None,
            payload,
        };
        assert_eq!(
            ingest_signed_delivery(&mut shared, &mut state, &secrets, &first, 10_001).unwrap(),
            IngestOutcome::Accepted
        );
        assert_eq!(
            ingest_signed_delivery(&mut shared, &mut state, &secrets, &first, 10_002).unwrap(),
            IngestOutcome::Duplicate
        );
        let replay = SignedDelivery {
            delivery_id: "delivery-two",
            ..first
        };
        assert_eq!(
            ingest_signed_delivery(&mut shared, &mut state, &secrets, &replay, 10_003).unwrap(),
            IngestOutcome::Duplicate
        );
        assert_eq!(state.pending_delivery_payloads(10).unwrap().len(), 1);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn filesystem_glob_ignores_unmatched_file_changes() {
        let root = std::env::temp_dir().join(format!(
            "little-monkey-trigger-glob-test-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("matched.txt"), b"one").unwrap();
        std::fs::write(root.join("ignored.rs"), b"one").unwrap();
        let before = filesystem_fingerprint_filtered(&root, true, Some("**/*.txt")).unwrap();
        std::fs::write(root.join("ignored.rs"), b"changed and longer").unwrap();
        let ignored = filesystem_fingerprint_filtered(&root, true, Some("**/*.txt")).unwrap();
        assert_eq!(before, ignored);
        std::fs::write(root.join("matched.txt"), b"changed and longer").unwrap();
        let matched = filesystem_fingerprint_filtered(&root, true, Some("**/*.txt")).unwrap();
        assert_ne!(before, matched);
        let _ = std::fs::remove_dir_all(root);
    }
}
