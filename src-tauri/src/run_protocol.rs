//! Platform-neutral, versioned execution contract shared by desktop, CLI,
//! ACP, scheduler, daemon, workflow, and remote-runner clients.
//!
//! This module deliberately contains no Tauri types and performs no I/O. It
//! describes immutable run-time snapshots plus the append-only events a
//! durable run ledger records. Credentials are represented only by opaque
//! keychain/secret-store reference ids; raw credential fields are not part of
//! the schema, and `deny_unknown_fields` prevents accidentally accepting them.

use std::collections::HashSet;
use std::error::Error;
use std::fmt;

use serde::{Deserialize, Serialize};

/// Current wire/storage version for [`RunSpec`] and [`RunEventEnvelope`].
pub const RUN_PROTOCOL_SCHEMA_VERSION: u32 = 1;

pub const MAX_ID_BYTES: usize = 128;
pub const MAX_IDEMPOTENCY_KEY_BYTES: usize = 256;
pub const MAX_LABEL_BYTES: usize = 1_024;
pub const MAX_PATH_BYTES: usize = 4_096;
pub const MAX_ENDPOINT_BYTES: usize = 2_048;
pub const MAX_TASK_BYTES: usize = 1_048_576;
pub const MAX_INSTRUCTIONS_BYTES: usize = 262_144;
pub const MAX_EVENT_TEXT_BYTES: usize = 65_536;
pub const MAX_EVENT_JSON_BYTES: usize = 262_144;
pub const MAX_ROOT_GRANTS: usize = 32;
pub const MAX_POLICY_RULES: usize = 256;
pub const MAX_REFERENCES_PER_EVENT: usize = 256;
pub const MAX_FIM_TEMPLATE_BYTES: usize = 16_384;
pub const MAX_FIM_TOKEN_BYTES: usize = 256;
pub const MAX_FIM_STOP_TOKENS: usize = 32;

const MAX_WALL_TIME_MS: u64 = 7 * 24 * 60 * 60 * 1_000;
const MAX_ITERATIONS: u32 = 10_000;
const MAX_CALLS: u32 = 100_000;
const MAX_TOKENS: u64 = 1_000_000_000;
const MAX_ARTIFACT_BYTES: u64 = 1 << 40;
const MAX_EVENT_COUNT: u64 = 10_000_000;

/// A field-addressed validation error suitable for API responses and tests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProtocolValidationError {
    pub field: String,
    pub message: String,
}

impl ProtocolValidationError {
    fn new(field: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            field: field.into(),
            message: message.into(),
        }
    }
}

impl fmt::Display for ProtocolValidationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.field, self.message)
    }
}

impl Error for ProtocolValidationError {}

pub type ProtocolValidationResult = Result<(), ProtocolValidationError>;

/// Reject any schema not understood by this binary. Future versions must be
/// migrated or negotiated explicitly rather than silently partially parsed.
pub fn validate_schema_version(version: u32) -> ProtocolValidationResult {
    if version == RUN_PROTOCOL_SCHEMA_VERSION {
        Ok(())
    } else {
        Err(ProtocolValidationError::new(
            "schema_version",
            format!(
                "unsupported run protocol version {version}; expected {RUN_PROTOCOL_SCHEMA_VERSION}"
            ),
        ))
    }
}

/// Validate an opaque protocol identifier without requiring a UUID crate.
/// Identifiers are deliberately ASCII and log-safe. They must start and end
/// with an alphanumeric character and may contain `-`, `_`, `.`, or `:` in
/// between.
pub fn validate_protocol_id(field: &str, value: &str) -> ProtocolValidationResult {
    if value.is_empty() || value.len() > MAX_ID_BYTES {
        return Err(ProtocolValidationError::new(
            field,
            format!("must contain 1..={MAX_ID_BYTES} bytes"),
        ));
    }

    let bytes = value.as_bytes();
    if !bytes[0].is_ascii_alphanumeric() || !bytes[bytes.len() - 1].is_ascii_alphanumeric() {
        return Err(ProtocolValidationError::new(
            field,
            "must start and end with an ASCII letter or digit",
        ));
    }

    if !bytes
        .iter()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':'))
    {
        return Err(ProtocolValidationError::new(
            field,
            "contains unsupported characters",
        ));
    }

    Ok(())
}

fn validate_idempotency_key(field: &str, value: &str) -> ProtocolValidationResult {
    if value.is_empty() || value.len() > MAX_IDEMPOTENCY_KEY_BYTES {
        return Err(ProtocolValidationError::new(
            field,
            format!("must contain 1..={MAX_IDEMPOTENCY_KEY_BYTES} bytes"),
        ));
    }
    if !value.bytes().all(|byte| {
        byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':' | b'/')
    }) {
        return Err(ProtocolValidationError::new(
            field,
            "contains unsupported characters",
        ));
    }
    Ok(())
}

fn validate_text(
    field: &str,
    value: &str,
    max_bytes: usize,
    allow_empty: bool,
) -> ProtocolValidationResult {
    if !allow_empty && value.trim().is_empty() {
        return Err(ProtocolValidationError::new(field, "must not be empty"));
    }
    if value.len() > max_bytes {
        return Err(ProtocolValidationError::new(
            field,
            format!("exceeds the {max_bytes}-byte limit"),
        ));
    }
    if value.chars().any(|character| character == '\0') {
        return Err(ProtocolValidationError::new(field, "must not contain NUL"));
    }
    Ok(())
}

fn validate_single_line(field: &str, value: &str, max_bytes: usize) -> ProtocolValidationResult {
    validate_text(field, value, max_bytes, false)?;
    if value.chars().any(char::is_control) {
        return Err(ProtocolValidationError::new(
            field,
            "must not contain control characters",
        ));
    }
    Ok(())
}

fn validate_path(field: &str, value: &str) -> ProtocolValidationResult {
    validate_text(field, value, MAX_PATH_BYTES, false)?;
    if value.chars().any(char::is_control) {
        return Err(ProtocolValidationError::new(
            field,
            "must not contain control characters",
        ));
    }
    Ok(())
}

fn validate_endpoint(field: &str, value: &str) -> ProtocolValidationResult {
    validate_single_line(field, value, MAX_ENDPOINT_BYTES)?;
    let remainder = value
        .strip_prefix("http://")
        .or_else(|| value.strip_prefix("https://"))
        .ok_or_else(|| ProtocolValidationError::new(field, "must use http:// or https://"))?;

    if remainder.is_empty() || value.contains('@') || value.contains('?') || value.contains('#') {
        return Err(ProtocolValidationError::new(
            field,
            "must be an origin-like endpoint without userinfo, query, or fragment",
        ));
    }
    Ok(())
}

fn validate_sha256(field: &str, value: &str) -> ProtocolValidationResult {
    if value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(ProtocolValidationError::new(
            field,
            "must be a 64-character SHA-256 hex digest",
        ));
    }
    Ok(())
}

fn validate_unique_ids(
    field: &str,
    values: &[String],
    max_count: usize,
) -> ProtocolValidationResult {
    if values.len() > max_count {
        return Err(ProtocolValidationError::new(
            field,
            format!("contains more than {max_count} entries"),
        ));
    }

    let mut seen = HashSet::new();
    for value in values {
        validate_protocol_id(field, value)?;
        if !seen.insert(value.as_str()) {
            return Err(ProtocolValidationError::new(
                field,
                "contains a duplicate id",
            ));
        }
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClientKind {
    Desktop,
    Cli,
    Acp,
    Scheduler,
    Daemon,
    Workflow,
    RemoteRunner,
    Test,
}

/// Identifies the client that submitted or emitted part of a run. No bearer
/// token, API key, or transport credential is representable here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClientIdentity {
    pub client_id: String,
    pub instance_id: String,
    pub kind: ClientKind,
    pub version: String,
}

impl ClientIdentity {
    pub fn validate(&self) -> ProtocolValidationResult {
        validate_protocol_id("client.client_id", &self.client_id)?;
        validate_protocol_id("client.instance_id", &self.instance_id)?;
        validate_single_line("client.version", &self.version, 128)
    }
}

/// The four named classes, best-served first.
///
/// Not a synonym for the `priority` integer. Priority is a number any producer
/// can pick; a class is derived from the frozen `RunKind` in the run spec, which
/// is decided at submission by the code path that submitted it and cannot be
/// re-asserted later. That is what makes "interactive" mean something: a desktop
/// turn frozen through `daemon_desktop_turn` is `RunKind::Interactive` because
/// `task.rs` writes that when `recipe.desktop_turn` is present, and a
/// `monkey daemon run <recipe>` batch migration is `RunKind::Workflow` because
/// it is not.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ProcessClass {
    /// Something is blocked on the answer: a person at a desktop turn, an ACP
    /// peer holding a stdio connection, a browser session, a remote controller.
    Interactive,
    /// Submitted work that wants throughput. Nobody is waiting on any individual
    /// turn, but the whole batch finishing sooner is worth something.
    Batch,
    /// Opportunistic. Runs when there is room, and is the first thing asked to
    /// step aside.
    Background,
    /// Housekeeping on a schedule. May always be deferred, because the next
    /// occurrence will come around anyway.
    Maintenance,
}

impl ProcessClass {
    /// Sort rank, lowest first. `Interactive` is 0.
    pub const fn rank(self) -> u32 {
        match self {
            Self::Interactive => 0,
            Self::Batch => 1,
            Self::Background => 2,
            Self::Maintenance => 3,
        }
    }

    pub const fn token(self) -> &'static str {
        match self {
            Self::Interactive => "interactive",
            Self::Batch => "batch",
            Self::Background => "background",
            Self::Maintenance => "maintenance",
        }
    }

    /// This class promoted `steps` levels toward `Interactive`, saturating there.
    pub const fn promoted(self, steps: u32) -> Self {
        match self.rank().saturating_sub(steps) {
            0 => Self::Interactive,
            1 => Self::Batch,
            2 => Self::Background,
            _ => Self::Maintenance,
        }
    }
}

