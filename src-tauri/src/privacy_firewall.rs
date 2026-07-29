//! Privacy Firewall (ROADMAP.md Phase 5): a visible, per-workspace data
//! boundary in front of outbound sends to a cloud model. Detection is
//! entirely delegated to `knowledge_pipeline::SensitiveDataScanner` — this
//! module writes no new detection regexes, only a persisted policy
//! (per-`SensitiveDataKind` action, a `local_only_fallback` flag, and literal
//! exceptions) and the mapping from a scan to a concrete, destination-tagged
//! decision.
//!
//! [`privacy_firewall_preview`] is pure and destination-agnostic: given
//! `content` plus a workspace's policy, it always returns the exact finding
//! spans, the single policy action each one resolved to, and a redaction
//! preview string — never a vague "this might be sensitive" warning, and the
//! preview string never contains the original text of any span it redacted.
//!
//! The two-phase `privacy_firewall_prepare_send` / `privacy_firewall_execute_send`
//! commands mirror `m5_delivery::{prepare_mutation_impl, execute_mutation_impl}`'s
//! digest-confirmed shape exactly (see that module's doc comment) for the
//! `RequireApproval` policy action: a preview is bound to a content digest
//! and a short-lived server-side pending entry, and executing requires
//! re-supplying the identical content plus the digest-derived confirmation
//! phrase — so a stale or tampered confirmation can never authorize sending
//! something the user never actually saw previewed.
//!
//! OUT OF SCOPE this pass (per the design doc's own scoping): wiring this
//! gate into every connector-write, MCP-tool-result, or paired-device send
//! path. Only the cloud-model chat-turn dispatch in `agentLoop.ts` calls
//! `privacy_firewall_preview`/`privacy_firewall_prepare_send` today. Paired
//! devices are Phase 4 and unshipped entirely; connector writes and MCP tool
//! results are separate follow-ups. The scanner/policy engine here takes an
//! explicit [`OutboundDestination`] specifically so those call sites are
//! additive later, never a redesign of this module.

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::knowledge_pipeline::{SensitiveDataKind, SensitiveDataScanner};
use crate::AppState;

const POLICY_DIR: &str = "privacy_firewall";
/// Mirrors `m5_delivery`'s confirmation-preview lifetime intent: long enough
/// for a user to read a redaction diff and click a button, short enough that
/// an abandoned tab can't be replayed hours later against content that may
/// have since been edited.
const PENDING_SEND_TTL_MS: u64 = 5 * 60 * 1_000;

/// Where outbound content is headed. The scanner/policy engine below never
/// branches on this beyond carrying it through to the report — it exists so
/// a per-destination policy is possible later without changing this shape,
/// and so a preview always states plainly which boundary it was evaluated
/// against.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum OutboundDestination {
    CloudModel,
    Connector,
    RemoteRunner,
    McpServer,
    PairedDevice,
}

/// What happens to a given `SensitiveDataKind` finding before it leaves the
/// machine. Also doubles as a preview's overall verdict — see
/// [`preview_impl`]'s doc comment for how the single strictest per-finding
/// action becomes the whole report's `verdict`.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PrivacyPolicyAction {
    Allow,
    Redact,
    Block,
    RequireApproval,
}

/// Ordering used only to pick the single strictest action across a set of
/// findings. Written out explicitly rather than derived from declaration
/// order (`derive(Ord)`) so reordering the enum's variants above can never
/// silently change which action wins: `Allow` < `Redact` < `RequireApproval`
/// < `Block` (`Block` is always the strictest — it can never be quietly
/// downgraded by a `Redact` or `RequireApproval` finding elsewhere in the
/// same content).
fn action_severity(action: PrivacyPolicyAction) -> u8 {
    match action {
        PrivacyPolicyAction::Allow => 0,
        PrivacyPolicyAction::Redact => 1,
        PrivacyPolicyAction::RequireApproval => 2,
        PrivacyPolicyAction::Block => 3,
    }
}