/// The declared class of a run, from its frozen kind and its declared priority.
///
/// The kind decides the class. Priority does **not** promote — that direction is
/// deliberately closed, because a producer that could promote itself by passing
/// `--priority 9` would make `interactive` mean "whoever asked loudest" within a
/// day. Priority orders work *inside* a class, which is what a number is good
/// for.
///
/// A negative priority is the one thing a caller may say about its own class,
/// and it can only demote: the enqueuer explicitly asked to be behind everything
/// else, so it lands in `Background` (never `Maintenance` — that is reserved for
/// work the daemon itself scheduled, which is a different claim).
pub fn classify(kind: &RunKind, priority: i32) -> ProcessClass {
    let declared = match kind {
        RunKind::Interactive | RunKind::Acp | RunKind::Browser | RunKind::RemoteDesktopControl => {
            ProcessClass::Interactive
        }
        RunKind::Workflow
        | RunKind::ComparisonBranch
        | RunKind::ComparisonSynthesis
        | RunKind::CrewMember
        | RunKind::CrewCoordinator
        | RunKind::Sandboxed => ProcessClass::Batch,
        RunKind::Background => ProcessClass::Background,
        RunKind::Scheduled => ProcessClass::Maintenance,
    };
    if priority < 0 && declared.rank() < ProcessClass::Background.rank() {
        return ProcessClass::Background;
    }
    declared
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunKind {
    Interactive,
    ComparisonBranch,
    ComparisonSynthesis,
    CrewMember,
    CrewCoordinator,
    Workflow,
    Scheduled,
    Browser,
    Acp,
    Background,
    /// Evidence ledger for a remote desktop-control session: periodic and
    /// start/stop screenshots recorded as `ArtifactAdded` events (see
    /// `daemon/remote/desktop.rs`).
    RemoteDesktopControl,
    /// A command executed inside a disposable copy of the workspace (see
    /// `sandbox.rs`). Distinct from `Background`: the workspace root grant
    /// for this kind is informational/read provenance only — the run itself
    /// never writes to the real workspace directly, only to its own
    /// ephemeral copy, and any change that should land in the real
    /// workspace requires a separate, explicit promote confirmation.
    Sandboxed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapabilityState {
    Supported,
    Unsupported,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapabilityAssessment {
    pub state: CapabilityState,
    pub evidence: String,
}

/// Optional formatting metadata for a target that explicitly advertises
/// fill-in-the-middle support. Some runtimes accept prefix/suffix fields
/// directly and need no prompt template; token-based runtimes can snapshot
/// all three marker tokens. A partial marker set is rejected.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FimTemplateMetadata {
    pub prompt_template: Option<String>,
    pub prefix_token: Option<String>,
    pub suffix_token: Option<String>,
    pub middle_token: Option<String>,
    pub stop_tokens: Vec<String>,
    pub max_prefix_tokens: Option<u32>,
    pub max_suffix_tokens: Option<u32>,
    pub max_completion_tokens: Option<u32>,
}

impl FimTemplateMetadata {
    fn validate(&self) -> ProtocolValidationResult {
        if let Some(template) = &self.prompt_template {
            validate_text(
                "target.capabilities.fim_metadata.prompt_template",
                template,
                MAX_FIM_TEMPLATE_BYTES,
                false,
            )?;
        }

        let markers = [
            self.prefix_token.as_ref(),
            self.suffix_token.as_ref(),
            self.middle_token.as_ref(),
        ];
        let marker_count = markers.iter().filter(|marker| marker.is_some()).count();
        if marker_count != 0 && marker_count != markers.len() {
            return Err(ProtocolValidationError::new(
                "target.capabilities.fim_metadata",
                "prefix, suffix, and middle tokens must be supplied together",
            ));
        }
        for (field, marker) in [
            (
                "target.capabilities.fim_metadata.prefix_token",
                &self.prefix_token,
            ),
            (
                "target.capabilities.fim_metadata.suffix_token",
                &self.suffix_token,
            ),
            (
                "target.capabilities.fim_metadata.middle_token",
                &self.middle_token,
            ),
        ] {
            if let Some(marker) = marker {
                validate_text(field, marker, MAX_FIM_TOKEN_BYTES, false)?;
            }
        }

        if self.prompt_template.is_none() && marker_count == 0 {
            return Err(ProtocolValidationError::new(
                "target.capabilities.fim_metadata",
                "must include a prompt template or a complete marker-token set",
            ));
        }

        if self.stop_tokens.len() > MAX_FIM_STOP_TOKENS {
            return Err(ProtocolValidationError::new(
                "target.capabilities.fim_metadata.stop_tokens",
                format!("contains more than {MAX_FIM_STOP_TOKENS} entries"),
            ));
        }
        let mut stop_tokens = HashSet::new();
        for token in &self.stop_tokens {
            validate_text(
                "target.capabilities.fim_metadata.stop_tokens",
                token,
                MAX_FIM_TOKEN_BYTES,
                false,
            )?;
            if !stop_tokens.insert(token.as_str()) {
                return Err(ProtocolValidationError::new(
                    "target.capabilities.fim_metadata.stop_tokens",
                    "contains a duplicate token",
                ));
            }
        }

        for (field, value) in [
            (
                "target.capabilities.fim_metadata.max_prefix_tokens",
                self.max_prefix_tokens,
            ),
            (
                "target.capabilities.fim_metadata.max_suffix_tokens",
                self.max_suffix_tokens,
            ),
            (
                "target.capabilities.fim_metadata.max_completion_tokens",
                self.max_completion_tokens,
            ),
        ] {
            if let Some(value) = value {
                validate_range_u64(field, u64::from(value), 1, MAX_TOKENS)?;
            }
        }
        Ok(())
    }
}

impl CapabilityAssessment {
    fn validate(&self, field: &str) -> ProtocolValidationResult {
        validate_single_line(field, &self.evidence, MAX_LABEL_BYTES)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelCapabilitiesSnapshot {
    pub tool_calling: CapabilityAssessment,
    pub vision: CapabilityAssessment,
    pub embeddings: CapabilityAssessment,
    pub structured_output: CapabilityAssessment,
    pub image_generation: CapabilityAssessment,
    pub audio: CapabilityAssessment,
    pub runtime_lifecycle: CapabilityAssessment,
    pub fim: CapabilityAssessment,
    pub code_completion: CapabilityAssessment,
    pub inline_edit: CapabilityAssessment,
    pub fim_metadata: Option<FimTemplateMetadata>,
}

impl ModelCapabilitiesSnapshot {
    fn validate(&self) -> ProtocolValidationResult {
        for (field, capability) in [
            ("target.capabilities.tool_calling", &self.tool_calling),
            ("target.capabilities.vision", &self.vision),
            ("target.capabilities.embeddings", &self.embeddings),
            (
                "target.capabilities.structured_output",
                &self.structured_output,
            ),
            (
                "target.capabilities.image_generation",
                &self.image_generation,
            ),
            ("target.capabilities.audio", &self.audio),
            (
                "target.capabilities.runtime_lifecycle",
                &self.runtime_lifecycle,
            ),
            ("target.capabilities.fim", &self.fim),
            ("target.capabilities.code_completion", &self.code_completion),
            ("target.capabilities.inline_edit", &self.inline_edit),
        ] {
            capability.validate(field)?;
        }
        if let Some(metadata) = &self.fim_metadata {
            if self.fim.state != CapabilityState::Supported {
                return Err(ProtocolValidationError::new(
                    "target.capabilities.fim_metadata",
                    "metadata requires FIM capability state 'supported'",
                ));
            }
            metadata.validate()?;
        }
        Ok(())
    }
}

/// Immutable target snapshot captured when the run is submitted. Provider
/// credentials are represented only by `credential_ref_id`, an opaque local
/// secret-store reference. Endpoints reject embedded userinfo/query strings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ModelTargetSnapshot {
    ManagedLlama {
        target_id: String,
        label: String,
        model_id: String,
        model_path: String,
        capabilities: ModelCapabilitiesSnapshot,
        estimated_memory_bytes: Option<u64>,
    },
    Ollama {
        target_id: String,
        label: String,
        base_url: String,
        model: String,
        is_cloud: bool,
        capabilities: ModelCapabilitiesSnapshot,
        estimated_memory_bytes: Option<u64>,
    },
    Provider {
        target_id: String,
        label: String,
        provider_id: String,
        endpoint: String,
        model: String,
        credential_ref_id: String,
        capabilities: ModelCapabilitiesSnapshot,
    },
}

impl ModelTargetSnapshot {
    pub fn target_id(&self) -> &str {
        match self {
            Self::ManagedLlama { target_id, .. }
            | Self::Ollama { target_id, .. }
            | Self::Provider { target_id, .. } => target_id,
        }
    }

    pub fn validate(&self) -> ProtocolValidationResult {
        match self {
            Self::ManagedLlama {
                target_id,
                label,
                model_id,
                model_path,
                capabilities,
                estimated_memory_bytes,
            } => {
                validate_protocol_id("target.target_id", target_id)?;
                validate_single_line("target.label", label, MAX_LABEL_BYTES)?;
                validate_single_line("target.model_id", model_id, MAX_LABEL_BYTES)?;
                validate_path("target.model_path", model_path)?;
                validate_optional_positive(
                    "target.estimated_memory_bytes",
                    *estimated_memory_bytes,
                )?;
                capabilities.validate()
            }
            Self::Ollama {
                target_id,
                label,
                base_url,
                model,
                is_cloud: _,
                capabilities,
                estimated_memory_bytes,
            } => {
                validate_protocol_id("target.target_id", target_id)?;
                validate_single_line("target.label", label, MAX_LABEL_BYTES)?;
                validate_endpoint("target.base_url", base_url)?;
                validate_single_line("target.model", model, MAX_LABEL_BYTES)?;
                validate_optional_positive(
                    "target.estimated_memory_bytes",
                    *estimated_memory_bytes,
                )?;
                capabilities.validate()
            }
            Self::Provider {
                target_id,
                label,
                provider_id,
                endpoint,
                model,
                credential_ref_id,
                capabilities,
            } => {
                validate_protocol_id("target.target_id", target_id)?;
                validate_single_line("target.label", label, MAX_LABEL_BYTES)?;
                validate_protocol_id("target.provider_id", provider_id)?;
                validate_endpoint("target.endpoint", endpoint)?;
                validate_single_line("target.model", model, MAX_LABEL_BYTES)?;
                validate_protocol_id("target.credential_ref_id", credential_ref_id)?;
                capabilities.validate()
            }
        }
    }
}

fn validate_optional_positive(field: &str, value: Option<u64>) -> ProtocolValidationResult {
    if value == Some(0) {
        Err(ProtocolValidationError::new(
            field,
            "must be positive when present",
        ))
    } else {
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RootAccess {
    ReadOnly,
    ReadWrite,
}

/// Canonical root authorization frozen at run creation. Path canonicalization
/// and platform-specific comparison happen before this snapshot is created.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RootGrant {
    pub root_id: String,
    pub canonical_path: String,
    pub access: RootAccess,
    pub allow_symlinks_within_root: bool,
}

impl RootGrant {
    pub fn validate(&self) -> ProtocolValidationResult {
        validate_protocol_id("workspace.roots.root_id", &self.root_id)?;
        validate_path("workspace.roots.canonical_path", &self.canonical_path)
    }
}

/// Default-deny repository mutation policy frozen for the run. Remote names
/// are stored instead of remote URLs so credentials embedded in URLs cannot
/// enter the protocol snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RepositoryPolicy {
    pub root_id: String,
    pub owned_worktree_required: bool,
    pub allowed_remote_names: Vec<String>,
    pub allowed_branch_prefixes: Vec<String>,
    pub allow_commit: bool,
    pub allow_push: bool,
    pub allow_create_pull_request: bool,
    pub allow_review_comment: bool,
    pub allow_merge: bool,
    pub allow_force_push: bool,
}

impl RepositoryPolicy {
    pub fn validate(&self) -> ProtocolValidationResult {
        validate_protocol_id("workspace.repository_policy.root_id", &self.root_id)?;
        if self.allowed_remote_names.len() > 32 {
            return Err(ProtocolValidationError::new(
                "workspace.repository_policy.allowed_remote_names",
                "contains more than 32 entries",
            ));
        }
        let mut remotes = HashSet::new();
        for remote in &self.allowed_remote_names {
            validate_protocol_id("workspace.repository_policy.allowed_remote_names", remote)?;
            if !remotes.insert(remote.as_str()) {
                return Err(ProtocolValidationError::new(
                    "workspace.repository_policy.allowed_remote_names",
                    "contains a duplicate remote",
                ));
            }
        }

        if self.allowed_branch_prefixes.len() > 64 {
            return Err(ProtocolValidationError::new(
                "workspace.repository_policy.allowed_branch_prefixes",
                "contains more than 64 entries",
            ));
        }
        let mut prefixes = HashSet::new();
        for prefix in &self.allowed_branch_prefixes {
            validate_single_line(
                "workspace.repository_policy.allowed_branch_prefixes",
                prefix,
                256,
            )?;
            if !prefixes.insert(prefix.as_str()) {
                return Err(ProtocolValidationError::new(
                    "workspace.repository_policy.allowed_branch_prefixes",
                    "contains a duplicate prefix",
                ));
            }
        }

        if (self.allow_merge || self.allow_force_push) && !self.allow_push {
            return Err(ProtocolValidationError::new(
                "workspace.repository_policy",
                "merge or force-push cannot be enabled while push is disabled",
            ));
        }
        Ok(())
    }
}

/// Workspace and repository grants captured once at run creation. Engines
/// must use this snapshot instead of rereading whichever workspace is active
/// in a UI later.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceContext {
    pub workspace_id: String,
    pub primary_root_id: String,
    pub roots: Vec<RootGrant>,
    pub repository_policy: Option<RepositoryPolicy>,
}

impl WorkspaceContext {
    pub fn validate(&self) -> ProtocolValidationResult {
        validate_protocol_id("workspace.workspace_id", &self.workspace_id)?;
        validate_protocol_id("workspace.primary_root_id", &self.primary_root_id)?;

        if self.roots.is_empty() || self.roots.len() > MAX_ROOT_GRANTS {
            return Err(ProtocolValidationError::new(
                "workspace.roots",
                format!("must contain 1..={MAX_ROOT_GRANTS} grants"),
            ));
        }

        let mut root_ids = HashSet::new();
        let mut root_paths = HashSet::new();
        for root in &self.roots {
            root.validate()?;
            if !root_ids.insert(root.root_id.as_str()) {
                return Err(ProtocolValidationError::new(
                    "workspace.roots",
                    "contains a duplicate root_id",
                ));
            }
            if !root_paths.insert(root.canonical_path.as_str()) {
                return Err(ProtocolValidationError::new(
                    "workspace.roots",
                    "contains a duplicate canonical_path",
                ));
            }
        }

        if !root_ids.contains(self.primary_root_id.as_str()) {
            return Err(ProtocolValidationError::new(
                "workspace.primary_root_id",
                "does not reference a granted root",
            ));
        }

        if let Some(policy) = &self.repository_policy {
            policy.validate()?;
            if !root_ids.contains(policy.root_id.as_str()) {
                return Err(ProtocolValidationError::new(
                    "workspace.repository_policy.root_id",
                    "does not reference a granted root",
                ));
            }
        }

        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PermissionMode {
    Manual,
    AcceptEdits,
    Smart,
    Plan,
    Auto,
    Bypass,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolPolicyDecision {
    Allow,
    Prompt,
    Deny,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolPermissionRule {
    pub tool: String,
    pub decision: ToolPolicyDecision,
}

/// Maximum entries in any one dimension of an [`EgressAllowlist`].
///
/// Small on purpose. A declaration is meant to be readable by whoever approves the
/// run; a list of hundreds of hosts is not a policy, it is a shrug.
pub const MAX_EGRESS_ALLOWLIST_ENTRIES: usize = 64;

/// Longest host entry. The DNS limit, so a legal name always fits and a 40 KB
/// "host" cannot be frozen into a spec.
pub const MAX_EGRESS_HOST_BYTES: usize = 253;

/// Where a run may send a request: allowed hosts, ports and protocols.
///
/// # Absent, empty, and present — the whole safety property is in this distinction
///
/// The field that holds this on [`PermissionPolicySnapshot`] is an `Option`, and the
/// three states are deliberately three:
///
/// - **Absent** (`None`, which is what every already-frozen run row on disk says,
///   because they were written before this field existed): the run declares nothing
///   about hosts, ports or protocols, and `allow_network` alone governs — exactly
///   today's behaviour. Retroactively reading those rows as "deny everything" would
///   refuse every existing and in-flight run, so absence cannot mean deny.
/// - **Present and empty** (`Some` with an empty `hosts`/`ports`/`protocols`): a
///   declaration that permits nothing. Deny-all, apart from the loopback exemption
///   below. This is the *point* of the shape: a submitter that wants a run with no
///   egress has a way to say so, and it is not spelled the same way as saying
///   nothing.
/// - **Present and populated**: deny-by-default *within* the declaration. A host,
///   port or protocol that is not named is refused, and the three dimensions are
///   conjunctive — a request must satisfy all three.
///
/// # Making a declaration mandatory is a later release's job
///
/// The same staged shape D1 used for token families: the mechanism lands first and
/// is honoured wherever it is declared, and only once submitters have been converted
/// can absence become an error. Doing it in one step would mean either rewriting
/// frozen specs — they are frozen for a reason, and no migration here will touch
/// them — or refusing every run submitted by a build that predates the field.
///
/// # What it does not cover
///
/// Loopback. A local-inference run legitimately talks to `127.0.0.1`, and reading a
/// network policy as "no sockets at all" is not a stricter policy but a broken one —
/// the same exemption, for the same reason, as
/// [`crate::egress::is_loopback_target`]'s.
///
/// Each dimension must be spelled out when the allowlist is present: the three
/// fields carry no serde default, so `{"hosts": ["api.example.com"]}` is a
/// deserialization error rather than a silent deny-all on ports. Absence is a
/// statement at the allowlist level and nowhere else.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EgressAllowlist {
    /// Hostnames, or `*.example.com` for "any subdomain of `example.com`".
    ///
    /// A wildcard matches at a label boundary only and never the apex, so
    /// `*.example.com` covers `api.example.com` and does **not** cover
    /// `example.com` (name it as well if it is wanted) or `evil-example.com`. The
    /// matcher is [`crate::egress::allowlist_host_matches`].
    pub hosts: Vec<String>,
    /// Ports, as the request's effective port — the URL's own, or the scheme's
    /// default when it omits one. So a run reaching `https://host/` needs `443`
    /// listed.
    pub ports: Vec<u16>,
    /// URL schemes, lowercase (`https`, `http`).
    pub protocols: Vec<String>,
}

impl EgressAllowlist {
    pub fn validate(&self) -> ProtocolValidationResult {
        for (field, len) in [
            ("hosts", self.hosts.len()),
            ("ports", self.ports.len()),
            ("protocols", self.protocols.len()),
        ] {
            if len > MAX_EGRESS_ALLOWLIST_ENTRIES {
                return Err(ProtocolValidationError::new(
                    format!("permission_policy.egress_allowlist.{field}"),
                    format!("contains more than {MAX_EGRESS_ALLOWLIST_ENTRIES} entries"),
                ));
            }
        }

        for host in &self.hosts {
            validate_egress_host(host)?;
        }
        for port in &self.ports {
            if *port == 0 {
                return Err(ProtocolValidationError::new(
                    "permission_policy.egress_allowlist.ports",
                    "must not contain port 0, which is not a destination",
                ));
            }
        }
        for protocol in &self.protocols {
            validate_egress_protocol(protocol)?;
        }
        Ok(())
    }
}

/// One host entry: a name, or `*.` plus a name.
///
/// Lowercase is required rather than folded, so the matcher can compare a
/// lowercased URL host against these bytes directly and no call site has to
/// remember to fold. Rejecting mixed case at submission is the earliest and
/// loudest place to say so.
fn validate_egress_host(value: &str) -> ProtocolValidationResult {
    const FIELD: &str = "permission_policy.egress_allowlist.hosts";
    let name = value.strip_prefix("*.").unwrap_or(value);
    if name.is_empty() || value.len() > MAX_EGRESS_HOST_BYTES {
        return Err(ProtocolValidationError::new(
            FIELD,
            format!("each entry must name 1..={MAX_EGRESS_HOST_BYTES} bytes of host"),
        ));
    }
    // `:`, `[` and `]` so an IPv6 literal can be named the way `Url::host_str`
    // spells one, brackets included.
    if !name
        .bytes()
        .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || b"-._:[]".contains(&byte))
    {
        return Err(ProtocolValidationError::new(
            FIELD,
            "each entry must be a lowercase host, optionally prefixed with `*.`",
        ));
    }
    if name.starts_with('.') || name.ends_with('.') {
        return Err(ProtocolValidationError::new(
            FIELD,
            "each entry must not start or end with a dot",
        ));
    }
    Ok(())
}

/// One protocol entry: an RFC 3986 scheme, lowercase, no `://`.
fn validate_egress_protocol(value: &str) -> ProtocolValidationResult {
    const FIELD: &str = "permission_policy.egress_allowlist.protocols";
    let mut bytes = value.bytes();
    let valid = bytes.next().is_some_and(|first| first.is_ascii_lowercase())
        && bytes.all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'+' | b'-' | b'.')
        });
    if !valid {
        return Err(ProtocolValidationError::new(
            FIELD,
            "each entry must be a lowercase URL scheme such as `https`",
        ));
    }
    Ok(())
}

/// Run-scoped permission snapshot. It records decisions and secret-free tool
/// policy only; transient approval channel handles and credentials never
/// enter the contract.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PermissionPolicySnapshot {
    pub mode: PermissionMode,
    pub unattended: bool,
    pub approval_timeout_ms: u64,
    pub default_tool_decision: ToolPolicyDecision,
    pub tool_rules: Vec<ToolPermissionRule>,
    pub allow_network: bool,
    pub allow_external_mutations: bool,
    /// The run's frozen host/port/protocol allowlist — see [`EgressAllowlist`] for
    /// what absent, empty and present each mean.
    ///
    /// `default` so every run row written before this field existed still
    /// deserializes, and `skip_serializing_if` so a spec that declares nothing
    /// serializes to the **same bytes** it did before. That second half is not
    /// cosmetic: `run_ledger` compares the serialized spec byte-for-byte to decide
    /// whether a resubmission is the same run, so emitting `"egress_allowlist":null`
    /// would turn every idempotent resubmit of an existing run into a conflict.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub egress_allowlist: Option<EgressAllowlist>,
}