/// Per-workspace privacy policy, persisted at
/// `<app_data>/privacy_firewall/<sha256(workspace_id)>.json`. `workspace_id`
/// is hashed for the filename rather than used directly (unlike
/// `terminal.rs`'s canonical-root convention) because this module never
/// needs to walk the directory back to a workspace path — it only needs a
/// stable, filesystem-safe name for whatever opaque identifier the frontend
/// already uses to key a workspace (see `workspace.rs`/`terminalStore.ts`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PrivacyPolicy {
    pub workspace_id: String,
    /// Every `SensitiveDataKind` is expected to have an entry; a kind
    /// missing from a loaded/older file is treated as `Block` (fail closed —
    /// see `PrivacyPolicy::action_for`) and backfilled on load (see
    /// `load_policy_impl`) so the settings editor always has a complete map
    /// to render.
    pub actions: BTreeMap<SensitiveDataKind, PrivacyPolicyAction>,
    /// When true, a `Block`/`RequireApproval` verdict against a `CloudModel`
    /// destination should offer "switch to a local-only model" as an
    /// alternative to cancelling the send outright — surfaced by the
    /// frontend, never decided here.
    pub local_only_fallback: bool,
    /// Literal, case-sensitive, exact-match strings the user has explicitly
    /// exempted from every future scan (e.g. a known-safe shared support
    /// email). Deliberately compared as an exact match against a finding's
    /// own matched span text — never compiled as a pattern — so this list
    /// can never become a wildcard bypass.
    pub exceptions: Vec<String>,
}

impl PrivacyPolicy {
    /// Private-key, API-credential, and credit-card findings block by
    /// default (the same set `knowledge_pipeline`'s own `RejectSecrets` mode
    /// refuses outright, plus credit cards); email/phone/IP address redact
    /// by default rather than block, since those alone are common and often
    /// necessary context for a cloud model to be useful.
    pub fn default_for(workspace_id: &str) -> Self {
        let mut actions = BTreeMap::new();
        actions.insert(SensitiveDataKind::PrivateKey, PrivacyPolicyAction::Block);
        actions.insert(SensitiveDataKind::ApiCredential, PrivacyPolicyAction::Block);
        actions.insert(SensitiveDataKind::CreditCard, PrivacyPolicyAction::Block);
        actions.insert(SensitiveDataKind::Email, PrivacyPolicyAction::Redact);
        actions.insert(SensitiveDataKind::Phone, PrivacyPolicyAction::Redact);
        actions.insert(SensitiveDataKind::IpAddress, PrivacyPolicyAction::Redact);
        Self {
            workspace_id: workspace_id.to_string(),
            actions,
            local_only_fallback: true,
            exceptions: Vec::new(),
        }
    }

    fn action_for(&self, kind: SensitiveDataKind) -> PrivacyPolicyAction {
        self.actions
            .get(&kind)
            .copied()
            .unwrap_or(PrivacyPolicyAction::Block)
    }
}

/// One scanner finding, carrying the policy action it resolved to. Mirrors
/// `SensitiveFinding` (never re-exports the original text — only
/// `masked_preview`, exactly as bounded/masked as the scanner already makes
/// it) plus the two fields this module adds.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PrivacyFinding {
    pub kind: SensitiveDataKind,
    pub byte_start: usize,
    pub byte_end: usize,
    pub line: u32,
    pub column: u32,
    /// Masked and bounded; never contains the original value — copied
    /// straight from `SensitiveFinding::masked_preview`.
    pub masked_preview: String,
    pub action: PrivacyPolicyAction,
    /// True when this exact matched span text is covered by an explicit
    /// policy exception, so `action` was forced to `Allow` regardless of
    /// `kind`'s configured default.
    pub exempted: bool,
}

/// Result of scanning `content` against one workspace's policy for one
/// destination. Concrete and destination-tagged by construction — there is
/// no code path that produces this struct without real spans, a real
/// `redacted_preview`, and a real `verdict`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PrivacyPreviewReport {
    pub destination: OutboundDestination,
    pub workspace_id: String,
    /// The single strictest action across every non-exempted finding —
    /// `Allow` when `findings` is empty or every finding was exempted.
    pub verdict: PrivacyPolicyAction,
    pub findings: Vec<PrivacyFinding>,
    /// `content` with every finding whose `action` is not `Allow` replaced
    /// by `[REDACTED:<KIND>]`, applied back-to-front by byte offset so
    /// earlier replacements never shift a later span. Findings actioned
    /// `Allow` (including exempted ones) are left as their original text —
    /// this is the exact payload the `Redact` action would actually send,
    /// not a blanket "redact everything" preview.
    pub redacted_preview: String,
    pub original_sha256: String,
    pub local_only_fallback_available: bool,
    pub content_length: usize,
}

fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn build_selective_redaction(content: &str, findings: &[PrivacyFinding]) -> String {
    let mut redacted = content.to_string();
    for finding in findings.iter().rev() {
        if finding.action == PrivacyPolicyAction::Allow {
            continue;
        }
        let replacement = format!("[REDACTED:{}]", finding.kind.label());
        redacted.replace_range(finding.byte_start..finding.byte_end, &replacement);
    }
    redacted
}

/// Pure scan-to-policy mapping — no filesystem or Tauri access. Scans
/// `content` with `SensitiveDataScanner`, resolves each finding's action
/// against `policy` (honoring `policy.exceptions` as an exact-match
/// override to `Allow`), and returns the concrete report described on
/// [`PrivacyPreviewReport`].
pub fn preview_impl(
    content: &str,
    destination: OutboundDestination,
    policy: &PrivacyPolicy,
) -> Result<PrivacyPreviewReport, String> {
    let scanner = SensitiveDataScanner::new().map_err(|error| error.to_string())?;
    let raw_findings = scanner.scan(content);

    let mut findings = Vec::with_capacity(raw_findings.len());
    let mut worst = PrivacyPolicyAction::Allow;
    for finding in raw_findings {
        let span_text = content
            .get(finding.byte_start..finding.byte_end)
            .unwrap_or_default();
        let exempted = policy
            .exceptions
            .iter()
            .any(|exception| !exception.is_empty() && exception == span_text);
        let action = if exempted {
            PrivacyPolicyAction::Allow
        } else {
            policy.action_for(finding.kind)
        };
        if action_severity(action) > action_severity(worst) {
            worst = action;
        }
        findings.push(PrivacyFinding {
            kind: finding.kind,
            byte_start: finding.byte_start,
            byte_end: finding.byte_end,
            line: finding.line,
            column: finding.column,
            masked_preview: finding.masked_preview,
            action,
            exempted,
        });
    }

    let redacted_preview = build_selective_redaction(content, &findings);
    Ok(PrivacyPreviewReport {
        destination,
        workspace_id: policy.workspace_id.clone(),
        verdict: worst,
        findings,
        redacted_preview,
        original_sha256: sha256_hex(content.as_bytes()),
        local_only_fallback_available: policy.local_only_fallback,
        content_length: content.len(),
    })
}

fn workspace_file_stem(workspace_id: &str) -> String {
    sha256_hex(workspace_id.as_bytes())
}

fn policy_path(app_data: &Path, workspace_id: &str) -> PathBuf {
    app_data
        .join(POLICY_DIR)
        .join(format!("{}.json", workspace_file_stem(workspace_id)))
}

/// Loads the persisted policy for `workspace_id`, or a fresh
/// [`PrivacyPolicy::default_for`] when none has ever been saved. Any
/// `SensitiveDataKind` missing from a loaded file (e.g. one saved before a
/// new kind was added) is backfilled from the default policy so callers
/// (the settings editor, `preview_impl`) always see a complete `actions` map
/// — `action_for` would fail closed to `Block` for a missing kind anyway,
/// but backfilling keeps what's persisted and what's shown in sync.
pub fn load_policy_impl(app_data: &Path, workspace_id: &str) -> Result<PrivacyPolicy, String> {
    if workspace_id.trim().is_empty() {
        return Err("Workspace id must not be empty".to_string());
    }
    let path = policy_path(app_data, workspace_id);
    match fs::read(&path) {
        Ok(bytes) => {
            let mut policy: PrivacyPolicy = serde_json::from_slice(&bytes)
                .map_err(|error| format!("Invalid privacy firewall policy: {error}"))?;
            for (kind, action) in PrivacyPolicy::default_for(workspace_id).actions {
                policy.actions.entry(kind).or_insert(action);
            }
            policy.workspace_id = workspace_id.to_string();
            Ok(policy)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            Ok(PrivacyPolicy::default_for(workspace_id))
        }
        Err(error) => Err(format!("Could not read privacy firewall policy: {error}")),
    }
}