impl PermissionPolicySnapshot {
    pub fn validate(&self) -> ProtocolValidationResult {
        if self.approval_timeout_ms == 0 || self.approval_timeout_ms > 24 * 60 * 60 * 1_000 {
            return Err(ProtocolValidationError::new(
                "permission_policy.approval_timeout_ms",
                "must be between 1 ms and 24 hours",
            ));
        }
        if self.unattended && self.mode == PermissionMode::Bypass {
            return Err(ProtocolValidationError::new(
                "permission_policy.mode",
                "bypass is forbidden for unattended runs",
            ));
        }
        if self.tool_rules.len() > MAX_POLICY_RULES {
            return Err(ProtocolValidationError::new(
                "permission_policy.tool_rules",
                format!("contains more than {MAX_POLICY_RULES} rules"),
            ));
        }

        let mut tools = HashSet::new();
        for rule in &self.tool_rules {
            validate_protocol_id("permission_policy.tool_rules.tool", &rule.tool)?;
            if !tools.insert(rule.tool.as_str()) {
                return Err(ProtocolValidationError::new(
                    "permission_policy.tool_rules",
                    "contains duplicate rules for one tool",
                ));
            }
        }

        if let Some(allowlist) = &self.egress_allowlist {
            allowlist.validate()?;
        }
        Ok(())
    }
}