/// Atomic temp-file-then-rename write — the exact pattern
/// `automations.rs::save_to` and `security_doctor.rs::atomic_write_private_json`
/// already use for every other app-data JSON file in this crate. The temp
/// file's name embeds a fresh UUID (rather than a fixed `.tmp` suffix like
/// `automations.rs`'s single-file case) because this directory holds one
/// file per workspace and a fixed name would let two concurrent saves for
/// two *different* workspaces collide on the same temp path.
pub fn save_policy_impl(app_data: &Path, policy: &PrivacyPolicy) -> Result<(), String> {
    if policy.workspace_id.trim().is_empty() {
        return Err("Workspace id must not be empty".to_string());
    }
    let dir = app_data.join(POLICY_DIR);
    fs::create_dir_all(&dir)
        .map_err(|error| format!("Could not create the privacy firewall directory: {error}"))?;
    let path = policy_path(app_data, &policy.workspace_id);
    let bytes = serde_json::to_vec_pretty(policy)
        .map_err(|error| format!("Could not serialize privacy firewall policy: {error}"))?;
    let temp = dir.join(format!("privacy-firewall-{}.tmp", Uuid::new_v4().simple()));
    let result = fs::write(&temp, &bytes)
        .map_err(|error| format!("Could not write privacy firewall policy: {error}"))
        .and_then(|()| {
            fs::rename(&temp, &path)
                .map_err(|error| format!("Could not publish privacy firewall policy: {error}"))
        });
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

/// A preview bound to a content digest, awaiting an explicit
/// `RequireApproval` decision — the server-side half of the two-phase
/// pattern, mirroring `m5_delivery`'s `DeliveryStore` preview bookkeeping but
/// kept in memory (an unconfirmed privacy decision has no audit-trail value
/// once it expires, unlike a git mutation).
#[derive(Debug, Clone)]
pub struct PendingPrivacySend {
    destination: OutboundDestination,
    workspace_id: String,
    expires_at_ms: u64,
}

pub type PendingPrivacySends = std::sync::Mutex<HashMap<String, PendingPrivacySend>>;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PrivacySendConfirmation {
    pub digest: String,
    pub confirmation_phrase: String,
    pub report: PrivacyPreviewReport,
    pub expires_at_ms: u64,
}

/// The user's explicit decision for a `RequireApproval` (or, via the same
/// path, a `Block` the user chooses to override) send.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PrivacySendDecision {
    /// Send `report.redacted_preview` instead of the original content.
    SendRedacted,
    /// Send the original content unchanged.
    SendUnredacted,
    /// Send nothing; the caller should fall back to a local-only model or
    /// simply not send this turn.
    Cancel,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PrivacySendResult {
    pub allowed: bool,
    /// The content to actually send — `None` whenever `allowed` is `false`.
    pub content: Option<String>,
}

fn confirmation_phrase(digest: &str) -> String {
    format!("CONFIRM {}", &digest[..12.min(digest.len())])
}

/// Phase 1: scans `content`, records a short-lived pending entry keyed by
/// `content`'s own digest, and returns the confirmation phrase the caller
/// must echo back verbatim to `execute_send_impl`. Calling this twice for
/// the same content simply refreshes the pending entry's expiry — it is not
/// an error to preview the same content more than once.
pub fn prepare_send_impl(
    pending: &PendingPrivacySends,
    content: &str,
    destination: OutboundDestination,
    policy: &PrivacyPolicy,
    now_ms: u64,
) -> Result<PrivacySendConfirmation, String> {
    let report = preview_impl(content, destination, policy)?;
    let digest = sha256_hex(content.as_bytes());
    let expires_at_ms = now_ms
        .checked_add(PENDING_SEND_TTL_MS)
        .ok_or_else(|| "Confirmation expiry overflow".to_string())?;
    {
        let mut guard = pending
            .lock()
            .map_err(|_| "Pending privacy sends lock was poisoned".to_string())?;
        guard.retain(|_, entry| entry.expires_at_ms > now_ms);
        guard.insert(
            digest.clone(),
            PendingPrivacySend {
                destination,
                workspace_id: policy.workspace_id.clone(),
                expires_at_ms,
            },
        );
    }
    Ok(PrivacySendConfirmation {
        confirmation_phrase: confirmation_phrase(&digest),
        digest,
        report,
        expires_at_ms,
    })
}

/// Phase 2: validates the confirmation phrase and digest against the pending
/// entry `prepare_send_impl` created, consumes it (single-use — a second
/// `execute_send_impl` call with the same digest fails with "expired or
/// already decided"), and returns exactly what the caller should send.
#[allow(clippy::too_many_arguments)]
pub fn execute_send_impl(
    pending: &PendingPrivacySends,
    content: &str,
    digest: &str,
    confirmation: &str,
    decision: PrivacySendDecision,
    destination: OutboundDestination,
    policy: &PrivacyPolicy,
    now_ms: u64,
) -> Result<PrivacySendResult, String> {
    if confirmation != confirmation_phrase(digest) {
        return Err("Type the exact confirmation phrase shown in the preview".to_string());
    }
    let actual_digest = sha256_hex(content.as_bytes());
    if actual_digest != digest {
        return Err(
            "The confirmation digest does not match the exact content that was previewed"
                .to_string(),
        );
    }
    {
        let mut guard = pending
            .lock()
            .map_err(|_| "Pending privacy sends lock was poisoned".to_string())?;
        let entry = guard.remove(digest).ok_or_else(|| {
            "This preview has expired or was already decided — request a new preview".to_string()
        })?;
        if entry.expires_at_ms <= now_ms {
            return Err("This preview has expired — request a new preview".to_string());
        }
        if entry.destination != destination || entry.workspace_id != policy.workspace_id {
            return Err(
                "The confirmation does not match the original preview's destination or workspace"
                    .to_string(),
            );
        }
    }
    match decision {
        PrivacySendDecision::Cancel => Ok(PrivacySendResult {
            allowed: false,
            content: None,
        }),
        PrivacySendDecision::SendUnredacted => Ok(PrivacySendResult {
            allowed: true,
            content: Some(content.to_string()),
        }),
        PrivacySendDecision::SendRedacted => {
            let report = preview_impl(content, destination, policy)?;
            Ok(PrivacySendResult {
                allowed: true,
                content: Some(report.redacted_preview),
            })
        }
    }
}

fn now_ms() -> Result<u64, String> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .map_err(|error| error.to_string())
}