/// Hard run-level resource limits. Zero is allowed only for `max_tool_calls`,
/// which is how no-tools comparison/synthesis runs are represented.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunBudgets {
    pub wall_time_ms: u64,
    pub max_iterations: u32,
    pub max_model_calls: u32,
    pub max_tool_calls: u32,
    pub max_input_tokens: u64,
    pub max_output_tokens: u64,
    pub max_cost_micros: Option<u64>,
    pub max_artifact_bytes: u64,
    pub max_event_count: u64,
}

impl RunBudgets {
    pub fn validate(&self) -> ProtocolValidationResult {
        validate_range_u64(
            "budgets.wall_time_ms",
            self.wall_time_ms,
            1,
            MAX_WALL_TIME_MS,
        )?;
        validate_range_u64(
            "budgets.max_iterations",
            u64::from(self.max_iterations),
            1,
            u64::from(MAX_ITERATIONS),
        )?;
        validate_range_u64(
            "budgets.max_model_calls",
            u64::from(self.max_model_calls),
            1,
            u64::from(MAX_CALLS),
        )?;
        validate_range_u64(
            "budgets.max_tool_calls",
            u64::from(self.max_tool_calls),
            0,
            u64::from(MAX_CALLS),
        )?;
        validate_range_u64(
            "budgets.max_input_tokens",
            self.max_input_tokens,
            1,
            MAX_TOKENS,
        )?;
        validate_range_u64(
            "budgets.max_output_tokens",
            self.max_output_tokens,
            1,
            MAX_TOKENS,
        )?;
        if self.max_cost_micros == Some(0) {
            return Err(ProtocolValidationError::new(
                "budgets.max_cost_micros",
                "must be positive when present",
            ));
        }
        validate_range_u64(
            "budgets.max_artifact_bytes",
            self.max_artifact_bytes,
            1,
            MAX_ARTIFACT_BYTES,
        )?;
        validate_range_u64(
            "budgets.max_event_count",
            self.max_event_count,
            1,
            MAX_EVENT_COUNT,
        )
    }
}

fn validate_range_u64(field: &str, value: u64, min: u64, max: u64) -> ProtocolValidationResult {
    if value < min || value > max {
        Err(ProtocolValidationError::new(
            field,
            format!("must be between {min} and {max}"),
        ))
    } else {
        Ok(())
    }
}

/// Immutable submission record consumed by every execution surface.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunSpec {
    pub schema_version: u32,
    pub run_id: String,
    pub idempotency_key: String,
    pub created_at_ms: u64,
    pub kind: RunKind,
    pub submitted_by: ClientIdentity,
    pub task: String,
    pub instructions: Option<String>,
    pub input_artifact_ids: Vec<String>,
    pub target: ModelTargetSnapshot,
    /// Absent for model-only chat/comparison runs that have no filesystem,
    /// repository, or workspace tools. Tool execution requiring a root grant
    /// must reject the run unless this snapshot is present.
    pub workspace: Option<WorkspaceContext>,
    pub permission_policy: PermissionPolicySnapshot,
    pub budgets: RunBudgets,
}