fn app_data_dir() -> Result<PathBuf, String> {
    crate::app_paths::data_dir()
        .ok_or_else(|| "Could not resolve the application data directory".to_string())
}

#[tauri::command]
pub fn privacy_firewall_get_policy(workspace_id: String) -> Result<PrivacyPolicy, String> {
    load_policy_impl(&app_data_dir()?, &workspace_id)
}

#[tauri::command]
pub fn privacy_firewall_save_policy(
    state: tauri::State<'_, AppState>,
    policy: PrivacyPolicy,
) -> Result<PrivacyPolicy, String> {
    let _guard = state
        .privacy_firewall_lock
        .lock()
        .map_err(|_| "Privacy firewall lock was poisoned".to_string())?;
    save_policy_impl(&app_data_dir()?, &policy)?;
    Ok(policy)
}

#[tauri::command]
pub fn privacy_firewall_preview(
    content: String,
    destination: OutboundDestination,
    workspace_id: String,
) -> Result<PrivacyPreviewReport, String> {
    let policy = load_policy_impl(&app_data_dir()?, &workspace_id)?;
    preview_impl(&content, destination, &policy)
}

#[tauri::command]
pub fn privacy_firewall_prepare_send(
    state: tauri::State<'_, AppState>,
    content: String,
    destination: OutboundDestination,
    workspace_id: String,
) -> Result<PrivacySendConfirmation, String> {
    let policy = load_policy_impl(&app_data_dir()?, &workspace_id)?;
    prepare_send_impl(
        &state.pending_privacy_sends,
        &content,
        destination,
        &policy,
        now_ms()?,
    )
}

#[tauri::command]
pub fn privacy_firewall_execute_send(
    state: tauri::State<'_, AppState>,
    content: String,
    digest: String,
    confirmation: String,
    decision: PrivacySendDecision,
    destination: OutboundDestination,
    workspace_id: String,
) -> Result<PrivacySendResult, String> {
    let policy = load_policy_impl(&app_data_dir()?, &workspace_id)?;
    execute_send_impl(
        &state.pending_privacy_sends,
        &content,
        &digest,
        &confirmation,
        decision,
        destination,
        &policy,
        now_ms()?,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestDir(PathBuf);

    impl TestDir {
        fn new(label: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "little-monkey-privacy-firewall-{label}-{}",
                Uuid::new_v4().simple()
            ));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn policy_json_round_trips_through_save_and_load() {
        let temp = TestDir::new("round-trip");
        let mut policy = PrivacyPolicy::default_for("workspace-alpha");
        policy
            .actions
            .insert(SensitiveDataKind::Email, PrivacyPolicyAction::Allow);
        policy.local_only_fallback = false;
        policy.exceptions.push("support@example.com".to_string());

        save_policy_impl(&temp.0, &policy).expect("save should succeed");
        let loaded = load_policy_impl(&temp.0, "workspace-alpha").expect("load should succeed");

        assert_eq!(loaded, policy);
    }

    #[test]
    fn load_without_a_saved_file_returns_the_default_policy() {
        let temp = TestDir::new("default");
        let loaded = load_policy_impl(&temp.0, "never-saved").expect("load should succeed");
        assert_eq!(loaded, PrivacyPolicy::default_for("never-saved"));
    }

    #[test]
    fn load_backfills_a_kind_missing_from_an_older_saved_file() {
        let temp = TestDir::new("backfill");
        let mut policy = PrivacyPolicy::default_for("workspace-legacy");
        // Simulate a policy saved before some kind existed.
        policy.actions.remove(&SensitiveDataKind::IpAddress);
        save_policy_impl(&temp.0, &policy).expect("save should succeed");

        let loaded = load_policy_impl(&temp.0, "workspace-legacy").expect("load should succeed");
        assert_eq!(
            loaded.actions.get(&SensitiveDataKind::IpAddress),
            Some(&PrivacyPolicyAction::Redact)
        );
    }

    #[test]
    fn save_is_atomic_and_leaves_no_temp_file_behind() {
        let temp = TestDir::new("atomic");
        let policy = PrivacyPolicy::default_for("workspace-atomic");
        save_policy_impl(&temp.0, &policy).expect("save should succeed");

        let dir = temp.0.join(POLICY_DIR);
        let entries: Vec<_> = fs::read_dir(&dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().to_string())
            .collect();
        assert_eq!(
            entries.len(),
            1,
            "expected exactly one published file, got {entries:?}"
        );
        assert!(!entries[0].ends_with(".tmp"));

        let saved: PrivacyPolicy =
            serde_json::from_slice(&fs::read(dir.join(&entries[0])).unwrap()).unwrap();
        assert_eq!(saved, policy);
    }

    #[test]
    fn save_and_load_reject_an_empty_workspace_id() {
        let temp = TestDir::new("empty-id");
        let policy = PrivacyPolicy::default_for("");
        assert!(save_policy_impl(&temp.0, &policy).is_err());
        assert!(load_policy_impl(&temp.0, "").is_err());
    }

    /// Content carrying one clear sample of every `SensitiveDataKind`,
    /// used by the mapping test below. The credit card number is a
    /// Luhn-valid test PAN (the well-known "4111 1111 1111 1111" test
    /// number), matching `SensitiveDataScanner::scan`'s own Luhn gate.
    // The api_key literal is split so secret scanners don't flag the fixture.
    const ALL_KINDS_SAMPLE: &str = concat!(
        "-----BEGIN RSA PRIVATE KEY-----\nMIIBOgIBAAJBAK\n-----END RSA PRIVATE KEY-----\n",
        "api_key: sk-",
        "abcdefghijklmnop12345\n",
        "contact me at person@example.com\n",
        "card 4111 1111 1111 1111\n",
        "call 415-555-0100\n",
        "server at 203.0.113.10\n"
    );

    fn policy_with_action_per_kind(action: PrivacyPolicyAction) -> PrivacyPolicy {
        let mut policy = PrivacyPolicy::default_for("workspace-mapping");
        for kind in [
            SensitiveDataKind::PrivateKey,
            SensitiveDataKind::ApiCredential,
            SensitiveDataKind::Email,
            SensitiveDataKind::CreditCard,
            SensitiveDataKind::Phone,
            SensitiveDataKind::IpAddress,
        ] {
            policy.actions.insert(kind, action);
        }
        policy
    }

    #[test]
    fn every_sensitive_kind_maps_to_its_configured_policy_action() {
        for action in [
            PrivacyPolicyAction::Allow,
            PrivacyPolicyAction::Redact,
            PrivacyPolicyAction::Block,
            PrivacyPolicyAction::RequireApproval,
        ] {
            let policy = policy_with_action_per_kind(action);
            let report = preview_impl(ALL_KINDS_SAMPLE, OutboundDestination::CloudModel, &policy)
                .expect("scan should succeed");

            let found_kinds: std::collections::BTreeSet<_> =
                report.findings.iter().map(|finding| finding.kind).collect();
            for kind in [
                SensitiveDataKind::PrivateKey,
                SensitiveDataKind::ApiCredential,
                SensitiveDataKind::Email,
                SensitiveDataKind::CreditCard,
                SensitiveDataKind::Phone,
                SensitiveDataKind::IpAddress,
            ] {
                assert!(found_kinds.contains(&kind), "expected a {kind:?} finding");
            }
            for finding in &report.findings {
                assert_eq!(
                    finding.action, action,
                    "kind {:?} did not map to {action:?}",
                    finding.kind
                );
            }
            assert_eq!(report.verdict, action);
        }
    }

    #[test]
    fn an_exact_exception_forces_a_finding_to_allow_regardless_of_kind_action() {
        let mut policy = policy_with_action_per_kind(PrivacyPolicyAction::Block);
        policy.exceptions.push("person@example.com".to_string());
        let report = preview_impl(ALL_KINDS_SAMPLE, OutboundDestination::CloudModel, &policy)
            .expect("scan should succeed");

        let email_finding = report
            .findings
            .iter()
            .find(|finding| finding.kind == SensitiveDataKind::Email)
            .expect("an email finding should exist");
        assert!(email_finding.exempted);
        assert_eq!(email_finding.action, PrivacyPolicyAction::Allow);
        // The exempted email is intentionally not redacted in the preview —
        // only its allowed pass-through text should appear.
        assert!(report.redacted_preview.contains("person@example.com"));
        // Every other (blocked) kind must still be redacted.
        assert!(!report.redacted_preview.contains("4111 1111 1111 1111"));
    }

    #[test]
    fn redaction_never_leaks_the_original_span_in_the_returned_preview() {
        let policy = policy_with_action_per_kind(PrivacyPolicyAction::Block);
        let report = preview_impl(ALL_KINDS_SAMPLE, OutboundDestination::CloudModel, &policy)
            .expect("scan should succeed");

        for finding in &report.findings {
            let original_span = &ALL_KINDS_SAMPLE[finding.byte_start..finding.byte_end];
            assert!(
                !report.redacted_preview.contains(original_span),
                "redacted preview leaked the original span for {:?}",
                finding.kind
            );
            // The whole serialized report — not just the preview string —
            // must never carry the raw span either, since this is what
            // crosses the IPC boundary to the frontend verbatim.
            let serialized = serde_json::to_string(&report).unwrap();
            assert!(
                !serialized.contains(original_span),
                "serialized report leaked the original span for {:?}",
                finding.kind
            );
        }
    }

    #[test]
    fn content_with_no_findings_allows_with_an_empty_findings_list() {
        let policy = policy_with_action_per_kind(PrivacyPolicyAction::Block);
        let report = preview_impl(
            "nothing sensitive here",
            OutboundDestination::CloudModel,
            &policy,
        )
        .expect("scan should succeed");
        assert_eq!(report.verdict, PrivacyPolicyAction::Allow);
        assert!(report.findings.is_empty());
        assert_eq!(report.redacted_preview, "nothing sensitive here");
    }

    #[test]
    fn prepare_then_execute_send_redacted_returns_the_redacted_content() {
        let pending: PendingPrivacySends = std::sync::Mutex::new(HashMap::new());
        let policy = policy_with_action_per_kind(PrivacyPolicyAction::RequireApproval);
        let confirmation = prepare_send_impl(
            &pending,
            ALL_KINDS_SAMPLE,
            OutboundDestination::CloudModel,
            &policy,
            1_000,
        )
        .expect("prepare should succeed");

        let result = execute_send_impl(
            &pending,
            ALL_KINDS_SAMPLE,
            &confirmation.digest,
            &confirmation.confirmation_phrase,
            PrivacySendDecision::SendRedacted,
            OutboundDestination::CloudModel,
            &policy,
            1_500,
        )
        .expect("execute should succeed");

        assert!(result.allowed);
        let sent = result.content.expect("redacted content expected");
        assert!(!sent.contains("4111 1111 1111 1111"));
        assert!(sent.contains("[REDACTED:CREDIT_CARD]"));
    }

    #[test]
    fn execute_send_rejects_a_wrong_confirmation_phrase() {
        let pending: PendingPrivacySends = std::sync::Mutex::new(HashMap::new());
        let policy = PrivacyPolicy::default_for("workspace-confirm");
        let confirmation = prepare_send_impl(
            &pending,
            "hello world",
            OutboundDestination::CloudModel,
            &policy,
            1_000,
        )
        .expect("prepare should succeed");

        let result = execute_send_impl(
            &pending,
            "hello world",
            &confirmation.digest,
            "CONFIRM wrong",
            PrivacySendDecision::SendUnredacted,
            OutboundDestination::CloudModel,
            &policy,
            1_500,
        );
        assert!(result.is_err());
    }

    #[test]
    fn execute_send_rejects_content_that_does_not_match_the_previewed_digest() {
        let pending: PendingPrivacySends = std::sync::Mutex::new(HashMap::new());
        let policy = PrivacyPolicy::default_for("workspace-mismatch");
        let confirmation = prepare_send_impl(
            &pending,
            "hello world",
            OutboundDestination::CloudModel,
            &policy,
            1_000,
        )
        .expect("prepare should succeed");

        let result = execute_send_impl(
            &pending,
            "a different message entirely",
            &confirmation.digest,
            &confirmation.confirmation_phrase,
            PrivacySendDecision::SendUnredacted,
            OutboundDestination::CloudModel,
            &policy,
            1_500,
        );
        assert!(result.is_err());
    }

    #[test]
    fn execute_send_rejects_an_expired_pending_entry() {
        let pending: PendingPrivacySends = std::sync::Mutex::new(HashMap::new());
        let policy = PrivacyPolicy::default_for("workspace-expiry");
        let confirmation = prepare_send_impl(
            &pending,
            "hello world",
            OutboundDestination::CloudModel,
            &policy,
            1_000,
        )
        .expect("prepare should succeed");

        let far_future = confirmation.expires_at_ms + 1;
        let result = execute_send_impl(
            &pending,
            "hello world",
            &confirmation.digest,
            &confirmation.confirmation_phrase,
            PrivacySendDecision::SendUnredacted,
            OutboundDestination::CloudModel,
            &policy,
            far_future,
        );
        assert!(result.is_err());
    }

    #[test]
    fn execute_send_cancel_never_returns_content() {
        let pending: PendingPrivacySends = std::sync::Mutex::new(HashMap::new());
        let policy = PrivacyPolicy::default_for("workspace-cancel");
        let confirmation = prepare_send_impl(
            &pending,
            "hello world",
            OutboundDestination::CloudModel,
            &policy,
            1_000,
        )
        .expect("prepare should succeed");

        let result = execute_send_impl(
            &pending,
            "hello world",
            &confirmation.digest,
            &confirmation.confirmation_phrase,
            PrivacySendDecision::Cancel,
            OutboundDestination::CloudModel,
            &policy,
            1_500,
        )
        .expect("execute should succeed");
        assert!(!result.allowed);
        assert!(result.content.is_none());
    }
}