impl RunSpec {
    pub fn validate(&self) -> ProtocolValidationResult {
        validate_schema_version(self.schema_version)?;
        validate_protocol_id("run_id", &self.run_id)?;
        validate_idempotency_key("idempotency_key", &self.idempotency_key)?;
        if self.created_at_ms == 0 {
            return Err(ProtocolValidationError::new(
                "created_at_ms",
                "must be a positive Unix timestamp in milliseconds",
            ));
        }
        self.submitted_by.validate()?;
        validate_text("task", &self.task, MAX_TASK_BYTES, false)?;
        if let Some(instructions) = &self.instructions {
            validate_text("instructions", instructions, MAX_INSTRUCTIONS_BYTES, false)?;
        }
        validate_unique_ids(
            "input_artifact_ids",
            &self.input_artifact_ids,
            MAX_REFERENCES_PER_EVENT,
        )?;
        self.target.validate()?;
        if let Some(workspace) = &self.workspace {
            workspace.validate()?;
        }
        self.permission_policy.validate()?;
        self.budgets.validate()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    Queued,
    Running,
    WaitingForPermission,
    Paused,
    Cancelling,
    Succeeded,
    Failed,
    Cancelled,
    NeedsReconciliation,
}

impl RunStatus {
    #[must_use]
    pub const fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Succeeded | Self::Failed | Self::Cancelled | Self::NeedsReconciliation
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutputChannel {
    Assistant,
    Status,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionDecision {
    AllowOnce,
    AllowForRun,
    Deny,
    Expired,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RiskLevel {
    Low,
    Medium,
    High,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolOutcome {
    Succeeded,
    Failed,
    Denied,
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactKind {
    File,
    Document,
    Image,
    Audio,
    Video,
    Archive,
    Report,
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckpointKind {
    Workspace,
    Git,
    Conversation,
    ExternalState,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExternalMutationState {
    Pending,
    Confirmed,
    NeedsReconciliation,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MutationKind {
    Filesystem,
    Git,
    Network,
    ExternalService,
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RedactionState {
    Applied,
    NotNeeded,
}

/// The only arbitrary JSON admitted to events. Its name and explicit state
/// make the producer's redaction responsibility visible, and validation caps
/// the serialized size before ledger insertion or transport.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RedactedPayload {
    pub value: serde_json::Value,
    pub redaction: RedactionState,
}

impl RedactedPayload {
    fn validate(&self, field: &str) -> ProtocolValidationResult {
        let size = serde_json::to_vec(&self.value)
            .map_err(|error| ProtocolValidationError::new(field, error.to_string()))?
            .len();
        if size > MAX_EVENT_JSON_BYTES {
            Err(ProtocolValidationError::new(
                field,
                format!("serialized payload exceeds the {MAX_EVENT_JSON_BYTES}-byte limit"),
            ))
        } else {
            Ok(())
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UsageSnapshot {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cached_input_tokens: u64,
    pub model_calls: u32,
    pub tool_calls: u32,
    pub cost_micros: Option<u64>,
}

impl UsageSnapshot {
    fn validate(&self) -> ProtocolValidationResult {
        if self.input_tokens > MAX_TOKENS
            || self.output_tokens > MAX_TOKENS
            || self.cached_input_tokens > MAX_TOKENS
        {
            return Err(ProtocolValidationError::new(
                "event.usage",
                "token count exceeds the protocol limit",
            ));
        }
        if self.model_calls > MAX_CALLS || self.tool_calls > MAX_CALLS {
            return Err(ProtocolValidationError::new(
                "event.usage",
                "call count exceeds the protocol limit",
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunFailureDetails {
    pub code: String,
    pub message: String,
    pub retryable: bool,
}

impl RunFailureDetails {
    fn validate(&self) -> ProtocolValidationResult {
        validate_protocol_id("result.failure.code", &self.code)?;
        validate_text(
            "result.failure.message",
            &self.message,
            MAX_EVENT_TEXT_BYTES,
            false,
        )
    }
}

/// Details for a terminal result whose external side effect cannot be proven
/// confirmed or absent after interruption. This state is intentionally
/// separate from an ordinary retryable failure: it requires inspection or
/// approval before any retry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunReconciliationDetails {
    pub mutation_id: String,
    pub reason: String,
}

impl RunReconciliationDetails {
    fn validate(&self) -> ProtocolValidationResult {
        validate_protocol_id("result.reconciliation.mutation_id", &self.mutation_id)?;
        validate_text(
            "result.reconciliation.reason",
            &self.reason,
            MAX_EVENT_TEXT_BYTES,
            false,
        )
    }
}

/// Versioned, immutable terminal result derived from a run's append-only
/// event history. It is a projection for clients/exports, not a replacement
/// for the ledger. Status-specific validation prevents contradictory records
/// such as a successful result carrying failure details or a reconciliation
/// result being treated as an ordinary retryable failure.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunResultSnapshot {
    pub schema_version: u32,
    pub run_id: String,
    pub status: RunStatus,
    pub started_at_ms: u64,
    pub finished_at_ms: u64,
    pub usage: UsageSnapshot,
    pub result_artifact_ids: Vec<String>,
    pub summary: Option<String>,
    pub failure: Option<RunFailureDetails>,
    pub reconciliation: Option<RunReconciliationDetails>,
}

impl RunResultSnapshot {
    pub fn validate(&self) -> ProtocolValidationResult {
        validate_schema_version(self.schema_version)?;
        validate_protocol_id("result.run_id", &self.run_id)?;
        validate_positive_timestamp("result.started_at_ms", self.started_at_ms)?;
        validate_positive_timestamp("result.finished_at_ms", self.finished_at_ms)?;
        if self.finished_at_ms < self.started_at_ms {
            return Err(ProtocolValidationError::new(
                "result.finished_at_ms",
                "must not precede started_at_ms",
            ));
        }
        if !self.status.is_terminal() {
            return Err(ProtocolValidationError::new(
                "result.status",
                "RunResultSnapshot accepts terminal statuses only",
            ));
        }

        self.usage.validate()?;
        validate_unique_ids(
            "result.result_artifact_ids",
            &self.result_artifact_ids,
            MAX_REFERENCES_PER_EVENT,
        )?;
        if let Some(summary) = &self.summary {
            validate_text("result.summary", summary, MAX_EVENT_TEXT_BYTES, false)?;
        }
        if let Some(failure) = &self.failure {
            failure.validate()?;
        }
        if let Some(reconciliation) = &self.reconciliation {
            reconciliation.validate()?;
        }

        match self.status {
            RunStatus::Succeeded | RunStatus::Cancelled => {
                if self.failure.is_some() || self.reconciliation.is_some() {
                    return Err(ProtocolValidationError::new(
                        "result.status",
                        "succeeded and cancelled results cannot carry failure or reconciliation details",
                    ));
                }
            }
            RunStatus::Failed => {
                if self.failure.is_none() || self.reconciliation.is_some() {
                    return Err(ProtocolValidationError::new(
                        "result.status",
                        "failed results require failure details and forbid reconciliation details",
                    ));
                }
            }
            RunStatus::NeedsReconciliation => {
                if self.reconciliation.is_none() || self.failure.is_some() {
                    return Err(ProtocolValidationError::new(
                        "result.status",
                        "needs_reconciliation results require reconciliation details and forbid failure details",
                    ));
                }
            }
            RunStatus::Queued
            | RunStatus::Running
            | RunStatus::WaitingForPermission
            | RunStatus::Paused
            | RunStatus::Cancelling => unreachable!("nonterminal status rejected above"),
        }

        Ok(())
    }
}

/// Append-only run events. The names deliberately match the shared durable
/// engine vocabulary used by desktop, CLI, ACP, scheduler, daemon, and remote
/// clients. Terminal outcomes have dedicated variants so the ledger can
/// enforce exactly one terminal event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    content = "payload",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum RunEvent {
    Queued {
        queue: Option<String>,
    },
    Started {
        engine_id: String,
    },
    ModelDelta {
        message_id: String,
        channel: OutputChannel,
        text: String,
    },
    ToolProposed {
        tool_call_id: String,
        tool_name: String,
        arguments: RedactedPayload,
        arguments_sha256: String,
        mutation: bool,
    },
    PermissionRequested {
        request_id: String,
        tool_call_id: String,
        tool_name: String,
        /// SHA-256 of the canonical tool name + canonical arguments + grant
        /// scope. Approval records bind to this digest, never only UI text.
        operation_sha256: String,
        expires_at_ms: u64,
        detail: String,
        risk_level: Option<RiskLevel>,
        risk_reason: Option<String>,
    },
    PermissionDecided {
        request_id: String,
        operation_sha256: String,
        decision: PermissionDecision,
        decided_by: ClientIdentity,
    },
    /// Which dispatch policy chose this run's target, and why (roadmap K9).
    ///
    /// The run's frozen `ModelTargetSnapshot` already records *what* ran. This
    /// records *why* it was that one, on the same append-only, hash-chained
    /// stream — so "which policy chose this run's target" survives a restart
    /// instead of living only in the transcript and in session state.
    ///
    /// Every field is nullable where the honest answer is "no policy": a fresh
    /// profile has none, and `reason` still says so rather than leaving the
    /// event to be read as an absence.
    RoutingDecided {
        task_class: String,
        policy_id: Option<String>,
        policy_name: Option<String>,
        /// The `ModelTargetSnapshot` key that won, or `None` when the caller
        /// keeps its active target.
        chosen_key: Option<String>,
        /// False when a policy applied and the active target already satisfied
        /// it — the steady state of a working conversation.
        changed_from_active: bool,
        reason: String,
    },
    ToolStarted {
        tool_call_id: String,
    },
    ToolFinished {
        tool_call_id: String,
        outcome: ToolOutcome,
        output_excerpt: Option<String>,
        output_sha256: Option<String>,
        duration_ms: u64,
    },
    ArtifactAdded {
        artifact_id: String,
        kind: ArtifactKind,
        name: String,
        media_type: String,
        content_sha256: String,
        size_bytes: u64,
    },
    CheckpointLinked {
        checkpoint_id: String,
        kind: CheckpointKind,
        label: String,
        content_sha256: Option<String>,
    },
    VerificationFinished {
        verification_id: String,
        name: String,
        passed: bool,
        summary: String,
        artifact_ids: Vec<String>,
        duration_ms: u64,
    },
    UsageRecorded {
        usage: UsageSnapshot,
    },
    CancellationRequested {
        requested_by: ClientIdentity,
        reason: Option<String>,
    },
    ExternalMutationPrepared {
        mutation_id: String,
        tool_call_id: String,
        kind: MutationKind,
        idempotency_key: Option<String>,
        summary: String,
    },
    ExternalMutationConfirmed {
        mutation_id: String,
        confirmation_ref: Option<String>,
        summary: String,
    },
    AwaitingApproval {
        request_id: String,
        operation_sha256: String,
        expires_at_ms: u64,
        reason: Option<String>,
    },
    Paused {
        reason: Option<String>,
    },
    Cancelling {
        reason: Option<String>,
    },
    Completed {
        summary: Option<String>,
        result_artifact_ids: Vec<String>,
        usage: UsageSnapshot,
    },
    Failed {
        code: String,
        message: String,
        retryable: bool,
    },
    Cancelled {
        reason: Option<String>,
    },
    NeedsReconciliation {
        mutation_id: String,
        reason: String,
    },
    /// This run's frozen process image left for another owned node (roadmap
    /// K18), recorded on the origin's half of the chain.
    ///
    /// Deliberately **not terminal**. A departure is an attempt, not an
    /// outcome: the target can still refuse the image, and a run whose move was
    /// refused has to be able to carry on here. What makes the move auditable
    /// is not this event's status but its *hash* — it is the origin's chain tip
    /// that [`RunEvent::MigrationArrived`] names on the far side, so the two
    /// halves are one chain even though no database spans both machines.
    MigrationDeparted {
        target_node_id: String,
        /// SHA-256 of the transferred file payload, repeated by the arrival so
        /// a reader can tell that both nodes are talking about the same bytes.
        payload_sha256: String,
        checkpoint_id: String,
    },
    /// A frozen process image from another owned node arrived here and was
    /// admitted (roadmap K18), recorded as the first event of the target's half.
    ///
    /// `origin_last_event_hash` is what joins the halves. It is inside the
    /// envelope, which `event_chain_hash` covers, so the link cannot be edited
    /// on the target without breaking the target's own chain — no schema column
    /// and no second store needed to span two machines.
    MigrationArrived {
        origin_node_id: String,
        origin_last_sequence: u64,
        origin_last_event_hash: String,
        payload_sha256: String,
    },
}

impl RunEvent {
    #[must_use]
    pub const fn terminal_status(&self) -> Option<RunStatus> {
        match self {
            Self::Completed { .. } => Some(RunStatus::Succeeded),
            Self::Failed { .. } => Some(RunStatus::Failed),
            Self::Cancelled { .. } => Some(RunStatus::Cancelled),
            Self::NeedsReconciliation { .. } => Some(RunStatus::NeedsReconciliation),
            _ => None,
        }
    }

    #[must_use]
    pub const fn is_terminal(&self) -> bool {
        self.terminal_status().is_some()
    }

    pub fn validate(&self) -> ProtocolValidationResult {
        match self {
            Self::Queued { queue } => {
                if let Some(queue) = queue {
                    validate_protocol_id("event.queue", queue)?;
                }
            }
            Self::Started { engine_id } => {
                validate_protocol_id("event.engine_id", engine_id)?;
            }
            Self::ModelDelta {
                message_id,
                channel: _,
                text,
            } => {
                validate_protocol_id("event.message_id", message_id)?;
                validate_text("event.text", text, MAX_EVENT_TEXT_BYTES, false)?;
            }
            Self::ToolProposed {
                tool_call_id,
                tool_name,
                arguments,
                arguments_sha256,
                mutation: _,
            } => {
                validate_protocol_id("event.tool_call_id", tool_call_id)?;
                validate_protocol_id("event.tool_name", tool_name)?;
                arguments.validate("event.arguments")?;
                validate_sha256("event.arguments_sha256", arguments_sha256)?;
            }
            Self::PermissionRequested {
                request_id,
                tool_call_id,
                tool_name,
                operation_sha256,
                expires_at_ms,
                detail,
                risk_level: _,
                risk_reason,
            } => {
                validate_protocol_id("event.request_id", request_id)?;
                validate_protocol_id("event.tool_call_id", tool_call_id)?;
                validate_protocol_id("event.tool_name", tool_name)?;
                validate_sha256("event.operation_sha256", operation_sha256)?;
                validate_positive_timestamp("event.expires_at_ms", *expires_at_ms)?;
                validate_text("event.detail", detail, MAX_EVENT_TEXT_BYTES, false)?;
                validate_optional_event_text("event.risk_reason", risk_reason)?;
            }
            Self::PermissionDecided {
                request_id,
                operation_sha256,
                decision: _,
                decided_by,
            } => {
                validate_protocol_id("event.request_id", request_id)?;
                validate_sha256("event.operation_sha256", operation_sha256)?;
                decided_by.validate()?;
            }
            Self::RoutingDecided {
                task_class, reason, ..
            } => {
                // Only the two fields a reader cannot do without. The policy
                // id and name are legitimately absent when nothing matched, and
                // the chosen key is absent when the active target stands.
                validate_text("event.task_class", task_class, MAX_LABEL_BYTES, false)?;
                validate_text("event.reason", reason, MAX_EVENT_TEXT_BYTES, false)?;
            }
            Self::ToolStarted { tool_call_id } => {
                validate_protocol_id("event.tool_call_id", tool_call_id)?;
            }
            Self::ToolFinished {
                tool_call_id,
                outcome: _,
                output_excerpt,
                output_sha256,
                duration_ms,
            } => {
                validate_protocol_id("event.tool_call_id", tool_call_id)?;
                validate_optional_event_text("event.output_excerpt", output_excerpt)?;
                if let Some(digest) = output_sha256 {
                    validate_sha256("event.output_sha256", digest)?;
                }
                validate_range_u64("event.duration_ms", *duration_ms, 0, MAX_WALL_TIME_MS)?;
            }
            Self::ArtifactAdded {
                artifact_id,
                kind: _,
                name,
                media_type,
                content_sha256,
                size_bytes,
            } => {
                validate_protocol_id("event.artifact_id", artifact_id)?;
                validate_single_line("event.name", name, MAX_LABEL_BYTES)?;
                validate_single_line("event.media_type", media_type, 256)?;
                validate_sha256("event.content_sha256", content_sha256)?;
                validate_range_u64("event.size_bytes", *size_bytes, 0, MAX_ARTIFACT_BYTES)?;
            }
            Self::CheckpointLinked {
                checkpoint_id,
                kind: _,
                label,
                content_sha256,
            } => {
                validate_protocol_id("event.checkpoint_id", checkpoint_id)?;
                validate_single_line("event.label", label, MAX_LABEL_BYTES)?;
                if let Some(digest) = content_sha256 {
                    validate_sha256("event.content_sha256", digest)?;
                }
            }
            Self::VerificationFinished {
                verification_id,
                name,
                passed: _,
                summary,
                artifact_ids,
                duration_ms,
            } => {
                validate_protocol_id("event.verification_id", verification_id)?;
                validate_single_line("event.name", name, MAX_LABEL_BYTES)?;
                validate_text("event.summary", summary, MAX_EVENT_TEXT_BYTES, false)?;
                validate_unique_ids("event.artifact_ids", artifact_ids, MAX_REFERENCES_PER_EVENT)?;
                validate_range_u64("event.duration_ms", *duration_ms, 0, MAX_WALL_TIME_MS)?;
            }
            Self::UsageRecorded { usage } => usage.validate()?,
            Self::CancellationRequested {
                requested_by,
                reason,
            } => {
                requested_by.validate()?;
                validate_optional_event_text("event.reason", reason)?;
            }
            Self::ExternalMutationPrepared {
                mutation_id,
                tool_call_id,
                kind: _,
                idempotency_key,
                summary,
            } => {
                validate_protocol_id("event.mutation_id", mutation_id)?;
                validate_protocol_id("event.tool_call_id", tool_call_id)?;
                if let Some(key) = idempotency_key {
                    validate_idempotency_key("event.idempotency_key", key)?;
                }
                validate_text("event.summary", summary, MAX_EVENT_TEXT_BYTES, false)?;
            }
            Self::ExternalMutationConfirmed {
                mutation_id,
                confirmation_ref,
                summary,
            } => {
                validate_protocol_id("event.mutation_id", mutation_id)?;
                if let Some(reference) = confirmation_ref {
                    validate_single_line("event.confirmation_ref", reference, MAX_LABEL_BYTES)?;
                }
                validate_text("event.summary", summary, MAX_EVENT_TEXT_BYTES, false)?;
            }
            Self::AwaitingApproval {
                request_id,
                operation_sha256,
                expires_at_ms,
                reason,
            } => {
                validate_protocol_id("event.request_id", request_id)?;
                validate_sha256("event.operation_sha256", operation_sha256)?;
                validate_positive_timestamp("event.expires_at_ms", *expires_at_ms)?;
                validate_optional_event_text("event.reason", reason)?;
            }
            Self::Paused { reason } | Self::Cancelling { reason } => {
                validate_optional_event_text("event.reason", reason)?;
            }
            Self::Completed {
                summary,
                result_artifact_ids,
                usage,
            } => {
                validate_optional_event_text("event.summary", summary)?;
                validate_unique_ids(
                    "event.result_artifact_ids",
                    result_artifact_ids,
                    MAX_REFERENCES_PER_EVENT,
                )?;
                usage.validate()?;
            }
            Self::Failed {
                code,
                message,
                retryable: _,
            } => {
                validate_protocol_id("event.code", code)?;
                validate_text("event.message", message, MAX_EVENT_TEXT_BYTES, false)?;
            }
            Self::Cancelled { reason } => {
                validate_optional_event_text("event.reason", reason)?;
            }
            Self::NeedsReconciliation {
                mutation_id,
                reason,
            } => {
                validate_protocol_id("event.mutation_id", mutation_id)?;
                validate_text("event.reason", reason, MAX_EVENT_TEXT_BYTES, false)?;
            }
            Self::MigrationDeparted {
                target_node_id,
                payload_sha256,
                checkpoint_id,
            } => {
                validate_protocol_id("event.target_node_id", target_node_id)?;
                validate_sha256("event.payload_sha256", payload_sha256)?;
                validate_protocol_id("event.checkpoint_id", checkpoint_id)?;
            }
            Self::MigrationArrived {
                origin_node_id,
                origin_last_sequence,
                origin_last_event_hash,
                payload_sha256,
            } => {
                validate_protocol_id("event.origin_node_id", origin_node_id)?;
                if *origin_last_sequence == 0 {
                    return Err(ProtocolValidationError::new(
                        "event.origin_last_sequence",
                        "must name a real event on the origin's chain",
                    ));
                }
                validate_sha256("event.origin_last_event_hash", origin_last_event_hash)?;
                validate_sha256("event.payload_sha256", payload_sha256)?;
            }
        }
        Ok(())
    }

    fn approval_expiry_ms(&self) -> Option<u64> {
        match self {
            Self::PermissionRequested { expires_at_ms, .. }
            | Self::AwaitingApproval { expires_at_ms, .. } => Some(*expires_at_ms),
            _ => None,
        }
    }
}

fn validate_positive_timestamp(field: &str, value: u64) -> ProtocolValidationResult {
    if value == 0 {
        Err(ProtocolValidationError::new(
            field,
            "must be a positive Unix timestamp in milliseconds",
        ))
    } else {
        Ok(())
    }
}

fn validate_optional_event_text(field: &str, value: &Option<String>) -> ProtocolValidationResult {
    if let Some(value) = value {
        validate_text(field, value, MAX_EVENT_TEXT_BYTES, false)
    } else {
        Ok(())
    }
}

/// Versioned ledger/transport wrapper. `(run_id, sequence)` and `event_id`
/// are intended to be uniquely constrained by the durable ledger.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunEventEnvelope {
    pub schema_version: u32,
    pub event_id: String,
    pub run_id: String,
    pub sequence: u64,
    pub occurred_at_ms: u64,
    pub actor_id: Option<String>,
    pub emitter: ClientIdentity,
    pub event: RunEvent,
}

impl RunEventEnvelope {
    pub fn validate(&self) -> ProtocolValidationResult {
        validate_schema_version(self.schema_version)?;
        validate_protocol_id("event_id", &self.event_id)?;
        validate_protocol_id("run_id", &self.run_id)?;
        if self.sequence == 0 {
            return Err(ProtocolValidationError::new("sequence", "must start at 1"));
        }
        if self.occurred_at_ms == 0 {
            return Err(ProtocolValidationError::new(
                "occurred_at_ms",
                "must be a positive Unix timestamp in milliseconds",
            ));
        }
        if let Some(actor_id) = &self.actor_id {
            validate_protocol_id("actor_id", actor_id)?;
        }
        self.emitter.validate()?;
        self.event.validate()?;
        if let Some(expires_at_ms) = self.event.approval_expiry_ms() {
            if expires_at_ms <= self.occurred_at_ms {
                return Err(ProtocolValidationError::new(
                    "event.expires_at_ms",
                    "must be later than the event timestamp",
                ));
            }
        }
        Ok(())
    }

    #[must_use]
    pub const fn is_terminal(&self) -> bool {
        self.event.is_terminal()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn client() -> ClientIdentity {
        ClientIdentity {
            client_id: "desktop".to_string(),
            instance_id: "window-01".to_string(),
            kind: ClientKind::Desktop,
            version: "1.0.0-test".to_string(),
        }
    }

    fn capability(state: CapabilityState) -> CapabilityAssessment {
        CapabilityAssessment {
            state,
            evidence: "fixture evidence".to_string(),
        }
    }

    fn capabilities() -> ModelCapabilitiesSnapshot {
        ModelCapabilitiesSnapshot {
            tool_calling: capability(CapabilityState::Supported),
            vision: capability(CapabilityState::Unknown),
            embeddings: capability(CapabilityState::Unsupported),
            structured_output: capability(CapabilityState::Unknown),
            image_generation: capability(CapabilityState::Unsupported),
            audio: capability(CapabilityState::Unsupported),
            runtime_lifecycle: capability(CapabilityState::Supported),
            fim: capability(CapabilityState::Supported),
            code_completion: capability(CapabilityState::Supported),
            inline_edit: capability(CapabilityState::Supported),
            fim_metadata: Some(FimTemplateMetadata {
                prompt_template: None,
                prefix_token: Some("<fim_prefix>".to_string()),
                suffix_token: Some("<fim_suffix>".to_string()),
                middle_token: Some("<fim_middle>".to_string()),
                stop_tokens: vec!["<fim_end>".to_string()],
                max_prefix_tokens: Some(8_192),
                max_suffix_tokens: Some(4_096),
                max_completion_tokens: Some(1_024),
            }),
        }
    }

    fn spec() -> RunSpec {
        RunSpec {
            schema_version: RUN_PROTOCOL_SCHEMA_VERSION,
            run_id: "run-01".to_string(),
            idempotency_key: "desktop/run-01".to_string(),
            created_at_ms: 1_784_000_000_000,
            kind: RunKind::Interactive,
            submitted_by: client(),
            task: "Summarize the workspace".to_string(),
            instructions: Some("Use read-only tools.".to_string()),
            input_artifact_ids: vec!["artifact-input-01".to_string()],
            target: ModelTargetSnapshot::Provider {
                target_id: "provider-main-model".to_string(),
                label: "Provider model".to_string(),
                provider_id: "provider-main".to_string(),
                endpoint: "https://api.example.test/v1".to_string(),
                model: "example-model".to_string(),
                credential_ref_id: "provider-key-main".to_string(),
                capabilities: capabilities(),
            },
            workspace: Some(WorkspaceContext {
                workspace_id: "workspace-01".to_string(),
                primary_root_id: "root-main".to_string(),
                roots: vec![RootGrant {
                    root_id: "root-main".to_string(),
                    canonical_path: "/workspace/project".to_string(),
                    access: RootAccess::ReadOnly,
                    allow_symlinks_within_root: false,
                }],
                repository_policy: Some(RepositoryPolicy {
                    root_id: "root-main".to_string(),
                    owned_worktree_required: true,
                    allowed_remote_names: vec!["origin".to_string()],
                    allowed_branch_prefixes: vec!["codex/".to_string()],
                    allow_commit: false,
                    allow_push: false,
                    allow_create_pull_request: false,
                    allow_review_comment: false,
                    allow_merge: false,
                    allow_force_push: false,
                }),
            }),
            permission_policy: PermissionPolicySnapshot {
                mode: PermissionMode::Manual,
                unattended: false,
                approval_timeout_ms: 60_000,
                default_tool_decision: ToolPolicyDecision::Prompt,
                tool_rules: vec![ToolPermissionRule {
                    tool: "read_file".to_string(),
                    decision: ToolPolicyDecision::Allow,
                }],
                allow_network: false,
                allow_external_mutations: false,
                egress_allowlist: None,
            },
            budgets: RunBudgets {
                wall_time_ms: 60_000,
                max_iterations: 10,
                max_model_calls: 10,
                max_tool_calls: 20,
                max_input_tokens: 100_000,
                max_output_tokens: 10_000,
                max_cost_micros: Some(1_000_000),
                max_artifact_bytes: 10_000_000,
                max_event_count: 10_000,
            },
        }
    }

    fn usage() -> UsageSnapshot {
        UsageSnapshot {
            input_tokens: 100,
            output_tokens: 25,
            cached_input_tokens: 0,
            model_calls: 1,
            tool_calls: 0,
            cost_micros: Some(100),
        }
    }

    fn digest() -> String {
        "a".repeat(64)
    }

    fn successful_result() -> RunResultSnapshot {
        RunResultSnapshot {
            schema_version: RUN_PROTOCOL_SCHEMA_VERSION,
            run_id: "run-01".to_string(),
            status: RunStatus::Succeeded,
            started_at_ms: 1_000,
            finished_at_ms: 2_000,
            usage: usage(),
            result_artifact_ids: vec!["artifact-result-01".to_string()],
            summary: Some("Completed successfully".to_string()),
            failure: None,
            reconciliation: None,
        }
    }

    #[test]
    fn run_spec_and_event_roundtrip_without_semantic_loss() {
        let spec = spec();
        spec.validate().expect("valid spec");
        let encoded = serde_json::to_string(&spec).expect("serialize spec");
        let decoded: RunSpec = serde_json::from_str(&encoded).expect("deserialize spec");
        assert_eq!(decoded, spec);

        let envelope = RunEventEnvelope {
            schema_version: RUN_PROTOCOL_SCHEMA_VERSION,
            event_id: "event-01".to_string(),
            run_id: spec.run_id.clone(),
            sequence: 1,
            occurred_at_ms: spec.created_at_ms,
            actor_id: Some("coordinator-01".to_string()),
            emitter: client(),
            event: RunEvent::Completed {
                summary: Some("Done".to_string()),
                result_artifact_ids: vec!["artifact-result-01".to_string()],
                usage: usage(),
            },
        };
        envelope.validate().expect("valid event");
        let encoded = serde_json::to_string(&envelope).expect("serialize event");
        let decoded: RunEventEnvelope = serde_json::from_str(&encoded).expect("deserialize event");
        assert_eq!(decoded, envelope);
    }

    #[test]
    fn run_result_roundtrips_and_accepts_each_coherent_terminal_shape() {
        let succeeded = successful_result();
        succeeded.validate().expect("valid successful result");
        let encoded = serde_json::to_string(&succeeded).expect("serialize result");
        let decoded: RunResultSnapshot =
            serde_json::from_str(&encoded).expect("deserialize result");
        assert_eq!(decoded, succeeded);

        let mut failed = successful_result();
        failed.status = RunStatus::Failed;
        failed.failure = Some(RunFailureDetails {
            code: "provider_error".to_string(),
            message: "Provider request failed".to_string(),
            retryable: true,
        });
        failed.validate().expect("valid failed result");

        let mut cancelled = successful_result();
        cancelled.status = RunStatus::Cancelled;
        cancelled.summary = Some("Cancelled by the user".to_string());
        cancelled.validate().expect("valid cancelled result");

        let mut reconciliation = successful_result();
        reconciliation.status = RunStatus::NeedsReconciliation;
        reconciliation.reconciliation = Some(RunReconciliationDetails {
            mutation_id: "mutation-01".to_string(),
            reason: "Remote response was lost after submission".to_string(),
        });
        reconciliation
            .validate()
            .expect("valid reconciliation result");
    }

    #[test]
    fn run_result_rejects_nonterminal_and_contradictory_states() {
        let mut invalid = successful_result();
        invalid.status = RunStatus::Running;
        assert_eq!(invalid.validate().unwrap_err().field, "result.status");

        invalid = successful_result();
        invalid.status = RunStatus::Failed;
        assert_eq!(invalid.validate().unwrap_err().field, "result.status");

        invalid = successful_result();
        invalid.failure = Some(RunFailureDetails {
            code: "unexpected".to_string(),
            message: "Contradicts success".to_string(),
            retryable: false,
        });
        assert_eq!(invalid.validate().unwrap_err().field, "result.status");

        invalid = successful_result();
        invalid.status = RunStatus::NeedsReconciliation;
        assert_eq!(invalid.validate().unwrap_err().field, "result.status");

        invalid = successful_result();
        invalid.finished_at_ms = invalid.started_at_ms - 1;
        assert_eq!(
            invalid.validate().unwrap_err().field,
            "result.finished_at_ms"
        );
    }

    #[test]
    fn model_only_run_without_workspace_is_valid() {
        let mut model_only = spec();
        model_only.workspace = None;
        model_only.input_artifact_ids.clear();
        model_only.permission_policy.tool_rules.clear();
        model_only.budgets.max_tool_calls = 0;
        model_only.validate().expect("workspace is optional");
    }

    #[test]
    fn model_snapshot_is_secret_reference_only_and_rejects_raw_secret_fields() {
        let spec = spec();
        let encoded = serde_json::to_string(&spec.target).expect("serialize target");
        assert!(encoded.contains("credential_ref_id"));
        for forbidden in [
            "api_key",
            "access_token",
            "refresh_token",
            "password",
            "sk-live-secret",
        ] {
            assert!(!encoded.contains(forbidden));
        }

        let mut target = serde_json::to_value(&spec.target).expect("target value");
        target
            .as_object_mut()
            .expect("target object")
            .insert("api_key".to_string(), serde_json::json!("sk-live-secret"));
        assert!(serde_json::from_value::<ModelTargetSnapshot>(target).is_err());
    }

    #[test]
    fn invalid_schema_versions_and_ids_are_rejected() {
        let mut invalid = spec();
        invalid.schema_version = RUN_PROTOCOL_SCHEMA_VERSION + 1;
        assert_eq!(invalid.validate().unwrap_err().field, "schema_version");

        invalid = spec();
        invalid.run_id = "bad run id".to_string();
        assert_eq!(invalid.validate().unwrap_err().field, "run_id");

        let envelope = RunEventEnvelope {
            schema_version: RUN_PROTOCOL_SCHEMA_VERSION,
            event_id: "../event".to_string(),
            run_id: "run-01".to_string(),
            sequence: 1,
            occurred_at_ms: 1,
            actor_id: None,
            emitter: client(),
            event: RunEvent::Started {
                engine_id: "engine-01".to_string(),
            },
        };
        assert_eq!(envelope.validate().unwrap_err().field, "event_id");
    }

    #[test]
    fn terminal_status_and_event_classification_is_explicit() {
        for status in [
            RunStatus::Succeeded,
            RunStatus::Failed,
            RunStatus::Cancelled,
            RunStatus::NeedsReconciliation,
        ] {
            assert!(status.is_terminal());
        }
        for status in [
            RunStatus::Queued,
            RunStatus::Running,
            RunStatus::WaitingForPermission,
            RunStatus::Paused,
            RunStatus::Cancelling,
        ] {
            assert!(!status.is_terminal());
        }

        assert_eq!(
            RunEvent::Completed {
                summary: None,
                result_artifact_ids: vec![],
                usage: usage(),
            }
            .terminal_status(),
            Some(RunStatus::Succeeded)
        );
        assert_eq!(
            RunEvent::NeedsReconciliation {
                mutation_id: "mutation-01".to_string(),
                reason: "External outcome is uncertain".to_string(),
            }
            .terminal_status(),
            Some(RunStatus::NeedsReconciliation)
        );
        assert!(!RunEvent::Started {
            engine_id: "engine-01".to_string(),
        }
        .is_terminal());
        assert!(!RunEvent::Paused {
            reason: Some("waiting for user".to_string()),
        }
        .is_terminal());
    }

    /// Roadmap K9: the run already records *what* target ran (its frozen
    /// `ModelTargetSnapshot`); this is the *why*.
    ///
    /// Every field but the two a reader cannot do without is nullable, because
    /// "no policy matched" is the answer for a fresh profile and is worth being
    /// able to produce. What must never be empty is the class that was asked
    /// under and the sentence explaining the outcome — an event carrying
    /// neither would record that a decision happened and nothing about it.
    #[test]
    fn a_routing_decision_may_name_no_policy_but_never_no_reason() {
        let unrouted = RunEvent::RoutingDecided {
            task_class: "subagent_explore".to_string(),
            policy_id: None,
            policy_name: None,
            chosen_key: None,
            changed_from_active: false,
            reason: "No enabled policy covers this task class.".to_string(),
        };
        assert!(unrouted.validate().is_ok());
        assert!(!unrouted.is_terminal(), "a dispatch decision ends nothing");

        let blank_reason = RunEvent::RoutingDecided {
            task_class: "chat".to_string(),
            policy_id: Some("p-1".to_string()),
            policy_name: Some("Cheap explorers".to_string()),
            chosen_key: Some("provider:openrouter/cheap".to_string()),
            changed_from_active: true,
            reason: "   ".to_string(),
        };
        assert!(blank_reason.validate().is_err());

        let blank_class = RunEvent::RoutingDecided {
            task_class: String::new(),
            policy_id: None,
            policy_name: None,
            chosen_key: None,
            changed_from_active: false,
            reason: "No policy.".to_string(),
        };
        assert!(blank_class.validate().is_err());
    }

    #[test]
    fn audited_event_variants_have_exact_stable_wire_names() {
        let events = vec![
            (
                "queued",
                RunEvent::Queued {
                    queue: Some("default".to_string()),
                },
            ),
            (
                "started",
                RunEvent::Started {
                    engine_id: "engine-01".to_string(),
                },
            ),
            (
                "model_delta",
                RunEvent::ModelDelta {
                    message_id: "message-01".to_string(),
                    channel: OutputChannel::Assistant,
                    text: "hello".to_string(),
                },
            ),
            (
                "tool_proposed",
                RunEvent::ToolProposed {
                    tool_call_id: "tool-call-01".to_string(),
                    tool_name: "read_file".to_string(),
                    arguments: RedactedPayload {
                        value: serde_json::json!({"path": "src/main.rs"}),
                        redaction: RedactionState::NotNeeded,
                    },
                    arguments_sha256: digest(),
                    mutation: false,
                },
            ),
            (
                "permission_requested",
                RunEvent::PermissionRequested {
                    request_id: "permission-01".to_string(),
                    tool_call_id: "tool-call-01".to_string(),
                    tool_name: "write_file".to_string(),
                    operation_sha256: digest(),
                    expires_at_ms: 2_000,
                    detail: "Write src/main.rs".to_string(),
                    risk_level: Some(RiskLevel::Medium),
                    risk_reason: Some("workspace mutation".to_string()),
                },
            ),
            (
                "permission_decided",
                RunEvent::PermissionDecided {
                    request_id: "permission-01".to_string(),
                    operation_sha256: digest(),
                    decision: PermissionDecision::AllowOnce,
                    decided_by: client(),
                },
            ),
            (
                "tool_started",
                RunEvent::ToolStarted {
                    tool_call_id: "tool-call-01".to_string(),
                },
            ),
            (
                "tool_finished",
                RunEvent::ToolFinished {
                    tool_call_id: "tool-call-01".to_string(),
                    outcome: ToolOutcome::Succeeded,
                    output_excerpt: Some("done".to_string()),
                    output_sha256: Some(digest()),
                    duration_ms: 10,
                },
            ),
            (
                "artifact_added",
                RunEvent::ArtifactAdded {
                    artifact_id: "artifact-01".to_string(),
                    kind: ArtifactKind::Report,
                    name: "Report".to_string(),
                    media_type: "text/markdown".to_string(),
                    content_sha256: digest(),
                    size_bytes: 20,
                },
            ),
            (
                "checkpoint_linked",
                RunEvent::CheckpointLinked {
                    checkpoint_id: "checkpoint-01".to_string(),
                    kind: CheckpointKind::Workspace,
                    label: "Before edit".to_string(),
                    content_sha256: Some(digest()),
                },
            ),
            (
                "verification_finished",
                RunEvent::VerificationFinished {
                    verification_id: "verification-01".to_string(),
                    name: "unit-tests".to_string(),
                    passed: true,
                    summary: "All tests passed".to_string(),
                    artifact_ids: vec!["artifact-01".to_string()],
                    duration_ms: 15,
                },
            ),
            (
                "external_mutation_prepared",
                RunEvent::ExternalMutationPrepared {
                    mutation_id: "mutation-01".to_string(),
                    tool_call_id: "tool-call-01".to_string(),
                    kind: MutationKind::ExternalService,
                    idempotency_key: Some("github/pr/create/01".to_string()),
                    summary: "Create draft pull request".to_string(),
                },
            ),
            (
                "external_mutation_confirmed",
                RunEvent::ExternalMutationConfirmed {
                    mutation_id: "mutation-01".to_string(),
                    confirmation_ref: Some("pull-request-42".to_string()),
                    summary: "Draft pull request created".to_string(),
                },
            ),
            (
                "awaiting_approval",
                RunEvent::AwaitingApproval {
                    request_id: "permission-01".to_string(),
                    operation_sha256: digest(),
                    expires_at_ms: 2_000,
                    reason: Some("User decision required".to_string()),
                },
            ),
            (
                "paused",
                RunEvent::Paused {
                    reason: Some("Detached client".to_string()),
                },
            ),
            (
                "cancelling",
                RunEvent::Cancelling {
                    reason: Some("User requested cancellation".to_string()),
                },
            ),
            (
                "completed",
                RunEvent::Completed {
                    summary: Some("Done".to_string()),
                    result_artifact_ids: vec!["artifact-01".to_string()],
                    usage: usage(),
                },
            ),
            (
                "failed",
                RunEvent::Failed {
                    code: "provider_error".to_string(),
                    message: "Provider request failed".to_string(),
                    retryable: true,
                },
            ),
            (
                "cancelled",
                RunEvent::Cancelled {
                    reason: Some("Cancelled".to_string()),
                },
            ),
            (
                "needs_reconciliation",
                RunEvent::NeedsReconciliation {
                    mutation_id: "mutation-01".to_string(),
                    reason: "External outcome is uncertain".to_string(),
                },
            ),
        ];

        for (expected_name, event) in events {
            event.validate().expect(expected_name);
            let encoded = serde_json::to_value(event).expect("serialize event");
            assert_eq!(encoded["type"], expected_name);
        }
    }

    #[test]
    fn approval_events_bind_to_operation_digest_and_future_expiry() {
        let operation_sha256 = digest();
        let requested = RunEvent::PermissionRequested {
            request_id: "permission-01".to_string(),
            tool_call_id: "tool-call-01".to_string(),
            tool_name: "write_file".to_string(),
            operation_sha256: operation_sha256.clone(),
            expires_at_ms: 2_000,
            detail: "Write file".to_string(),
            risk_level: None,
            risk_reason: None,
        };
        let decided = RunEvent::PermissionDecided {
            request_id: "permission-01".to_string(),
            operation_sha256: operation_sha256.clone(),
            decision: PermissionDecision::AllowOnce,
            decided_by: client(),
        };
        assert_eq!(
            serde_json::to_value(&requested).expect("request")["payload"]["operation_sha256"],
            operation_sha256
        );
        assert_eq!(
            serde_json::to_value(&decided).expect("decision")["payload"]["operation_sha256"],
            digest()
        );

        let stale = RunEventEnvelope {
            schema_version: RUN_PROTOCOL_SCHEMA_VERSION,
            event_id: "event-approval-01".to_string(),
            run_id: "run-01".to_string(),
            sequence: 1,
            occurred_at_ms: 2_000,
            actor_id: None,
            emitter: client(),
            event: requested,
        };
        assert_eq!(stale.validate().unwrap_err().field, "event.expires_at_ms");
    }

    #[test]
    fn fim_metadata_is_bounded_and_requires_declared_support() {
        let mut target = spec().target;
        if let ModelTargetSnapshot::Provider { capabilities, .. } = &mut target {
            capabilities
                .fim_metadata
                .as_mut()
                .expect("fixture metadata")
                .prompt_template = Some("x".repeat(MAX_FIM_TEMPLATE_BYTES + 1));
        }
        assert_eq!(
            target.validate().unwrap_err().field,
            "target.capabilities.fim_metadata.prompt_template"
        );

        let mut target = spec().target;
        if let ModelTargetSnapshot::Provider { capabilities, .. } = &mut target {
            capabilities.fim.state = CapabilityState::Unknown;
        }
        assert_eq!(
            target.validate().unwrap_err().field,
            "target.capabilities.fim_metadata"
        );
    }

    #[test]
    fn event_fields_are_bounded_before_ledger_insertion() {
        let event = RunEvent::ModelDelta {
            message_id: "message-01".to_string(),
            channel: OutputChannel::Assistant,
            text: "x".repeat(MAX_EVENT_TEXT_BYTES + 1),
        };
        assert_eq!(event.validate().unwrap_err().field, "event.text");
    }

    mod egress_allowlist {
        use super::*;

        /// Exactly what a run row frozen before the field existed contains.
        ///
        /// Written out as a **string literal** and not built with `json!`, because the
        /// claim is about bytes that already exist on disk: a `json!` fixture is built
        /// from this build's own idea of the shape, so it would keep passing after a
        /// change that broke every stored row. `deny_unknown_fields` is on this struct,
        /// so this is also the test that would catch the field being added without a
        /// `default`.
        const FROZEN_WITHOUT_THE_FIELD: &str = r#"{
            "mode": "manual",
            "unattended": false,
            "approval_timeout_ms": 60000,
            "default_tool_decision": "prompt",
            "tool_rules": [{"tool": "read_file", "decision": "allow"}],
            "allow_network": false,
            "allow_external_mutations": false
        }"#;

        #[test]
        fn a_policy_frozen_before_the_field_existed_still_deserializes() {
            let policy: PermissionPolicySnapshot = serde_json::from_str(FROZEN_WITHOUT_THE_FIELD)
                .expect("every run row already on disk lacks this field and must keep loading");
            assert_eq!(
                policy.egress_allowlist, None,
                "absent must mean absent, not an empty (deny-all) declaration"
            );
            policy.validate().expect("an absent allowlist is valid");
        }

        /// The other half of the compatibility property: a policy that declares
        /// nothing must serialize to the same bytes it always did. `run_ledger`
        /// compares the serialized spec byte-for-byte to decide whether a
        /// resubmission is the same run, so an emitted `"egress_allowlist":null`
        /// would turn every idempotent resubmit into a conflict.
        #[test]
        fn declaring_nothing_adds_no_bytes_to_a_frozen_spec() {
            let policy: PermissionPolicySnapshot =
                serde_json::from_str(FROZEN_WITHOUT_THE_FIELD).expect("loads");
            let reserialized = serde_json::to_string(&policy).expect("serializes");
            assert!(
                !reserialized.contains("egress_allowlist"),
                "an absent allowlist must not appear in the output at all: {reserialized}"
            );
        }

        #[test]
        fn a_declared_allowlist_round_trips() {
            let mut policy: PermissionPolicySnapshot =
                serde_json::from_str(FROZEN_WITHOUT_THE_FIELD).expect("loads");
            policy.egress_allowlist = Some(EgressAllowlist {
                hosts: vec![
                    "api.example.com".to_string(),
                    "*.cdn.example.com".to_string(),
                ],
                ports: vec![443],
                protocols: vec!["https".to_string()],
            });
            policy.validate().expect("valid declaration");

            let encoded = serde_json::to_string(&policy).expect("serializes");
            let decoded: PermissionPolicySnapshot =
                serde_json::from_str(&encoded).expect("deserializes");
            assert_eq!(decoded, policy);
        }

        /// An empty declaration is legal and means deny-all. Pinned because the
        /// temptation is to validate it as a mistake, which would remove the only way
        /// a submitter can say "this run sends nothing".
        #[test]
        fn an_empty_declaration_is_valid_and_is_not_the_same_as_no_declaration() {
            let empty = EgressAllowlist::default();
            empty
                .validate()
                .expect("an empty allowlist is a legal policy");
            assert_ne!(Some(empty), None::<EgressAllowlist>);
        }

        /// A dimension omitted from a present declaration is an error, not a silent
        /// deny-all: absence is a statement at the allowlist level and nowhere else.
        #[test]
        fn a_declaration_must_name_all_three_dimensions() {
            let partial = r#"{"hosts": ["api.example.com"]}"#;
            assert!(serde_json::from_str::<EgressAllowlist>(partial).is_err());
        }

        #[test]
        fn each_entry_shape_is_validated_at_submission() {
            let cases: &[(EgressAllowlist, &str)] = &[
                (
                    EgressAllowlist {
                        hosts: vec!["API.example.com".to_string()],
                        ports: vec![443],
                        protocols: vec!["https".to_string()],
                    },
                    "permission_policy.egress_allowlist.hosts",
                ),
                (
                    EgressAllowlist {
                        hosts: vec!["https://api.example.com/v1".to_string()],
                        ports: vec![443],
                        protocols: vec!["https".to_string()],
                    },
                    "permission_policy.egress_allowlist.hosts",
                ),
                (
                    EgressAllowlist {
                        hosts: vec!["*.".to_string()],
                        ports: vec![443],
                        protocols: vec!["https".to_string()],
                    },
                    "permission_policy.egress_allowlist.hosts",
                ),
                (
                    EgressAllowlist {
                        hosts: vec!["api.example.com".to_string()],
                        ports: vec![0],
                        protocols: vec!["https".to_string()],
                    },
                    "permission_policy.egress_allowlist.ports",
                ),
                (
                    EgressAllowlist {
                        hosts: vec!["api.example.com".to_string()],
                        ports: vec![443],
                        protocols: vec!["HTTPS".to_string()],
                    },
                    "permission_policy.egress_allowlist.protocols",
                ),
                (
                    EgressAllowlist {
                        hosts: vec!["api.example.com".to_string()],
                        ports: vec![443],
                        protocols: vec!["https://".to_string()],
                    },
                    "permission_policy.egress_allowlist.protocols",
                ),
                (
                    EgressAllowlist {
                        hosts: (0..=MAX_EGRESS_ALLOWLIST_ENTRIES)
                            .map(|index| format!("host{index}.example.com"))
                            .collect(),
                        ports: vec![443],
                        protocols: vec!["https".to_string()],
                    },
                    "permission_policy.egress_allowlist.hosts",
                ),
            ];

            for (allowlist, field) in cases {
                assert_eq!(
                    allowlist.validate().expect_err("must be refused").field,
                    *field,
                    "for {allowlist:?}"
                );
            }
        }

        /// The allowlist is validated *through* the policy, so a bad declaration
        /// cannot be frozen by a submitter that only calls the outer `validate`.
        #[test]
        fn a_bad_declaration_fails_the_policy_it_is_declared_on() {
            let mut policy: PermissionPolicySnapshot =
                serde_json::from_str(FROZEN_WITHOUT_THE_FIELD).expect("loads");
            policy.egress_allowlist = Some(EgressAllowlist {
                hosts: vec!["Api.Example.Com".to_string()],
                ports: vec![443],
                protocols: vec!["https".to_string()],
            });
            assert_eq!(
                policy.validate().expect_err("must be refused").field,
                "permission_policy.egress_allowlist.hosts"
            );
        }
    }
}
