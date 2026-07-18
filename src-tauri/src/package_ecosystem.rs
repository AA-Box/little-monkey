//! Declarative package, assistant, and connector trust/install core.
//!
//! Packages handled here are data-only. Native binaries, scripts, MCP server
//! installation, OAuth, and executable extensions are intentionally outside
//! this format and require separate approval/sandbox boundaries.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fmt;
#[cfg(unix)]
use std::fs::File;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::Mutex;

use serde::{de, Deserialize, Deserializer, Serialize, Serializer};
use sha2::{Digest, Sha256};
use url::{Host, Url};
use uuid::Uuid;

pub const PACKAGE_MANIFEST_VERSION: u32 = 1;
pub const TRUST_STORE_VERSION: u32 = 1;
pub const REGISTRY_SNAPSHOT_VERSION: u32 = 1;
pub const PACKAGE_STATE_VERSION: u32 = 1;
pub const PORTABLE_EXPORT_VERSION: u32 = 1;
pub const CONNECTOR_CONTRACT_VERSION: u32 = 1;

const CACHE_DIR: &str = "cache";
const STATE_DIR: &str = "state";
const STATE_PREFIX: &str = "state-";
const STATE_SUFFIX: &str = ".json";

pub type PackageResult<T> = Result<T, PackageError>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PackageError {
    InvalidManifest(String),
    InvalidBundle(String),
    Incompatible(String),
    Untrusted(String),
    Revoked(String),
    PermissionApprovalRequired(String),
    Pinned(String),
    NotInstalled(String),
    Conflict(String),
    LimitExceeded(String),
    Io(String),
    Json(String),
    Verifier(String),
}

impl fmt::Display for PackageError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidManifest(message) => {
                write!(formatter, "invalid package manifest: {message}")
            }
            Self::InvalidBundle(message) => write!(formatter, "invalid package bundle: {message}"),
            Self::Incompatible(message) => write!(formatter, "incompatible package: {message}"),
            Self::Untrusted(message) => write!(formatter, "untrusted package: {message}"),
            Self::Revoked(message) => write!(formatter, "revoked package: {message}"),
            Self::PermissionApprovalRequired(message) => {
                write!(formatter, "permission approval required: {message}")
            }
            Self::Pinned(message) => write!(formatter, "package is pinned: {message}"),
            Self::NotInstalled(message) => write!(formatter, "package not installed: {message}"),
            Self::Conflict(message) => write!(formatter, "package state conflict: {message}"),
            Self::LimitExceeded(message) => write!(formatter, "package limit exceeded: {message}"),
            Self::Io(message) => write!(formatter, "package I/O error: {message}"),
            Self::Json(message) => write!(formatter, "package JSON error: {message}"),
            Self::Verifier(message) => write!(formatter, "signature verifier error: {message}"),
        }
    }
}

impl std::error::Error for PackageError {}

impl From<io::Error> for PackageError {
    fn from(error: io::Error) -> Self {
        Self::Io(error.to_string())
    }
}

impl From<serde_json::Error> for PackageError {
    fn from(error: serde_json::Error) -> Self {
        Self::Json(error.to_string())
    }
}

fn sha256(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

fn is_sha256(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn validate_id(label: &str, value: &str) -> PackageResult<()> {
    if value.is_empty()
        || value.len() > 160
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':'))
    {
        return Err(PackageError::InvalidManifest(format!(
            "{label} must be a bounded ASCII identifier"
        )));
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SemanticVersion {
    pub major: u32,
    pub minor: u32,
    pub patch: u32,
}

impl SemanticVersion {
    pub const fn new(major: u32, minor: u32, patch: u32) -> Self {
        Self {
            major,
            minor,
            patch,
        }
    }

    pub fn parse(value: &str) -> PackageResult<Self> {
        let parts = value.split('.').collect::<Vec<_>>();
        if parts.len() != 3
            || parts.iter().any(|part| {
                part.is_empty()
                    || (part.len() > 1 && part.starts_with('0'))
                    || !part.bytes().all(|byte| byte.is_ascii_digit())
            })
        {
            return Err(PackageError::InvalidManifest(format!(
                "version must be strict major.minor.patch: {value}"
            )));
        }
        Ok(Self {
            major: parts[0]
                .parse()
                .map_err(|_| PackageError::InvalidManifest("version major overflow".to_string()))?,
            minor: parts[1]
                .parse()
                .map_err(|_| PackageError::InvalidManifest("version minor overflow".to_string()))?,
            patch: parts[2]
                .parse()
                .map_err(|_| PackageError::InvalidManifest("version patch overflow".to_string()))?,
        })
    }
}

impl fmt::Display for SemanticVersion {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

impl Serialize for SemanticVersion {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for SemanticVersion {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::parse(&value).map_err(de::Error::custom)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct PackageLimits {
    pub max_files: usize,
    pub max_file_bytes: u64,
    pub max_total_bytes: u64,
    pub max_manifest_bytes: usize,
    pub max_ui_resources: usize,
    pub max_permissions: usize,
}

impl Default for PackageLimits {
    fn default() -> Self {
        Self {
            max_files: 1_000,
            max_file_bytes: 16 * 1024 * 1024,
            max_total_bytes: 128 * 1024 * 1024,
            max_manifest_bytes: 2 * 1024 * 1024,
            max_ui_resources: 128,
            max_permissions: 256,
        }
    }
}

impl PackageLimits {
    pub fn validate(&self) -> PackageResult<()> {
        if self.max_files == 0
            || self.max_file_bytes == 0
            || self.max_total_bytes < self.max_file_bytes
            || self.max_manifest_bytes == 0
            || self.max_ui_resources == 0
            || self.max_permissions == 0
        {
            return Err(PackageError::LimitExceeded(
                "package limits are internally inconsistent".to_string(),
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum PackageKind {
    Skill,
    Assistant,
    Connector,
    Collection,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum ContentKind {
    Instructions,
    Prompt,
    Persona,
    Rule,
    WorkflowTemplate,
    KnowledgeTemplate,
    UiResource,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ContentReference {
    pub kind: ContentKind,
    pub path: String,
    pub media_type: String,
    pub sha256: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum McpRequirementKind {
    ExistingServer,
    RemoteHttp,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct McpRequirement {
    pub requirement_id: String,
    pub kind: McpRequirementKind,
    pub server_id: Option<String>,
    pub remote_origin: Option<String>,
    pub required_tools: BTreeSet<String>,
    /// Always a separate user action; package install never performs it.
    pub separate_install_approval_required: bool,
    pub separate_oauth_approval_required: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct UiResourceDeclaration {
    pub resource_id: String,
    pub entry_path: String,
    pub sha256: String,
    pub media_type: String,
    pub opaque_origin_required: bool,
    pub declared_network_origins: BTreeSet<String>,
    pub declared_host_actions: BTreeSet<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ConnectorKind {
    Github,
    Gitlab,
    Rest,
    Webhook,
    FilesystemEvents,
    Webdav,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ConnectorAuthKind {
    None,
    OAuth,
    SecretReference,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ConnectorEffect {
    Read,
    LocalMutation,
    ExternalMutation,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ConnectorOperation {
    pub operation_id: String,
    pub method: String,
    pub path_template: String,
    pub effect: ConnectorEffect,
    pub required_permission_ids: BTreeSet<String>,
    pub idempotency_supported: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ConnectorDeclaration {
    pub contract_version: u32,
    pub kind: ConnectorKind,
    pub auth: ConnectorAuthKind,
    pub allowed_origins: BTreeSet<String>,
    pub operations: Vec<ConnectorOperation>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AssistantComposition {
    pub persona_content_path: String,
    pub skill_package_ids: BTreeSet<String>,
    pub starter_workflow_paths: Vec<String>,
    pub knowledge_template_path: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ModelRequirement {
    pub capability: String,
    pub minimum_context_tokens: Option<u64>,
    pub local_compatible: bool,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum PermissionKind {
    ReadFiles,
    WriteFiles,
    Network,
    InvokeMcpTool,
    UseModel,
    CreateArtifact,
    ExecuteProcess,
    InstallExecutable,
    ReadRawKeychain,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(deny_unknown_fields)]
pub struct PackagePermission {
    pub permission_id: String,
    pub kind: PermissionKind,
    pub scope: String,
    pub reason: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum VulnerabilitySeverity {
    Low,
    Medium,
    High,
    Critical,
}

/// Manifest-declared security notice. There is no live CVE/vulnerability feed
/// in this app: publishers declare these notices as part of the signed
/// manifest, and this struct carries exactly what was declared and verified
/// through the same trust chain as the rest of the manifest — nothing here
/// is fetched, inferred, or refreshed from a live external source.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(deny_unknown_fields)]
pub struct VulnerabilityNotice {
    pub notice_id: String,
    pub severity: VulnerabilitySeverity,
    pub summary: String,
    pub affected_versions: BTreeSet<SemanticVersion>,
    pub advisory_url: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Compatibility {
    pub minimum_app_version: SemanticVersion,
    pub maximum_app_version_exclusive: Option<SemanticVersion>,
    pub platforms: BTreeSet<String>,
    pub architectures: BTreeSet<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum InstallSource {
    LocalFolder { canonical_path: String },
    Git { remote: String, commit_sha: String },
    CuratedRegistry { registry_id: String },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PackageProvenance {
    pub publisher: String,
    pub source: InstallSource,
    pub source_revision: String,
    pub build_reproducible: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PackageSignature {
    pub trust_root_id: String,
    pub key_id: String,
    pub algorithm: String,
    pub signature_hex: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PackageManifest {
    pub schema_version: u32,
    pub package_id: String,
    pub version: SemanticVersion,
    pub kind: PackageKind,
    pub display_name: String,
    pub description: String,
    pub content: Vec<ContentReference>,
    pub assistant: Option<AssistantComposition>,
    pub connector: Option<ConnectorDeclaration>,
    pub mcp_requirements: Vec<McpRequirement>,
    pub ui_resources: Vec<UiResourceDeclaration>,
    pub model_requirements: Vec<ModelRequirement>,
    pub permissions: BTreeSet<PackagePermission>,
    /// Publisher-declared, manifest-signed security notices. Absent from
    /// older/imported manifests that predate this field, hence the field
    /// default so existing bundles keep deserializing unchanged. Also
    /// skipped when empty on serialization, so a manifest that declares no
    /// notices produces the exact same signing payload bytes as it did
    /// before this field existed — already-issued signatures (including the
    /// bundled first-party release catalog) stay valid.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub vulnerability_notices: Vec<VulnerabilityNotice>,
    pub compatibility: Compatibility,
    pub file_checksums: BTreeMap<String, String>,
    pub provenance: PackageProvenance,
    pub signature: Option<PackageSignature>,
}

impl PackageManifest {
    pub fn validate(&self, limits: &PackageLimits) -> PackageResult<()> {
        limits.validate()?;
        if self.schema_version != PACKAGE_MANIFEST_VERSION {
            return Err(PackageError::InvalidManifest(format!(
                "unsupported schema version {}",
                self.schema_version
            )));
        }
        validate_package_id(&self.package_id)?;
        if self.display_name.trim().is_empty()
            || self.display_name.len() > 160
            || self.description.len() > 8_192
            || self.content.len() > limits.max_files
            || self.file_checksums.len() > limits.max_files
            || self.ui_resources.len() > limits.max_ui_resources
            || self.permissions.len() > limits.max_permissions
        {
            return Err(PackageError::LimitExceeded(
                "manifest metadata or collection exceeds limits".to_string(),
            ));
        }
        if self.kind == PackageKind::Assistant && self.assistant.is_none() {
            return Err(PackageError::InvalidManifest(
                "assistant package requires assistant composition".to_string(),
            ));
        }
        if self.kind != PackageKind::Assistant && self.assistant.is_some() {
            return Err(PackageError::InvalidManifest(
                "assistant composition appears on a non-assistant package".to_string(),
            ));
        }
        if let Some(assistant) = &self.assistant {
            validate_assistant(assistant, &self.content)?;
        }
        if self.kind == PackageKind::Connector && self.connector.is_none() {
            return Err(PackageError::InvalidManifest(
                "connector package requires connector declaration".to_string(),
            ));
        }
        if self.kind != PackageKind::Connector && self.connector.is_some() {
            return Err(PackageError::InvalidManifest(
                "connector declaration appears on a non-connector package".to_string(),
            ));
        }
        if let Some(connector) = &self.connector {
            validate_connector(connector, &self.permissions)?;
        }
        for content in &self.content {
            validate_relative_path(&content.path)?;
            if !is_sha256(&content.sha256)
                || content.media_type.is_empty()
                || content.media_type.len() > 160
                || self.file_checksums.get(&content.path) != Some(&content.sha256)
            {
                return Err(PackageError::InvalidManifest(format!(
                    "invalid content reference: {}",
                    content.path
                )));
            }
        }
        for (path, digest) in &self.file_checksums {
            validate_relative_path(path)?;
            if !is_sha256(digest) {
                return Err(PackageError::InvalidManifest(format!(
                    "invalid checksum for {path}"
                )));
            }
        }
        for requirement in &self.mcp_requirements {
            validate_mcp_requirement(requirement)?;
        }
        for resource in &self.ui_resources {
            validate_ui_resource(resource, &self.file_checksums)?;
        }
        for permission in &self.permissions {
            validate_permission(permission)?;
            if matches!(
                permission.kind,
                PermissionKind::ExecuteProcess
                    | PermissionKind::InstallExecutable
                    | PermissionKind::ReadRawKeychain
            ) {
                return Err(PackageError::InvalidManifest(format!(
                    "declarative packages cannot request {:?}",
                    permission.kind
                )));
            }
        }
        if self.vulnerability_notices.len() > 64 {
            return Err(PackageError::LimitExceeded(
                "manifest declares too many vulnerability notices".to_string(),
            ));
        }
        for notice in &self.vulnerability_notices {
            validate_vulnerability_notice(notice)?;
        }
        validate_compatibility(&self.compatibility)?;
        validate_provenance(&self.provenance)?;
        if let Some(signature) = &self.signature {
            validate_id("trust_root_id", &signature.trust_root_id)?;
            validate_id("key_id", &signature.key_id)?;
            validate_id("signature algorithm", &signature.algorithm)?;
            if decode_hex(&signature.signature_hex)?.is_empty() {
                return Err(PackageError::InvalidManifest(
                    "package signature cannot be empty".to_string(),
                ));
            }
        }
        let manifest_size = serde_json::to_vec(self)?.len();
        if manifest_size > limits.max_manifest_bytes {
            return Err(PackageError::LimitExceeded(format!(
                "manifest is {manifest_size} bytes"
            )));
        }
        Ok(())
    }

    pub fn signing_payload(&self) -> PackageResult<Vec<u8>> {
        let mut unsigned = self.clone();
        if let Some(signature) = &mut unsigned.signature {
            signature.signature_hex.clear();
        }
        Ok(serde_json::to_vec(&unsigned)?)
    }
}

fn validate_package_id(value: &str) -> PackageResult<()> {
    validate_id("package_id", value)?;
    let mut parts = value.split('.');
    if parts.clone().count() < 3
        || parts.any(|part| {
            part.is_empty()
                || !part
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        })
    {
        return Err(PackageError::InvalidManifest(
            "package_id must be a reverse-domain lower-case identifier".to_string(),
        ));
    }
    Ok(())
}

fn validate_relative_path(value: &str) -> PackageResult<()> {
    if value.is_empty()
        || value.len() > 512
        || value.contains('\\')
        || value.contains('\0')
        || Path::new(value).is_absolute()
        || Path::new(value).components().any(|component| {
            matches!(
                component,
                Component::ParentDir
                    | Component::CurDir
                    | Component::RootDir
                    | Component::Prefix(_)
            )
        })
    {
        return Err(PackageError::InvalidManifest(format!(
            "unsafe package path: {value}"
        )));
    }
    Ok(())
}

fn validate_origin(value: &str) -> PackageResult<String> {
    let url = Url::parse(value)
        .map_err(|error| PackageError::InvalidManifest(format!("invalid origin: {error}")))?;
    if url.scheme() != "https"
        || url.path() != "/"
        || url.query().is_some()
        || url.fragment().is_some()
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return Err(PackageError::InvalidManifest(
            "network origins must be credential-free HTTPS origins".to_string(),
        ));
    }
    match url.host() {
        Some(Host::Domain(host)) if host != "localhost" && !host.ends_with(".localhost") => {}
        _ => {
            return Err(PackageError::InvalidManifest(
                "network origins cannot use literal or local hosts".to_string(),
            ));
        }
    }
    Ok(url.origin().ascii_serialization())
}

fn validate_mcp_requirement(requirement: &McpRequirement) -> PackageResult<()> {
    validate_id("MCP requirement id", &requirement.requirement_id)?;
    if !requirement.separate_install_approval_required {
        return Err(PackageError::InvalidManifest(
            "MCP requirements must retain separate installation approval".to_string(),
        ));
    }
    match requirement.kind {
        McpRequirementKind::ExistingServer => {
            let server_id = requirement.server_id.as_deref().ok_or_else(|| {
                PackageError::InvalidManifest(
                    "existing MCP requirement needs server_id".to_string(),
                )
            })?;
            validate_id("MCP server id", server_id)?;
            if requirement.remote_origin.is_some() {
                return Err(PackageError::InvalidManifest(
                    "existing MCP requirement cannot declare an origin".to_string(),
                ));
            }
        }
        McpRequirementKind::RemoteHttp => {
            let origin = requirement.remote_origin.as_deref().ok_or_else(|| {
                PackageError::InvalidManifest("remote MCP requirement needs origin".to_string())
            })?;
            validate_origin(origin)?;
            if requirement.server_id.is_some() || !requirement.separate_oauth_approval_required {
                return Err(PackageError::InvalidManifest(
                    "remote MCP OAuth/configuration must remain a separate approval".to_string(),
                ));
            }
        }
    }
    for tool in &requirement.required_tools {
        validate_id("MCP tool", tool)?;
    }
    Ok(())
}

fn validate_ui_resource(
    resource: &UiResourceDeclaration,
    checksums: &BTreeMap<String, String>,
) -> PackageResult<()> {
    validate_id("UI resource id", &resource.resource_id)?;
    validate_relative_path(&resource.entry_path)?;
    if !resource.opaque_origin_required
        || !is_sha256(&resource.sha256)
        || checksums.get(&resource.entry_path) != Some(&resource.sha256)
        || !matches!(resource.media_type.as_str(), "text/html" | "image/svg+xml")
    {
        return Err(PackageError::InvalidManifest(
            "UI resources must be checksum-bound HTML/SVG in an opaque origin".to_string(),
        ));
    }
    for origin in &resource.declared_network_origins {
        if validate_origin(origin)? != *origin {
            return Err(PackageError::InvalidManifest(
                "UI network origin must be canonical".to_string(),
            ));
        }
    }
    for action in &resource.declared_host_actions {
        validate_id("UI host action", action)?;
    }
    Ok(())
}

fn validate_connector(
    connector: &ConnectorDeclaration,
    permissions: &BTreeSet<PackagePermission>,
) -> PackageResult<()> {
    if connector.contract_version != CONNECTOR_CONTRACT_VERSION || connector.operations.is_empty() {
        return Err(PackageError::InvalidManifest(
            "invalid connector contract version or empty operation catalog".to_string(),
        ));
    }
    for origin in &connector.allowed_origins {
        if validate_origin(origin)? != *origin {
            return Err(PackageError::InvalidManifest(
                "connector origin must be canonical".to_string(),
            ));
        }
    }
    if connector.kind != ConnectorKind::FilesystemEvents && connector.allowed_origins.is_empty() {
        return Err(PackageError::InvalidManifest(
            "network connector requires an explicit origin allowlist".to_string(),
        ));
    }
    let permission_ids = permissions
        .iter()
        .map(|permission| permission.permission_id.as_str())
        .collect::<HashSet<_>>();
    let mut operation_ids = HashSet::new();
    for operation in &connector.operations {
        validate_id("connector operation id", &operation.operation_id)?;
        if !operation_ids.insert(operation.operation_id.as_str())
            || !matches!(
                operation.method.as_str(),
                "GET" | "HEAD" | "POST" | "PUT" | "PATCH" | "DELETE" | "LOCAL_EVENT"
            )
            || operation.path_template.is_empty()
            || operation.path_template.len() > 1_024
            || operation.path_template.contains("://")
            || operation
                .required_permission_ids
                .iter()
                .any(|permission| !permission_ids.contains(permission.as_str()))
        {
            return Err(PackageError::InvalidManifest(format!(
                "invalid connector operation {}",
                operation.operation_id
            )));
        }
        if operation.effect == ConnectorEffect::ExternalMutation && operation.method == "GET" {
            return Err(PackageError::InvalidManifest(
                "GET operations cannot declare an external mutation".to_string(),
            ));
        }
    }
    Ok(())
}

fn validate_assistant(
    assistant: &AssistantComposition,
    content: &[ContentReference],
) -> PackageResult<()> {
    validate_relative_path(&assistant.persona_content_path)?;
    if !content.iter().any(|reference| {
        reference.path == assistant.persona_content_path && reference.kind == ContentKind::Persona
    }) {
        return Err(PackageError::InvalidManifest(
            "assistant persona path must reference declared persona content".to_string(),
        ));
    }
    for package_id in &assistant.skill_package_ids {
        validate_package_id(package_id)?;
    }
    let mut workflows = BTreeSet::new();
    for path in &assistant.starter_workflow_paths {
        validate_relative_path(path)?;
        if !workflows.insert(path)
            || !content.iter().any(|reference| {
                reference.path == *path && reference.kind == ContentKind::WorkflowTemplate
            })
        {
            return Err(PackageError::InvalidManifest(format!(
                "assistant starter workflow must uniquely reference declared workflow content: {path}"
            )));
        }
    }
    if let Some(path) = &assistant.knowledge_template_path {
        validate_relative_path(path)?;
        if !content.iter().any(|reference| {
            reference.path == *path && reference.kind == ContentKind::KnowledgeTemplate
        }) {
            return Err(PackageError::InvalidManifest(
                "assistant knowledge path must reference declared knowledge-template content"
                    .to_string(),
            ));
        }
    }
    Ok(())
}

fn validate_permission(permission: &PackagePermission) -> PackageResult<()> {
    validate_id("permission id", &permission.permission_id)?;
    if permission.scope.is_empty()
        || permission.scope.len() > 1_024
        || permission.reason.trim().is_empty()
        || permission.reason.len() > 2_048
    {
        return Err(PackageError::InvalidManifest(format!(
            "invalid permission {}",
            permission.permission_id
        )));
    }
    if permission.kind == PermissionKind::Network
        && validate_origin(&permission.scope)? != permission.scope
    {
        return Err(PackageError::InvalidManifest(
            "network permission scope must be a canonical HTTPS origin".to_string(),
        ));
    }
    Ok(())
}

fn validate_vulnerability_notice(notice: &VulnerabilityNotice) -> PackageResult<()> {
    validate_id("vulnerability notice id", &notice.notice_id)?;
    if notice.summary.trim().is_empty()
        || notice.summary.len() > 2_048
        || notice.affected_versions.is_empty()
    {
        return Err(PackageError::InvalidManifest(format!(
            "invalid vulnerability notice: {}",
            notice.notice_id
        )));
    }
    if let Some(advisory_url) = &notice.advisory_url {
        let url = Url::parse(advisory_url).map_err(|error| {
            PackageError::InvalidManifest(format!("invalid advisory URL: {error}"))
        })?;
        if url.scheme() != "https" {
            return Err(PackageError::InvalidManifest(
                "advisory URL must be HTTPS".to_string(),
            ));
        }
    }
    Ok(())
}

fn validate_compatibility(compatibility: &Compatibility) -> PackageResult<()> {
    if compatibility
        .maximum_app_version_exclusive
        .is_some_and(|maximum| maximum <= compatibility.minimum_app_version)
        || compatibility.platforms.is_empty()
        || compatibility.architectures.is_empty()
        || compatibility.platforms.iter().any(|value| value.len() > 64)
        || compatibility
            .architectures
            .iter()
            .any(|value| value.len() > 64)
    {
        return Err(PackageError::InvalidManifest(
            "invalid package compatibility range".to_string(),
        ));
    }
    Ok(())
}

fn validate_provenance(provenance: &PackageProvenance) -> PackageResult<()> {
    if provenance.publisher.trim().is_empty()
        || provenance.publisher.len() > 256
        || provenance.source_revision.is_empty()
        || provenance.source_revision.len() > 512
    {
        return Err(PackageError::InvalidManifest(
            "invalid package provenance".to_string(),
        ));
    }
    match &provenance.source {
        InstallSource::LocalFolder { canonical_path } => {
            let path = Path::new(canonical_path);
            if !path.is_absolute()
                || path
                    .components()
                    .any(|component| matches!(component, Component::ParentDir | Component::CurDir))
            {
                return Err(PackageError::InvalidManifest(
                    "local provenance path must be absolute and normalized".to_string(),
                ));
            }
        }
        InstallSource::Git { remote, commit_sha } => {
            let url = Url::parse(remote).map_err(|error| {
                PackageError::InvalidManifest(format!("invalid Git provenance: {error}"))
            })?;
            if !matches!(url.scheme(), "https" | "ssh")
                || commit_sha.len() != 40
                || !commit_sha.bytes().all(|byte| byte.is_ascii_hexdigit())
            {
                return Err(PackageError::InvalidManifest(
                    "Git provenance needs HTTPS/SSH and a full commit SHA".to_string(),
                ));
            }
        }
        InstallSource::CuratedRegistry { registry_id } => {
            validate_id("registry id", registry_id)?;
        }
    }
    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PackageBundle {
    pub manifest: PackageManifest,
    pub files: BTreeMap<String, Vec<u8>>,
}

impl PackageBundle {
    pub fn validate(&self, limits: &PackageLimits) -> PackageResult<String> {
        self.manifest.validate(limits)?;
        if self.files.len() != self.manifest.file_checksums.len() {
            return Err(PackageError::InvalidBundle(
                "bundle file set differs from manifest".to_string(),
            ));
        }
        let mut total = 0_u64;
        for (path, expected) in &self.manifest.file_checksums {
            validate_relative_path(path)?;
            let bytes = self.files.get(path).ok_or_else(|| {
                PackageError::InvalidBundle(format!("missing declared file: {path}"))
            })?;
            if bytes.len() as u64 > limits.max_file_bytes {
                return Err(PackageError::LimitExceeded(format!(
                    "{path} exceeds per-file limit"
                )));
            }
            total = total
                .checked_add(bytes.len() as u64)
                .ok_or_else(|| PackageError::LimitExceeded("bundle size overflow".to_string()))?;
            if total > limits.max_total_bytes {
                return Err(PackageError::LimitExceeded(
                    "bundle exceeds total size limit".to_string(),
                ));
            }
            if sha256(bytes) != *expected {
                return Err(PackageError::InvalidBundle(format!(
                    "checksum mismatch: {path}"
                )));
            }
            reject_executable_payload(path, bytes)?;
        }
        let mut digest = Sha256::new();
        digest.update(serde_json::to_vec(&self.manifest)?);
        for (path, bytes) in &self.files {
            digest.update((path.len() as u64).to_le_bytes());
            digest.update(path.as_bytes());
            digest.update((bytes.len() as u64).to_le_bytes());
            digest.update(bytes);
        }
        Ok(format!("{:x}", digest.finalize()))
    }
}

fn reject_executable_payload(path: &str, bytes: &[u8]) -> PackageResult<()> {
    let extension = Path::new(path)
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    if matches!(
        extension.as_str(),
        "exe"
            | "dll"
            | "so"
            | "dylib"
            | "bin"
            | "app"
            | "sh"
            | "bash"
            | "zsh"
            | "fish"
            | "bat"
            | "cmd"
            | "ps1"
            | "com"
            | "msi"
            | "wasm"
    ) || bytes.starts_with(b"#!")
        || bytes.starts_with(b"\x7fELF")
        || bytes.starts_with(b"MZ")
        || matches!(
            bytes.get(..4),
            Some([0xfe, 0xed, 0xfa, 0xce] | [0xcf, 0xfa, 0xed, 0xfe])
        )
    {
        return Err(PackageError::InvalidBundle(format!(
            "executable payloads are outside the declarative package format: {path}"
        )));
    }
    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct TrustedKey {
    pub key_id: String,
    pub algorithm: String,
    pub public_key_hex: String,
    pub valid_from_unix_ms: u64,
    pub valid_until_unix_ms: u64,
    pub revoked_at_unix_ms: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct TrustRoot {
    pub trust_root_id: String,
    pub publisher: String,
    pub package_namespaces: BTreeSet<String>,
    pub keys: BTreeMap<String, TrustedKey>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct TrustStore {
    pub schema_version: u32,
    pub roots: BTreeMap<String, TrustRoot>,
}

impl TrustStore {
    pub fn validate(&self) -> PackageResult<()> {
        if self.schema_version != TRUST_STORE_VERSION || self.roots.is_empty() {
            return Err(PackageError::Untrusted(
                "invalid or empty trust store".to_string(),
            ));
        }
        for (root_id, root) in &self.roots {
            validate_id("trust root id", root_id)?;
            if root.trust_root_id != *root_id
                || root.publisher.trim().is_empty()
                || root.package_namespaces.is_empty()
                || root.keys.is_empty()
            {
                return Err(PackageError::Untrusted(format!(
                    "invalid trust root: {root_id}"
                )));
            }
            for namespace in &root.package_namespaces {
                if !namespace.ends_with('.') {
                    return Err(PackageError::Untrusted(
                        "trust namespaces must end with '.'".to_string(),
                    ));
                }
            }
            for (key_id, key) in &root.keys {
                validate_id("key id", key_id)?;
                if key.key_id != *key_id
                    || key.algorithm.is_empty()
                    || decode_hex(&key.public_key_hex)?.is_empty()
                    || key.valid_until_unix_ms <= key.valid_from_unix_ms
                {
                    return Err(PackageError::Untrusted(format!(
                        "invalid trusted key: {key_id}"
                    )));
                }
            }
        }
        Ok(())
    }
}

pub trait SignatureVerifier: Send + Sync {
    fn verify(
        &self,
        algorithm: &str,
        public_key: &[u8],
        message: &[u8],
        signature: &[u8],
    ) -> Result<bool, String>;
}

/// Production verifier for the only signature algorithm accepted by the
/// bundled Little Monkey registry. Keeping algorithm dispatch explicit avoids
/// silent downgrade to a weaker or differently encoded signature scheme.
#[derive(Debug, Default)]
pub struct RingEd25519SignatureVerifier;

impl SignatureVerifier for RingEd25519SignatureVerifier {
    fn verify(
        &self,
        algorithm: &str,
        public_key: &[u8],
        message: &[u8],
        signature: &[u8],
    ) -> Result<bool, String> {
        if algorithm != "ed25519" {
            return Err(format!(
                "unsupported package signature algorithm: {algorithm}"
            ));
        }
        if public_key.len() != 32 || signature.len() != 64 {
            return Ok(false);
        }
        Ok(
            ring::signature::UnparsedPublicKey::new(&ring::signature::ED25519, public_key)
                .verify(message, signature)
                .is_ok(),
        )
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RevocationTarget {
    TrustRoot {
        trust_root_id: String,
    },
    SigningKey {
        trust_root_id: String,
        key_id: String,
    },
    Package {
        package_id: String,
    },
    PackageVersion {
        package_id: String,
        version: SemanticVersion,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RevocationEntry {
    pub revocation_id: String,
    pub target: RevocationTarget,
    pub effective_unix_ms: u64,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RegistrySignature {
    pub trust_root_id: String,
    pub key_id: String,
    pub algorithm: String,
    pub signature_hex: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RegistryPackageVersion {
    pub version: SemanticVersion,
    pub bundle_sha256: String,
    pub manifest_sha256: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RegistrySnapshot {
    pub schema_version: u32,
    pub registry_id: String,
    pub sequence: u64,
    pub generated_unix_ms: u64,
    pub refresh_after_unix_ms: u64,
    pub expires_unix_ms: u64,
    pub packages: BTreeMap<String, Vec<RegistryPackageVersion>>,
    pub revocations: Vec<RevocationEntry>,
    pub signature: RegistrySignature,
}

impl RegistrySnapshot {
    pub fn signing_payload(&self) -> PackageResult<Vec<u8>> {
        let mut unsigned = self.clone();
        unsigned.signature.signature_hex.clear();
        Ok(serde_json::to_vec(&unsigned)?)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct VerifiedRegistryState {
    snapshot: RegistrySnapshot,
    verified_unix_ms: u64,
    snapshot_sha256: String,
}

impl VerifiedRegistryState {
    pub fn snapshot(&self) -> &RegistrySnapshot {
        &self.snapshot
    }

    pub fn verified_unix_ms(&self) -> u64 {
        self.verified_unix_ms
    }

    pub fn snapshot_sha256(&self) -> &str {
        &self.snapshot_sha256
    }
}

pub fn verify_registry_snapshot(
    snapshot: &RegistrySnapshot,
    trust_store: &TrustStore,
    previous: Option<&VerifiedRegistryState>,
    verifier: &dyn SignatureVerifier,
    now_unix_ms: u64,
) -> PackageResult<VerifiedRegistryState> {
    trust_store.validate()?;
    if snapshot.schema_version != REGISTRY_SNAPSHOT_VERSION
        || snapshot.registry_id.is_empty()
        || snapshot.sequence == 0
        || snapshot.refresh_after_unix_ms < snapshot.generated_unix_ms
        || snapshot.expires_unix_ms < snapshot.refresh_after_unix_ms
        || now_unix_ms < snapshot.generated_unix_ms
        || previous.is_some_and(|state| {
            state.snapshot.registry_id == snapshot.registry_id
                && state.snapshot.sequence >= snapshot.sequence
        })
    {
        return Err(PackageError::Untrusted(
            "registry metadata is invalid, from the future, or rolled back".to_string(),
        ));
    }
    let key = trusted_key(
        trust_store,
        &snapshot.signature.trust_root_id,
        &snapshot.signature.key_id,
        &snapshot.signature.algorithm,
        now_unix_ms,
    )?;
    let public_key = decode_hex(&key.public_key_hex)?;
    let signature = decode_hex(&snapshot.signature.signature_hex)?;
    if signature.is_empty() {
        return Err(PackageError::Untrusted(
            "registry signature cannot be empty".to_string(),
        ));
    }
    let payload = snapshot.signing_payload()?;
    if !verifier
        .verify(&key.algorithm, &public_key, &payload, &signature)
        .map_err(PackageError::Verifier)?
    {
        return Err(PackageError::Untrusted(
            "registry signature verification failed".to_string(),
        ));
    }
    for (package_id, versions) in &snapshot.packages {
        validate_package_id(package_id)?;
        let mut seen = HashSet::new();
        for version in versions {
            if !seen.insert(version.version)
                || !is_sha256(&version.bundle_sha256)
                || !is_sha256(&version.manifest_sha256)
            {
                return Err(PackageError::Untrusted(
                    "registry package catalog contains malformed data".to_string(),
                ));
            }
        }
    }
    for revocation in &snapshot.revocations {
        validate_id("revocation id", &revocation.revocation_id)?;
        if revocation.reason.trim().is_empty() || revocation.reason.len() > 2_048 {
            return Err(PackageError::Untrusted(
                "registry revocation has no bounded reason".to_string(),
            ));
        }
    }
    Ok(VerifiedRegistryState {
        snapshot: snapshot.clone(),
        verified_unix_ms: now_unix_ms,
        snapshot_sha256: sha256(&serde_json::to_vec(snapshot)?),
    })
}

fn trusted_key<'a>(
    trust_store: &'a TrustStore,
    root_id: &str,
    key_id: &str,
    algorithm: &str,
    now_unix_ms: u64,
) -> PackageResult<&'a TrustedKey> {
    let root = trust_store
        .roots
        .get(root_id)
        .ok_or_else(|| PackageError::Untrusted(format!("unknown trust root: {root_id}")))?;
    let key = root
        .keys
        .get(key_id)
        .ok_or_else(|| PackageError::Untrusted(format!("unknown signing key: {key_id}")))?;
    if key.algorithm != algorithm
        || now_unix_ms < key.valid_from_unix_ms
        || now_unix_ms >= key.valid_until_unix_ms
        || key
            .revoked_at_unix_ms
            .is_some_and(|revoked| revoked <= now_unix_ms)
    {
        return Err(PackageError::Untrusted(
            "signing key is invalid, expired, or revoked".to_string(),
        ));
    }
    Ok(key)
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RegistryFreshness {
    Fresh,
    RefreshRecommended,
    Expired,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum RevocationKnowledge {
    NotRevokedAsOf {
        registry_sequence: u64,
        generated_unix_ms: u64,
        freshness: RegistryFreshness,
    },
    Revoked {
        revocation_id: String,
        effective_unix_ms: u64,
        reason: String,
    },
    UnknownNeverDownloaded,
}

fn registry_freshness(snapshot: &RegistrySnapshot, now_unix_ms: u64) -> RegistryFreshness {
    if now_unix_ms >= snapshot.expires_unix_ms {
        RegistryFreshness::Expired
    } else if now_unix_ms >= snapshot.refresh_after_unix_ms {
        RegistryFreshness::RefreshRecommended
    } else {
        RegistryFreshness::Fresh
    }
}

fn revocation_knowledge(
    manifest: &PackageManifest,
    registry: Option<&VerifiedRegistryState>,
    now_unix_ms: u64,
) -> RevocationKnowledge {
    let Some(registry) = registry else {
        return RevocationKnowledge::UnknownNeverDownloaded;
    };
    for entry in &registry.snapshot.revocations {
        if entry.effective_unix_ms > now_unix_ms {
            continue;
        }
        let matches = match &entry.target {
            RevocationTarget::TrustRoot { trust_root_id } => manifest
                .signature
                .as_ref()
                .is_some_and(|signature| &signature.trust_root_id == trust_root_id),
            RevocationTarget::SigningKey {
                trust_root_id,
                key_id,
            } => manifest.signature.as_ref().is_some_and(|signature| {
                &signature.trust_root_id == trust_root_id && &signature.key_id == key_id
            }),
            RevocationTarget::Package { package_id } => &manifest.package_id == package_id,
            RevocationTarget::PackageVersion {
                package_id,
                version,
            } => &manifest.package_id == package_id && manifest.version == *version,
        };
        if matches {
            return RevocationKnowledge::Revoked {
                revocation_id: entry.revocation_id.clone(),
                effective_unix_ms: entry.effective_unix_ms,
                reason: entry.reason.clone(),
            };
        }
    }
    RevocationKnowledge::NotRevokedAsOf {
        registry_sequence: registry.snapshot.sequence,
        generated_unix_ms: registry.snapshot.generated_unix_ms,
        freshness: registry_freshness(&registry.snapshot, now_unix_ms),
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct InstallEnvironment {
    pub app_version: SemanticVersion,
    pub platform: String,
    pub architecture: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct InstallTrustPolicy {
    pub allow_unsigned_local_folders: bool,
    pub allow_unsigned_git: bool,
    pub require_registry_catalog_match: bool,
    pub permit_expired_offline_registry: bool,
}

impl Default for InstallTrustPolicy {
    fn default() -> Self {
        Self {
            allow_unsigned_local_folders: true,
            allow_unsigned_git: false,
            require_registry_catalog_match: true,
            permit_expired_offline_registry: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TrustEvidence {
    pub signed: bool,
    pub trust_root_id: Option<String>,
    pub key_id: Option<String>,
    pub registry_snapshot_sha256: Option<String>,
    pub revocation: RevocationKnowledge,
}

#[derive(Debug, Clone)]
pub struct VerifiedPackage {
    bundle: PackageBundle,
    bundle_sha256: String,
    trust: TrustEvidence,
}

impl VerifiedPackage {
    pub fn bundle(&self) -> &PackageBundle {
        &self.bundle
    }

    pub fn manifest(&self) -> &PackageManifest {
        &self.bundle.manifest
    }

    pub fn bundle_sha256(&self) -> &str {
        &self.bundle_sha256
    }

    pub fn trust(&self) -> &TrustEvidence {
        &self.trust
    }
}

pub fn verify_package(
    bundle: &PackageBundle,
    trust_store: &TrustStore,
    registry: Option<&VerifiedRegistryState>,
    environment: &InstallEnvironment,
    policy: &InstallTrustPolicy,
    limits: &PackageLimits,
    verifier: &dyn SignatureVerifier,
    now_unix_ms: u64,
) -> PackageResult<VerifiedPackage> {
    trust_store.validate()?;
    let bundle_sha256 = bundle.validate(limits)?;
    let manifest = &bundle.manifest;
    if environment.app_version < manifest.compatibility.minimum_app_version
        || manifest
            .compatibility
            .maximum_app_version_exclusive
            .is_some_and(|maximum| environment.app_version >= maximum)
        || !manifest
            .compatibility
            .platforms
            .contains(&environment.platform)
        || !manifest
            .compatibility
            .architectures
            .contains(&environment.architecture)
    {
        return Err(PackageError::Incompatible(format!(
            "{} {} does not support this app/platform/architecture",
            manifest.package_id, manifest.version
        )));
    }
    let revocation = revocation_knowledge(manifest, registry, now_unix_ms);
    if let RevocationKnowledge::Revoked { reason, .. } = &revocation {
        return Err(PackageError::Revoked(reason.clone()));
    }
    if matches!(
        &revocation,
        RevocationKnowledge::NotRevokedAsOf {
            freshness: RegistryFreshness::Expired,
            ..
        }
    ) && !policy.permit_expired_offline_registry
    {
        return Err(PackageError::Untrusted(
            "last verified revocation metadata has expired".to_string(),
        ));
    }
    let (signed, trust_root_id, key_id) = if let Some(signature) = &manifest.signature {
        let root = trust_store
            .roots
            .get(&signature.trust_root_id)
            .ok_or_else(|| {
                PackageError::Untrusted(format!("unknown trust root: {}", signature.trust_root_id))
            })?;
        if !root
            .package_namespaces
            .iter()
            .any(|namespace| manifest.package_id.starts_with(namespace))
            || root.publisher != manifest.provenance.publisher
        {
            return Err(PackageError::Untrusted(
                "publisher or package namespace is outside trust-root authority".to_string(),
            ));
        }
        let key = trusted_key(
            trust_store,
            &signature.trust_root_id,
            &signature.key_id,
            &signature.algorithm,
            now_unix_ms,
        )?;
        let verified = verifier
            .verify(
                &signature.algorithm,
                &decode_hex(&key.public_key_hex)?,
                &manifest.signing_payload()?,
                &decode_hex(&signature.signature_hex)?,
            )
            .map_err(PackageError::Verifier)?;
        if !verified {
            return Err(PackageError::Untrusted(
                "package signature verification failed".to_string(),
            ));
        }
        (
            true,
            Some(signature.trust_root_id.clone()),
            Some(signature.key_id.clone()),
        )
    } else {
        let allowed = match &manifest.provenance.source {
            InstallSource::LocalFolder { .. } => policy.allow_unsigned_local_folders,
            InstallSource::Git { .. } => policy.allow_unsigned_git,
            InstallSource::CuratedRegistry { .. } => false,
        };
        if !allowed {
            return Err(PackageError::Untrusted(
                "this installation source requires a trusted signature".to_string(),
            ));
        }
        (false, None, None)
    };
    if let InstallSource::CuratedRegistry { registry_id } = &manifest.provenance.source {
        let registry = registry.ok_or_else(|| {
            PackageError::Untrusted("curated install needs verified registry metadata".to_string())
        })?;
        if registry.snapshot.registry_id != *registry_id {
            return Err(PackageError::Untrusted(
                "package provenance registry differs from verified registry".to_string(),
            ));
        }
        if policy.require_registry_catalog_match {
            let manifest_sha = sha256(&serde_json::to_vec(manifest)?);
            let catalog_match = registry
                .snapshot
                .packages
                .get(&manifest.package_id)
                .is_some_and(|versions| {
                    versions.iter().any(|version| {
                        version.version == manifest.version
                            && version.bundle_sha256 == bundle_sha256
                            && version.manifest_sha256 == manifest_sha
                    })
                });
            if !catalog_match {
                return Err(PackageError::Untrusted(
                    "bundle does not match the signed registry catalog".to_string(),
                ));
            }
        }
    }
    Ok(VerifiedPackage {
        bundle: bundle.clone(),
        bundle_sha256,
        trust: TrustEvidence {
            signed,
            trust_root_id,
            key_id,
            registry_snapshot_sha256: registry.map(|state| state.snapshot_sha256.clone()),
            revocation,
        },
    })
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PermissionDiff {
    pub added: BTreeSet<PackagePermission>,
    pub removed: BTreeSet<PackagePermission>,
    pub unchanged: BTreeSet<PackagePermission>,
    pub approval_digest: String,
    pub requires_new_approval: bool,
}

pub fn permission_diff(
    previous: &BTreeSet<PackagePermission>,
    next: &BTreeSet<PackagePermission>,
) -> PackageResult<PermissionDiff> {
    let added = next.difference(previous).cloned().collect::<BTreeSet<_>>();
    let removed = previous.difference(next).cloned().collect::<BTreeSet<_>>();
    let unchanged = previous
        .intersection(next)
        .cloned()
        .collect::<BTreeSet<_>>();
    let approval_digest = sha256(&serde_json::to_vec(&added)?);
    Ok(PermissionDiff {
        requires_new_approval: !added.is_empty(),
        added,
        removed,
        unchanged,
        approval_digest,
    })
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct InstallPreview {
    pub package_id: String,
    pub version: SemanticVersion,
    pub kind: PackageKind,
    pub source: InstallSource,
    pub bundle_sha256: String,
    pub trust: TrustEvidence,
    pub permissions: BTreeSet<PackagePermission>,
    pub permission_diff: Option<PermissionDiff>,
    pub mcp_actions_separate: Vec<McpRequirement>,
    pub file_count: usize,
    pub total_bytes: u64,
    pub warnings: Vec<String>,
}

pub fn install_preview(
    package: &VerifiedPackage,
    installed: Option<&InstalledPackageState>,
) -> PackageResult<InstallPreview> {
    let manifest = package.manifest();
    let permission_diff = installed
        .map(|state| permission_diff(&state.approved_permissions, &manifest.permissions))
        .transpose()?;
    let mut warnings = Vec::new();
    if !package.trust.signed {
        warnings.push(
            "This is an unsigned local data-only package. Review every file digest and permission before installing."
                .to_string(),
        );
    }
    match &package.trust.revocation {
        RevocationKnowledge::UnknownNeverDownloaded => warnings
            .push("Verified registry/revocation metadata has never been downloaded".to_string()),
        RevocationKnowledge::NotRevokedAsOf {
            freshness: RegistryFreshness::RefreshRecommended,
            generated_unix_ms,
            ..
        } => warnings.push(format!(
            "Revocation metadata is stale; last known generated time is {generated_unix_ms}"
        )),
        RevocationKnowledge::NotRevokedAsOf {
            freshness: RegistryFreshness::Expired,
            generated_unix_ms,
            ..
        } => warnings.push(format!(
            "Revocation metadata is expired; offline status is only known as of {generated_unix_ms}"
        )),
        _ => {}
    }
    if !manifest.mcp_requirements.is_empty() {
        warnings.push(
            "MCP installation/configuration and OAuth are not performed by package install"
                .to_string(),
        );
    }
    Ok(InstallPreview {
        package_id: manifest.package_id.clone(),
        version: manifest.version,
        kind: manifest.kind,
        source: manifest.provenance.source.clone(),
        bundle_sha256: package.bundle_sha256.clone(),
        trust: package.trust.clone(),
        permissions: manifest.permissions.clone(),
        permission_diff,
        mcp_actions_separate: manifest.mcp_requirements.clone(),
        file_count: package.bundle.files.len(),
        total_bytes: package
            .bundle
            .files
            .values()
            .map(|bytes| bytes.len() as u64)
            .sum(),
        warnings,
    })
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PermissionApproval {
    pub package_id: String,
    pub from_version: SemanticVersion,
    pub to_version: SemanticVersion,
    pub approval_digest: String,
    pub approved: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CachedVersion {
    pub version: SemanticVersion,
    pub bundle_sha256: String,
    pub trust: TrustEvidence,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct InstalledPackageState {
    pub schema_version: u32,
    pub sequence: u64,
    pub package_id: String,
    pub active_version: Option<SemanticVersion>,
    pub versions: BTreeMap<SemanticVersion, CachedVersion>,
    pub activation_history: Vec<SemanticVersion>,
    pub pinned_version: Option<SemanticVersion>,
    pub enabled: bool,
    pub revoked: bool,
    pub tombstoned: bool,
    pub approved_permissions: BTreeSet<PackagePermission>,
    /// Local-only install counter: incremented once per successful
    /// `PackageStore::install` call for this package_id, including
    /// reinstalls after an uninstall. There is no hosted telemetry backend
    /// in this app — this number is never transmitted anywhere and reflects
    /// only this device's own install history. Field-defaulted so state
    /// files written before this counter existed keep deserializing.
    #[serde(default)]
    pub local_install_count: u64,
    /// Locally user-set flag marking this package (in practice, a
    /// `PackageKind::Collection`) as approved by the team. This is a plain
    /// boolean toggle with no role/permission check of its own, kept fully
    /// independent so a separate "Team Mode" feature — present or not in
    /// this build — never has a hard dependency on this field.
    #[serde(default)]
    pub team_approved: bool,
}

impl InstalledPackageState {
    fn validate(&self) -> PackageResult<()> {
        if self.schema_version != PACKAGE_STATE_VERSION || self.sequence == 0 {
            return Err(PackageError::Conflict(
                "invalid installed package state".to_string(),
            ));
        }
        validate_package_id(&self.package_id)?;
        if let Some(active) = self.active_version {
            if !self.versions.contains_key(&active) || self.tombstoned {
                return Err(PackageError::Conflict(
                    "active version is missing or package is tombstoned".to_string(),
                ));
            }
        }
        if self
            .pinned_version
            .is_some_and(|version| !self.versions.contains_key(&version))
            || self.versions.iter().any(|(version, cached)| {
                *version != cached.version || !is_sha256(&cached.bundle_sha256)
            })
        {
            return Err(PackageError::Conflict(
                "installed version/pin metadata is inconsistent".to_string(),
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PortablePackageExport {
    pub schema_version: u32,
    pub bundle_sha256: String,
    pub manifest: PackageManifest,
    pub files_hex: BTreeMap<String, String>,
}

impl PortablePackageExport {
    pub fn into_bundle(self, limits: &PackageLimits) -> PackageResult<PackageBundle> {
        if self.schema_version != PORTABLE_EXPORT_VERSION {
            return Err(PackageError::InvalidBundle(
                "unsupported portable export version".to_string(),
            ));
        }
        let files = self
            .files_hex
            .into_iter()
            .map(|(path, value)| Ok((path, decode_hex(&value)?)))
            .collect::<PackageResult<BTreeMap<_, _>>>()?;
        let bundle = PackageBundle {
            manifest: self.manifest,
            files,
        };
        let digest = bundle.validate(limits)?;
        if digest != self.bundle_sha256 {
            return Err(PackageError::InvalidBundle(
                "portable export digest mismatch".to_string(),
            ));
        }
        Ok(bundle)
    }
}

/// One user-added registry source: the roadmap's "private/team catalog".
/// Only the audit-facing location (URL or local path) is stored here — the
/// actual snapshot bytes always come from an explicit caller argument and
/// must pass through [`verify_registry_snapshot`] (the same Ed25519 chain
/// used by the built-in first-party registry) before any package from it is
/// considered verified.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AdditionalRegistrySource {
    pub source_id: String,
    pub display_name: String,
    pub location: String,
    pub added_unix_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AdditionalRegistryRecord {
    pub source: AdditionalRegistrySource,
    pub verified: Option<VerifiedRegistryState>,
    pub last_verification_error: Option<String>,
}

const ADDITIONAL_REGISTRIES_SCHEMA_VERSION: u32 = 1;
const REGISTRIES_FILE: &str = "registries.json";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct AdditionalRegistryFile {
    schema_version: u32,
    sources: BTreeMap<String, AdditionalRegistryRecord>,
}

impl Default for AdditionalRegistryFile {
    fn default() -> Self {
        Self {
            schema_version: ADDITIONAL_REGISTRIES_SCHEMA_VERSION,
            sources: BTreeMap::new(),
        }
    }
}

#[derive(Debug)]
pub struct PackageStore {
    root: PathBuf,
    gate: Mutex<()>,
}

impl PackageStore {
    pub fn new(root: impl AsRef<Path>) -> PackageResult<Self> {
        let root = root.as_ref();
        if root.exists() && fs::symlink_metadata(root)?.file_type().is_symlink() {
            return Err(PackageError::Io(
                "package-store root cannot be a symlink".to_string(),
            ));
        }
        fs::create_dir_all(root)?;
        let root = fs::canonicalize(root)?;
        for child in [CACHE_DIR, STATE_DIR] {
            let path = root.join(child);
            if path.exists() && fs::symlink_metadata(&path)?.file_type().is_symlink() {
                return Err(PackageError::Io(format!(
                    "package-store directory cannot be a symlink: {}",
                    path.display()
                )));
            }
            fs::create_dir_all(path)?;
        }
        Ok(Self {
            root,
            gate: Mutex::new(()),
        })
    }

    pub fn install(&self, package: &VerifiedPackage) -> PackageResult<InstalledPackageState> {
        let _guard = self.lock()?;
        let package_id = &package.manifest().package_id;
        let previous = self.load_state_unlocked(package_id)?;
        if previous.as_ref().is_some_and(|state| !state.tombstoned) {
            return Err(PackageError::Conflict(format!(
                "{package_id} is already installed"
            )));
        }
        self.cache_unlocked(package)?;
        let version = package.manifest().version;
        let cached = CachedVersion {
            version,
            bundle_sha256: package.bundle_sha256.clone(),
            trust: package.trust.clone(),
        };
        let local_install_count = previous
            .as_ref()
            .map_or(1, |state| state.local_install_count.saturating_add(1));
        let team_approved = previous.as_ref().is_some_and(|state| state.team_approved);
        let state = InstalledPackageState {
            schema_version: PACKAGE_STATE_VERSION,
            sequence: self.next_sequence_unlocked(package_id)?,
            package_id: package_id.clone(),
            active_version: Some(version),
            versions: BTreeMap::from([(version, cached)]),
            activation_history: vec![version],
            pinned_version: None,
            enabled: true,
            revoked: false,
            tombstoned: false,
            approved_permissions: package.manifest().permissions.clone(),
            local_install_count,
            team_approved,
        };
        self.write_state_unlocked(&state)?;
        Ok(state)
    }

    pub fn update(
        &self,
        package: &VerifiedPackage,
        approval: Option<&PermissionApproval>,
    ) -> PackageResult<InstalledPackageState> {
        let _guard = self.lock()?;
        let package_id = &package.manifest().package_id;
        let mut state = self
            .load_state_unlocked(package_id)?
            .filter(|state| !state.tombstoned)
            .ok_or_else(|| PackageError::NotInstalled(package_id.clone()))?;
        let active = state.active_version.ok_or_else(|| {
            PackageError::Conflict("installed package has no active version".to_string())
        })?;
        let target = package.manifest().version;
        if target <= active {
            return Err(PackageError::Conflict(format!(
                "update target {target} is not newer than {active}"
            )));
        }
        if state.pinned_version.is_some_and(|pinned| pinned != target) {
            return Err(PackageError::Pinned(format!(
                "{} is pinned to {}",
                state.package_id,
                state.pinned_version.expect("checked Some")
            )));
        }
        let diff = permission_diff(&state.approved_permissions, &package.manifest().permissions)?;
        if diff.requires_new_approval {
            let valid = approval.is_some_and(|approval| {
                approval.approved
                    && approval.package_id == state.package_id
                    && approval.from_version == active
                    && approval.to_version == target
                    && approval.approval_digest == diff.approval_digest
            });
            if !valid {
                return Err(PackageError::PermissionApprovalRequired(
                    diff.approval_digest,
                ));
            }
        }
        self.cache_unlocked(package)?;
        state.sequence = state.sequence.saturating_add(1);
        state.active_version = Some(target);
        state.versions.insert(
            target,
            CachedVersion {
                version: target,
                bundle_sha256: package.bundle_sha256.clone(),
                trust: package.trust.clone(),
            },
        );
        state.activation_history.push(target);
        state.approved_permissions = package.manifest().permissions.clone();
        state.enabled = true;
        state.revoked = false;
        state.validate()?;
        self.write_state_unlocked(&state)?;
        Ok(state)
    }

    pub fn set_enabled(
        &self,
        package_id: &str,
        enabled: bool,
    ) -> PackageResult<InstalledPackageState> {
        self.mutate_state(package_id, |state| {
            if state.revoked && enabled {
                return Err(PackageError::Revoked(
                    "revoked packages cannot be enabled".to_string(),
                ));
            }
            state.enabled = enabled;
            Ok(())
        })
    }

    /// Sets the local "team approved" toggle. This never checks a role or
    /// permission of its own: it is a plain, locally-observed flag intended
    /// for `PackageKind::Collection` packages, and the caller (the M4
    /// package service) is responsible for deciding which kinds it applies
    /// to. Kept independent from `enabled`/`revoked` so a future Team Mode
    /// feature can read it without this store depending on that feature.
    pub fn set_team_approved(
        &self,
        package_id: &str,
        team_approved: bool,
    ) -> PackageResult<InstalledPackageState> {
        self.mutate_state(package_id, |state| {
            state.team_approved = team_approved;
            Ok(())
        })
    }

    pub fn pin(
        &self,
        package_id: &str,
        version: Option<SemanticVersion>,
    ) -> PackageResult<InstalledPackageState> {
        self.mutate_state(package_id, |state| {
            if version.is_some_and(|version| !state.versions.contains_key(&version)) {
                return Err(PackageError::Conflict(
                    "cannot pin a version that is not cached for this package".to_string(),
                ));
            }
            state.pinned_version = version;
            Ok(())
        })
    }

    pub fn rollback(&self, package_id: &str) -> PackageResult<InstalledPackageState> {
        self.mutate_state(package_id, |state| {
            if state.activation_history.len() < 2 {
                return Err(PackageError::Conflict(
                    "no previous activated version to roll back to".to_string(),
                ));
            }
            state.activation_history.pop();
            let previous = *state
                .activation_history
                .last()
                .ok_or_else(|| PackageError::Conflict("rollback history empty".to_string()))?;
            if state
                .pinned_version
                .is_some_and(|pinned| pinned != previous)
            {
                return Err(PackageError::Pinned(
                    "rollback target differs from pinned version".to_string(),
                ));
            }
            state.active_version = Some(previous);
            let bundle = self.load_cached_bundle_unlocked(
                &state
                    .versions
                    .get(&previous)
                    .ok_or_else(|| PackageError::Conflict("rollback cache missing".to_string()))?
                    .bundle_sha256,
            )?;
            state.approved_permissions = bundle.manifest.permissions;
            state.enabled = true;
            Ok(())
        })
    }

    pub fn mark_revoked(&self, package_id: &str) -> PackageResult<InstalledPackageState> {
        self.mutate_state(package_id, |state| {
            state.revoked = true;
            state.enabled = false;
            Ok(())
        })
    }

    pub fn uninstall(&self, package_id: &str) -> PackageResult<InstalledPackageState> {
        self.mutate_state(package_id, |state| {
            state.active_version = None;
            state.enabled = false;
            state.tombstoned = true;
            state.approved_permissions.clear();
            Ok(())
        })
    }

    pub fn installed(&self, package_id: &str) -> PackageResult<Option<InstalledPackageState>> {
        validate_package_id(package_id)?;
        self.load_state_unlocked(package_id)
    }

    pub fn list_installed(&self) -> PackageResult<Vec<InstalledPackageState>> {
        let _guard = self.lock()?;
        let mut states = Vec::new();
        for entry in fs::read_dir(self.root.join(STATE_DIR))? {
            let entry = entry?;
            if !entry.file_type()?.is_dir() || entry.file_type()?.is_symlink() {
                continue;
            }
            let mut newest: Option<InstalledPackageState> = None;
            for state_entry in fs::read_dir(entry.path())? {
                let state_entry = state_entry?;
                if !state_entry.file_type()?.is_file() || state_entry.file_type()?.is_symlink() {
                    continue;
                }
                let name = state_entry.file_name().to_string_lossy().to_string();
                if !name.starts_with(STATE_PREFIX) || !name.ends_with(STATE_SUFFIX) {
                    continue;
                }
                let Ok(bytes) = fs::read(state_entry.path()) else {
                    continue;
                };
                let Ok(candidate) = serde_json::from_slice::<InstalledPackageState>(&bytes) else {
                    continue;
                };
                if candidate.validate().is_ok()
                    && newest
                        .as_ref()
                        .is_none_or(|current| candidate.sequence > current.sequence)
                {
                    newest = Some(candidate);
                }
            }
            if let Some(state) = newest {
                states.push(state);
            }
        }
        states.sort_by(|left, right| left.package_id.cmp(&right.package_id));
        Ok(states)
    }

    pub fn export_active(&self, package_id: &str) -> PackageResult<PortablePackageExport> {
        let state = self
            .load_state_unlocked(package_id)?
            .filter(|state| !state.tombstoned)
            .ok_or_else(|| PackageError::NotInstalled(package_id.to_string()))?;
        let active = state.active_version.ok_or_else(|| {
            PackageError::Conflict("installed package has no active version".to_string())
        })?;
        self.export_version_unlocked(&state, active)
    }

    /// Reads and revalidates one immutable cached version. This is used by
    /// the plugin health view to prove that a rollback target is still
    /// usable before offering the action to the user.
    pub fn export_version(
        &self,
        package_id: &str,
        version: SemanticVersion,
    ) -> PackageResult<PortablePackageExport> {
        let _guard = self.lock()?;
        let state = self
            .load_state_unlocked(package_id)?
            .filter(|state| !state.tombstoned)
            .ok_or_else(|| PackageError::NotInstalled(package_id.to_string()))?;
        self.export_version_unlocked(&state, version)
    }

    fn export_version_unlocked(
        &self,
        state: &InstalledPackageState,
        version: SemanticVersion,
    ) -> PackageResult<PortablePackageExport> {
        let cached = state.versions.get(&version).ok_or_else(|| {
            PackageError::Conflict(format!(
                "version {version} is not cached for {}",
                state.package_id
            ))
        })?;
        let bundle = self.load_cached_bundle_unlocked(&cached.bundle_sha256)?;
        Ok(PortablePackageExport {
            schema_version: PORTABLE_EXPORT_VERSION,
            bundle_sha256: cached.bundle_sha256.clone(),
            manifest: bundle.manifest,
            files_hex: bundle
                .files
                .into_iter()
                .map(|(path, bytes)| (path, encode_hex(&bytes)))
                .collect(),
        })
    }

    /// Lists every user-added registry source, verified or not.
    pub fn list_registry_sources(&self) -> PackageResult<Vec<AdditionalRegistryRecord>> {
        let _guard = self.lock()?;
        Ok(self
            .load_registry_file_unlocked()?
            .sources
            .into_values()
            .collect())
    }

    /// Registers a new registry source. This only records where the source
    /// claims to be from; no packages from it are trusted until a caller
    /// separately supplies a snapshot to [`Self::record_registry_verification`]
    /// (via the same Ed25519 verification chain as the built-in registry).
    pub fn add_registry_source(
        &self,
        source: AdditionalRegistrySource,
    ) -> PackageResult<AdditionalRegistryRecord> {
        validate_id("registry source id", &source.source_id)?;
        if source.display_name.trim().is_empty() || source.display_name.len() > 200 {
            return Err(PackageError::InvalidManifest(
                "registry source display name must be a bounded non-empty string".to_string(),
            ));
        }
        if source.location.is_empty() || source.location.len() > 2_048 {
            return Err(PackageError::InvalidManifest(
                "registry source location must be a bounded non-empty string".to_string(),
            ));
        }
        let _guard = self.lock()?;
        let mut file = self.load_registry_file_unlocked()?;
        if file.sources.contains_key(&source.source_id) {
            return Err(PackageError::Conflict(format!(
                "registry source {} already exists",
                source.source_id
            )));
        }
        let record = AdditionalRegistryRecord {
            source: source.clone(),
            verified: None,
            last_verification_error: None,
        };
        file.sources.insert(source.source_id, record.clone());
        self.write_registry_file_unlocked(&file)?;
        Ok(record)
    }

    pub fn remove_registry_source(&self, source_id: &str) -> PackageResult<bool> {
        let _guard = self.lock()?;
        let mut file = self.load_registry_file_unlocked()?;
        let removed = file.sources.remove(source_id).is_some();
        if removed {
            self.write_registry_file_unlocked(&file)?;
        }
        Ok(removed)
    }

    /// Persists the outcome of verifying a registry source's snapshot
    /// through [`verify_registry_snapshot`]. On verification failure the
    /// previous verified state (if any) is retained so a source that was
    /// trustworthy once does not silently lose its last-known-good snapshot
    /// just because a later fetch/paste failed to verify.
    pub fn record_registry_verification(
        &self,
        source_id: &str,
        verified: Option<VerifiedRegistryState>,
        error: Option<String>,
    ) -> PackageResult<AdditionalRegistryRecord> {
        let _guard = self.lock()?;
        let mut file = self.load_registry_file_unlocked()?;
        let record = file.sources.get_mut(source_id).ok_or_else(|| {
            PackageError::NotInstalled(format!("registry source {source_id} is not registered"))
        })?;
        if let Some(verified) = verified {
            record.verified = Some(verified);
        }
        record.last_verification_error = error;
        let updated = record.clone();
        self.write_registry_file_unlocked(&file)?;
        Ok(updated)
    }

    fn load_registry_file_unlocked(&self) -> PackageResult<AdditionalRegistryFile> {
        let path = self.root.join(REGISTRIES_FILE);
        if !path.exists() {
            return Ok(AdditionalRegistryFile::default());
        }
        if fs::symlink_metadata(&path)?.file_type().is_symlink() {
            return Err(PackageError::Io(
                "registries file cannot be a symlink".to_string(),
            ));
        }
        let file: AdditionalRegistryFile = serde_json::from_slice(&fs::read(&path)?)?;
        if file.schema_version != ADDITIONAL_REGISTRIES_SCHEMA_VERSION {
            return Err(PackageError::Conflict(
                "unsupported registries file schema version".to_string(),
            ));
        }
        Ok(file)
    }

    fn write_registry_file_unlocked(&self, file: &AdditionalRegistryFile) -> PackageResult<()> {
        let path = self.root.join(REGISTRIES_FILE);
        let temporary = self
            .root
            .join(format!("{REGISTRIES_FILE}.{}.tmp", Uuid::new_v4().simple()));
        fs::write(&temporary, serde_json::to_vec(file)?)?;
        fs::rename(&temporary, &path)?;
        sync_directory(&self.root)
    }

    fn mutate_state<F>(&self, package_id: &str, mutation: F) -> PackageResult<InstalledPackageState>
    where
        F: FnOnce(&mut InstalledPackageState) -> PackageResult<()>,
    {
        let _guard = self.lock()?;
        let mut state = self
            .load_state_unlocked(package_id)?
            .filter(|state| !state.tombstoned)
            .ok_or_else(|| PackageError::NotInstalled(package_id.to_string()))?;
        mutation(&mut state)?;
        state.sequence = state.sequence.saturating_add(1);
        state.validate()?;
        self.write_state_unlocked(&state)?;
        Ok(state)
    }

    fn cache_unlocked(&self, package: &VerifiedPackage) -> PackageResult<()> {
        let path = self
            .root
            .join(CACHE_DIR)
            .join(format!("{}.json", package.bundle_sha256));
        let export = PortablePackageExport {
            schema_version: PORTABLE_EXPORT_VERSION,
            bundle_sha256: package.bundle_sha256.clone(),
            manifest: package.bundle.manifest.clone(),
            files_hex: package
                .bundle
                .files
                .iter()
                .map(|(name, bytes)| (name.clone(), encode_hex(bytes)))
                .collect(),
        };
        let bytes = serde_json::to_vec(&export)?;
        if path.exists() {
            if sha256(&fs::read(&path)?) != sha256(&bytes) {
                return Err(PackageError::Conflict(
                    "immutable package cache entry differs".to_string(),
                ));
            }
            return Ok(());
        }
        write_new_synced(&path, &bytes)
    }

    fn load_cached_bundle_unlocked(&self, digest: &str) -> PackageResult<PackageBundle> {
        if !is_sha256(digest) {
            return Err(PackageError::Conflict("invalid cache digest".to_string()));
        }
        let path = self.root.join(CACHE_DIR).join(format!("{digest}.json"));
        let export: PortablePackageExport = serde_json::from_slice(&fs::read(path)?)?;
        export.into_bundle(&PackageLimits::default())
    }

    fn load_state_unlocked(
        &self,
        package_id: &str,
    ) -> PackageResult<Option<InstalledPackageState>> {
        validate_package_id(package_id)?;
        let directory = self
            .root
            .join(STATE_DIR)
            .join(sha256(package_id.as_bytes()));
        if !directory.exists() {
            return Ok(None);
        }
        if fs::symlink_metadata(&directory)?.file_type().is_symlink() {
            return Err(PackageError::Io(
                "package state directory cannot be a symlink".to_string(),
            ));
        }
        let mut states = Vec::new();
        for entry in fs::read_dir(directory)? {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().to_string();
            if !name.starts_with(STATE_PREFIX)
                || !name.ends_with(STATE_SUFFIX)
                || !entry.file_type()?.is_file()
                || entry.file_type()?.is_symlink()
            {
                continue;
            }
            let Ok(bytes) = fs::read(entry.path()) else {
                continue;
            };
            let Ok(state) = serde_json::from_slice::<InstalledPackageState>(&bytes) else {
                continue;
            };
            if state.package_id == package_id && state.validate().is_ok() {
                states.push(state);
            }
        }
        states.sort_by(|left, right| right.sequence.cmp(&left.sequence));
        Ok(states.into_iter().next())
    }

    fn next_sequence_unlocked(&self, package_id: &str) -> PackageResult<u64> {
        Ok(self
            .load_state_unlocked(package_id)?
            .map_or(1, |state| state.sequence.saturating_add(1)))
    }

    fn write_state_unlocked(&self, state: &InstalledPackageState) -> PackageResult<()> {
        state.validate()?;
        let directory = self
            .root
            .join(STATE_DIR)
            .join(sha256(state.package_id.as_bytes()));
        fs::create_dir_all(&directory)?;
        let path = directory.join(format!(
            "{STATE_PREFIX}{:020}-{}{STATE_SUFFIX}",
            state.sequence,
            Uuid::new_v4().simple()
        ));
        write_new_synced(&path, &serde_json::to_vec(state)?)?;
        sync_directory(&directory)
    }

    fn lock(&self) -> PackageResult<std::sync::MutexGuard<'_, ()>> {
        self.gate
            .lock()
            .map_err(|_| PackageError::Io("package-store lock poisoned".to_string()))
    }
}

fn write_new_synced(path: &Path, bytes: &[u8]) -> PackageResult<()> {
    let mut file = OpenOptions::new().create_new(true).write(true).open(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}

fn sync_directory(path: &Path) -> PackageResult<()> {
    #[cfg(unix)]
    File::open(path)?.sync_all()?;
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

fn decode_hex(value: &str) -> PackageResult<Vec<u8>> {
    if value.len() % 2 != 0 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(PackageError::InvalidManifest(
            "expected even-length hexadecimal data".to_string(),
        ));
    }
    value
        .as_bytes()
        .chunks_exact(2)
        .map(|pair| {
            let text = std::str::from_utf8(pair)
                .map_err(|error| PackageError::InvalidManifest(error.to_string()))?;
            u8::from_str_radix(text, 16)
                .map_err(|error| PackageError::InvalidManifest(error.to_string()))
        })
        .collect()
}

fn encode_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(HEX[(byte >> 4) as usize] as char);
        output.push(HEX[(byte & 0x0f) as usize] as char);
    }
    output
}

#[derive(Debug, Clone)]
pub struct FirstPartyPackageFixture {
    pub fixture_id: String,
    pub bundle: PackageBundle,
}

/// Ten deterministic, data-only fixtures covering skills, assistants, and
/// the initial connector catalog. They are unsigned local-folder fixtures;
/// release tooling signs the same canonical manifest payloads.
pub fn first_party_package_fixtures() -> Vec<FirstPartyPackageFixture> {
    vec![
        skill_fixture(
            "review",
            "Review",
            "Review code for correctness and security.",
        ),
        skill_fixture("testing", "Testing", "Plan and run bounded software tests."),
        skill_fixture(
            "documentation",
            "Documentation",
            "Produce source-grounded documentation.",
        ),
        skill_fixture(
            "browser-qa",
            "Browser QA",
            "Inspect declared browser QA evidence.",
        ),
        skill_fixture(
            "release-preparation",
            "Release Preparation",
            "Prepare auditable releases.",
        ),
        skill_fixture(
            "knowledge-workflows",
            "Knowledge Workflows",
            "Curate and evaluate knowledge retrieval.",
        ),
        connector_fixture("github", ConnectorKind::Github, "https://api.github.com"),
        connector_fixture("gitlab", ConnectorKind::Gitlab, "https://gitlab.com"),
        connector_fixture("webdav", ConnectorKind::Webdav, "https://dav.example.com"),
        connector_fixture(
            "rest-webhook",
            ConnectorKind::Rest,
            "https://api.example.com",
        ),
    ]
}

pub const FIRST_PARTY_REGISTRY_ID: &str = "little-monkey-first-party";
pub const FIRST_PARTY_TRUST_ROOT_ID: &str = "little-monkey-first-party";
pub const FIRST_PARTY_RELEASE_KEY_ID: &str = "release-2026-1";
pub const FIRST_PARTY_REGISTRY_GENERATED_UNIX_MS: u64 = 1_783_900_800_000;
pub const FIRST_PARTY_REGISTRY_REFRESH_AFTER_UNIX_MS: u64 = 1_786_492_800_000;
pub const FIRST_PARTY_REGISTRY_EXPIRES_UNIX_MS: u64 = 2_524_608_000_000;

/// Returns the immutable first-party release catalog shipped with the app.
/// The private signing key is deliberately not present in the repository;
/// every manifest and the registry envelope are verified at runtime against
/// the embedded Ed25519 public trust root before they enter the catalog.
pub fn signed_first_party_catalog(
) -> PackageResult<(TrustStore, RegistrySnapshot, Vec<PackageBundle>)> {
    const PUBLIC_KEY_HEX: &str = "0fd1a0b2a2e6a90c5f61eb8e9db503bf4e123c4cee11888650748c2f0efc669e";
    const REGISTRY_SIGNATURE_HEX: &str =
        "d0bac591ae808fc7d0f7022425c210f83c500bf4605db53149ca06ae61f962c9c6ec3b264e949d1892d92dfd2485802b38384511ddff09e2340e2abd47b04900";
    const PACKAGE_SIGNATURES: &[(&str, &str)] = &[
        (
            "com.littlemonkey.skill.review",
            "bce3446d576c01e556c2bdfd78d1b7393787c678591e111066f3be3821063f15531f8c9795a96544af4ed4c83906e9426a02945d699baafea64af57cac7c3708",
        ),
        (
            "com.littlemonkey.skill.testing",
            "370fe8178d30a3b197671f642e79f1b9420a83ceae417b0d5866f3828468c6e032f145a9c02f69db1bc311fa3a90ee393ca82cbc0832e926ca256e091dede20e",
        ),
        (
            "com.littlemonkey.skill.documentation",
            "7fec34360e0127e01ba39d54336c54805fbf5d0189c69b68d1201830d2553b945bdbd12ebf0eea0517de87c0b79401e0753759e7b7bd0c76190ad301e4546d00",
        ),
        (
            "com.littlemonkey.skill.browser-qa",
            "7ce68d16ee5401b8aa5f50a69e2954f55f2123713347da8e1fef23d7612c44beee9a9e968c62b367df0914a4b3ef360e24cee696c1b2a3831b5d703e249c4409",
        ),
        (
            "com.littlemonkey.skill.release-preparation",
            "d0485a4b670c717c86b3254f6786c30590d5c55ad8182acea34349bcaa7553f5c6f8a65122dad3c20710b935ac079972cc9252b18738c989a9c5d54f6846cf06",
        ),
        (
            "com.littlemonkey.skill.knowledge-workflows",
            "ab05330188f638cf87e81b6529c4a3cec7b9b63fa8cda71679652b5c43ca3b130ae3f1cd1a5befba9b4a92c69c26c1f208fb4b21c7a945d71c7db1de17dafd0d",
        ),
        (
            "com.littlemonkey.connector.github",
            "4d60ddf3648fa5148af7693d9c0b9c30205cfeaeb6c5d3d6a1e44a7575e8f1617bed9f6a3d046916f5d1789c84a0b87825054cd42db6f51a0ba61e46a1115309",
        ),
        (
            "com.littlemonkey.connector.gitlab",
            "23f54383112a6a9a1ffc26082a268e02df583cefae9211fa74243204b731cbe600c0711a9d7807a1d2f514e55e319292ecd728298383db050712880b2b177f0a",
        ),
        (
            "com.littlemonkey.connector.webdav",
            "8f54a2463a57007472a18d790d6af25d679b99070e21aee93b67a934a2de0215a3eb6c2ecd9f97d489a1ce513d7afbaac5209dafa2a629d710b8c232521ba902",
        ),
        (
            "com.littlemonkey.connector.rest-webhook",
            "2749aa9ff1e2e0f45446a71091471713f8f6b020405c4ab51092e19327b4d3bf039b07f80b7dea904bcf4aa29b81623ffa9a0ee5cf780bfc5f9e92dbdc086009",
        ),
    ];

    let signatures = PACKAGE_SIGNATURES
        .iter()
        .copied()
        .collect::<BTreeMap<_, _>>();
    let mut packages = BTreeMap::new();
    let mut bundles = Vec::new();
    for fixture in first_party_package_fixtures() {
        let mut bundle = fixture.bundle;
        bundle.manifest.provenance.source = InstallSource::CuratedRegistry {
            registry_id: FIRST_PARTY_REGISTRY_ID.to_string(),
        };
        bundle.manifest.provenance.source_revision = "builtin-registry-2026-07-13".to_string();
        let signature_hex = signatures
            .get(bundle.manifest.package_id.as_str())
            .ok_or_else(|| {
                PackageError::InvalidBundle(format!(
                    "first-party package has no release signature: {}",
                    bundle.manifest.package_id
                ))
            })?;
        bundle.manifest.signature = Some(PackageSignature {
            trust_root_id: FIRST_PARTY_TRUST_ROOT_ID.to_string(),
            key_id: FIRST_PARTY_RELEASE_KEY_ID.to_string(),
            algorithm: "ed25519".to_string(),
            signature_hex: (*signature_hex).to_string(),
        });
        let bundle_sha256 = bundle.validate(&PackageLimits::default())?;
        let manifest_sha256 = sha256(&serde_json::to_vec(&bundle.manifest)?);
        packages.insert(
            bundle.manifest.package_id.clone(),
            vec![RegistryPackageVersion {
                version: bundle.manifest.version,
                bundle_sha256,
                manifest_sha256,
            }],
        );
        bundles.push(bundle);
    }
    if bundles.len() != PACKAGE_SIGNATURES.len() {
        return Err(PackageError::InvalidBundle(
            "first-party release catalog/signature count mismatch".to_string(),
        ));
    }

    let trust_store = TrustStore {
        schema_version: TRUST_STORE_VERSION,
        roots: BTreeMap::from([(
            FIRST_PARTY_TRUST_ROOT_ID.to_string(),
            TrustRoot {
                trust_root_id: FIRST_PARTY_TRUST_ROOT_ID.to_string(),
                publisher: "Little Monkey".to_string(),
                package_namespaces: BTreeSet::from(["com.littlemonkey.".to_string()]),
                keys: BTreeMap::from([(
                    FIRST_PARTY_RELEASE_KEY_ID.to_string(),
                    TrustedKey {
                        key_id: FIRST_PARTY_RELEASE_KEY_ID.to_string(),
                        algorithm: "ed25519".to_string(),
                        public_key_hex: PUBLIC_KEY_HEX.to_string(),
                        valid_from_unix_ms: 1_767_225_600_000,
                        valid_until_unix_ms: 4_102_444_800_000,
                        revoked_at_unix_ms: None,
                    },
                )]),
            },
        )]),
    };
    let snapshot = RegistrySnapshot {
        schema_version: REGISTRY_SNAPSHOT_VERSION,
        registry_id: FIRST_PARTY_REGISTRY_ID.to_string(),
        sequence: 1,
        generated_unix_ms: FIRST_PARTY_REGISTRY_GENERATED_UNIX_MS,
        refresh_after_unix_ms: FIRST_PARTY_REGISTRY_REFRESH_AFTER_UNIX_MS,
        expires_unix_ms: FIRST_PARTY_REGISTRY_EXPIRES_UNIX_MS,
        packages,
        revocations: Vec::new(),
        signature: RegistrySignature {
            trust_root_id: FIRST_PARTY_TRUST_ROOT_ID.to_string(),
            key_id: FIRST_PARTY_RELEASE_KEY_ID.to_string(),
            algorithm: "ed25519".to_string(),
            signature_hex: REGISTRY_SIGNATURE_HEX.to_string(),
        },
    };
    trust_store.validate()?;
    Ok((trust_store, snapshot, bundles))
}

fn fixture_compatibility() -> Compatibility {
    Compatibility {
        minimum_app_version: SemanticVersion::new(0, 1, 0),
        maximum_app_version_exclusive: None,
        platforms: ["macos", "linux", "windows"]
            .into_iter()
            .map(str::to_string)
            .collect(),
        architectures: ["aarch64", "x86_64"]
            .into_iter()
            .map(str::to_string)
            .collect(),
    }
}

/// Builds an absolute-path *string* valid on whichever OS this actually runs
/// under. This value is a pure identity/provenance string — checked only by
/// [`validate_provenance`]'s `Path::is_absolute()` call, never touched by
/// real disk I/O — but `/foo` satisfies `is_absolute()` on Unix and not on
/// Windows (which requires a drive-letter or UNC prefix), so a hardcoded
/// `/`-rooted fixture fails Windows-only validation that has nothing to do
/// with what these fixtures exist to exercise.
fn fixture_absolute_path(rest: &str) -> String {
    if cfg!(windows) {
        format!(r"C:\{}", rest.replace('/', "\\"))
    } else {
        format!("/{rest}")
    }
}

fn fixture_provenance(slug: &str) -> PackageProvenance {
    PackageProvenance {
        publisher: "Little Monkey".to_string(),
        source: InstallSource::LocalFolder {
            canonical_path: fixture_absolute_path(&format!("first-party-fixtures/{slug}")),
        },
        source_revision: "checked-in-fixture-v1".to_string(),
        build_reproducible: true,
    }
}

fn skill_fixture(slug: &str, display_name: &str, instructions: &str) -> FirstPartyPackageFixture {
    let path = "instructions.md".to_string();
    let bytes = instructions.as_bytes().to_vec();
    let digest = sha256(&bytes);
    let manifest = PackageManifest {
        schema_version: PACKAGE_MANIFEST_VERSION,
        package_id: format!("com.littlemonkey.skill.{slug}"),
        version: SemanticVersion::new(1, 0, 0),
        kind: PackageKind::Skill,
        display_name: display_name.to_string(),
        description: instructions.to_string(),
        content: vec![ContentReference {
            kind: ContentKind::Instructions,
            path: path.clone(),
            media_type: "text/markdown".to_string(),
            sha256: digest.clone(),
        }],
        assistant: None,
        connector: None,
        mcp_requirements: Vec::new(),
        ui_resources: Vec::new(),
        model_requirements: vec![ModelRequirement {
            capability: "text".to_string(),
            minimum_context_tokens: Some(4_096),
            local_compatible: true,
        }],
        permissions: BTreeSet::new(),
        vulnerability_notices: Vec::new(),
        compatibility: fixture_compatibility(),
        file_checksums: BTreeMap::from([(path.clone(), digest)]),
        provenance: fixture_provenance(slug),
        signature: None,
    };
    FirstPartyPackageFixture {
        fixture_id: slug.to_string(),
        bundle: PackageBundle {
            manifest,
            files: BTreeMap::from([(path, bytes)]),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signed_first_party_catalog_has_a_real_verifiable_chain() {
        let (trust, snapshot, bundles) = signed_first_party_catalog().expect("catalog");
        let verifier = RingEd25519SignatureVerifier;
        let verified_registry = verify_registry_snapshot(
            &snapshot,
            &trust,
            None,
            &verifier,
            FIRST_PARTY_REGISTRY_GENERATED_UNIX_MS,
        )
        .expect("registry signature");
        assert_eq!(bundles.len(), 10);
        let environment = InstallEnvironment {
            app_version: SemanticVersion::new(0, 1, 0),
            platform: "macos".to_string(),
            architecture: "aarch64".to_string(),
        };
        for bundle in bundles {
            let verified = verify_package(
                &bundle,
                &trust,
                Some(&verified_registry),
                &environment,
                &InstallTrustPolicy {
                    allow_unsigned_local_folders: false,
                    allow_unsigned_git: false,
                    require_registry_catalog_match: true,
                    permit_expired_offline_registry: true,
                },
                &PackageLimits::default(),
                &verifier,
                FIRST_PARTY_REGISTRY_GENERATED_UNIX_MS,
            )
            .expect("signed first-party package");
            assert!(verified.trust().signed);
        }
    }

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new(label: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "little-monkey-package-{label}-{}",
                Uuid::new_v4().simple()
            ));
            fs::create_dir_all(&path).expect("create test directory");
            Self(path)
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    struct DigestVerifier;

    impl SignatureVerifier for DigestVerifier {
        fn verify(
            &self,
            algorithm: &str,
            public_key: &[u8],
            message: &[u8],
            signature: &[u8],
        ) -> Result<bool, String> {
            let mut signed = public_key.to_vec();
            signed.extend_from_slice(message);
            Ok(algorithm == "fixture-sha256"
                && decode_hex(&sha256(&signed)).expect("digest bytes") == signature)
        }
    }

    fn trust_store() -> TrustStore {
        let key = TrustedKey {
            key_id: "release-1".to_string(),
            algorithm: "fixture-sha256".to_string(),
            public_key_hex: encode_hex(b"fixture-public-key"),
            valid_from_unix_ms: 1,
            valid_until_unix_ms: 10_000_000,
            revoked_at_unix_ms: None,
        };
        TrustStore {
            schema_version: TRUST_STORE_VERSION,
            roots: BTreeMap::from([(
                "littlemonkey-root".to_string(),
                TrustRoot {
                    trust_root_id: "littlemonkey-root".to_string(),
                    publisher: "Little Monkey".to_string(),
                    package_namespaces: ["com.littlemonkey.".to_string()].into_iter().collect(),
                    keys: BTreeMap::from([("release-1".to_string(), key)]),
                },
            )]),
        }
    }

    fn environment() -> InstallEnvironment {
        InstallEnvironment {
            app_version: SemanticVersion::new(1, 0, 0),
            platform: "macos".to_string(),
            architecture: "aarch64".to_string(),
        }
    }

    fn verify_local(bundle: &PackageBundle) -> VerifiedPackage {
        verify_package(
            bundle,
            &trust_store(),
            None,
            &environment(),
            &InstallTrustPolicy::default(),
            &PackageLimits::default(),
            &DigestVerifier,
            1_000,
        )
        .expect("verify local fixture")
    }

    fn sign_manifest(manifest: &mut PackageManifest) {
        manifest.signature = Some(PackageSignature {
            trust_root_id: "littlemonkey-root".to_string(),
            key_id: "release-1".to_string(),
            algorithm: "fixture-sha256".to_string(),
            signature_hex: String::new(),
        });
        let mut signed = b"fixture-public-key".to_vec();
        signed.extend_from_slice(&manifest.signing_payload().expect("payload"));
        manifest
            .signature
            .as_mut()
            .expect("signature")
            .signature_hex = sha256(&signed);
    }

    fn sign_registry(snapshot: &mut RegistrySnapshot) {
        snapshot.signature.signature_hex.clear();
        let mut signed = b"fixture-public-key".to_vec();
        signed.extend_from_slice(&snapshot.signing_payload().expect("payload"));
        snapshot.signature.signature_hex = sha256(&signed);
    }

    #[test]
    fn ten_first_party_fixtures_complete_offline_lifecycle() {
        let fixtures = first_party_package_fixtures();
        assert_eq!(fixtures.len(), 10);
        assert_eq!(
            fixtures
                .iter()
                .map(|fixture| fixture.fixture_id.as_str())
                .collect::<BTreeSet<_>>()
                .len(),
            10
        );
        let directory = TestDirectory::new("ten-fixtures");
        let store = PackageStore::new(&directory.0).expect("store");
        for fixture in fixtures {
            let verified = verify_local(&fixture.bundle);
            let installed = store.install(&verified).expect("install");
            assert!(installed.enabled);
            assert!(
                !store
                    .set_enabled(&installed.package_id, false)
                    .expect("disable")
                    .enabled
            );
            let export = store.export_active(&installed.package_id).expect("export");
            assert_eq!(
                export
                    .clone()
                    .into_bundle(&PackageLimits::default())
                    .expect("import"),
                fixture.bundle
            );
            store
                .set_enabled(&installed.package_id, true)
                .expect("enable");
            assert!(
                store
                    .uninstall(&installed.package_id)
                    .expect("uninstall")
                    .tombstoned
            );
            assert_eq!(export.bundle_sha256, verified.bundle_sha256());
        }
    }

    #[test]
    fn tampered_executable_and_privileged_packages_are_rejected() {
        let mut tampered = first_party_package_fixtures().remove(0).bundle;
        tampered
            .files
            .insert("instructions.md".to_string(), b"tampered".to_vec());
        assert!(matches!(
            tampered.validate(&PackageLimits::default()),
            Err(PackageError::InvalidBundle(_))
        ));

        let mut executable = first_party_package_fixtures().remove(0).bundle;
        let bytes = b"#!/bin/sh\necho unsafe".to_vec();
        let digest = sha256(&bytes);
        executable.files = BTreeMap::from([("payload.sh".to_string(), bytes)]);
        executable.manifest.content[0].path = "payload.sh".to_string();
        executable.manifest.content[0].sha256 = digest.clone();
        executable.manifest.file_checksums = BTreeMap::from([("payload.sh".to_string(), digest)]);
        assert!(matches!(
            executable.validate(&PackageLimits::default()),
            Err(PackageError::InvalidBundle(_))
        ));

        let mut privileged = first_party_package_fixtures().remove(0).bundle;
        privileged.manifest.permissions.insert(PackagePermission {
            permission_id: "native".to_string(),
            kind: PermissionKind::ExecuteProcess,
            scope: "*".to_string(),
            reason: "not permitted".to_string(),
        });
        assert!(matches!(
            privileged.manifest.validate(&PackageLimits::default()),
            Err(PackageError::InvalidManifest(_))
        ));

        let mut invalid_assistant = first_party_package_fixtures().remove(0).bundle;
        invalid_assistant.manifest.kind = PackageKind::Assistant;
        invalid_assistant.manifest.content[0].kind = ContentKind::Persona;
        invalid_assistant.manifest.assistant = Some(AssistantComposition {
            persona_content_path: "instructions.md".to_string(),
            skill_package_ids: BTreeSet::new(),
            starter_workflow_paths: vec!["missing-workflow.json".to_string()],
            knowledge_template_path: None,
        });
        assert!(matches!(
            invalid_assistant
                .manifest
                .validate(&PackageLimits::default()),
            Err(PackageError::InvalidManifest(_))
        ));
    }

    #[test]
    fn signed_registry_rejects_signature_tamper_and_revocation() {
        let now = 1_000;
        let mut bundle = first_party_package_fixtures().remove(0).bundle;
        bundle.manifest.provenance.source = InstallSource::CuratedRegistry {
            registry_id: "first-party".to_string(),
        };
        bundle.manifest.provenance.source_revision = "registry-sequence-1".to_string();
        sign_manifest(&mut bundle.manifest);
        let bundle_digest = bundle
            .validate(&PackageLimits::default())
            .expect("bundle digest");
        let manifest_digest = sha256(&serde_json::to_vec(&bundle.manifest).expect("manifest"));
        let mut snapshot = RegistrySnapshot {
            schema_version: REGISTRY_SNAPSHOT_VERSION,
            registry_id: "first-party".to_string(),
            sequence: 1,
            generated_unix_ms: 900,
            refresh_after_unix_ms: 950,
            expires_unix_ms: 2_000,
            packages: BTreeMap::from([(
                bundle.manifest.package_id.clone(),
                vec![RegistryPackageVersion {
                    version: bundle.manifest.version,
                    bundle_sha256: bundle_digest,
                    manifest_sha256: manifest_digest,
                }],
            )]),
            revocations: Vec::new(),
            signature: RegistrySignature {
                trust_root_id: "littlemonkey-root".to_string(),
                key_id: "release-1".to_string(),
                algorithm: "fixture-sha256".to_string(),
                signature_hex: String::new(),
            },
        };
        sign_registry(&mut snapshot);
        let registry =
            verify_registry_snapshot(&snapshot, &trust_store(), None, &DigestVerifier, now)
                .expect("verify registry");
        verify_package(
            &bundle,
            &trust_store(),
            Some(&registry),
            &environment(),
            &InstallTrustPolicy::default(),
            &PackageLimits::default(),
            &DigestVerifier,
            now,
        )
        .expect("verify package");

        let mut tampered = bundle.clone();
        tampered
            .manifest
            .signature
            .as_mut()
            .expect("signature")
            .signature_hex = "00".repeat(32);
        assert!(matches!(
            verify_package(
                &tampered,
                &trust_store(),
                Some(&registry),
                &environment(),
                &InstallTrustPolicy::default(),
                &PackageLimits::default(),
                &DigestVerifier,
                now,
            ),
            Err(PackageError::Untrusted(_))
        ));

        let mut revoked = snapshot;
        revoked.sequence = 2;
        revoked.revocations.push(RevocationEntry {
            revocation_id: "revoke-review".to_string(),
            target: RevocationTarget::PackageVersion {
                package_id: bundle.manifest.package_id.clone(),
                version: bundle.manifest.version,
            },
            effective_unix_ms: now,
            reason: "fixture compromise".to_string(),
        });
        sign_registry(&mut revoked);
        let registry = verify_registry_snapshot(
            &revoked,
            &trust_store(),
            Some(&registry),
            &DigestVerifier,
            now,
        )
        .expect("verify revocation");
        assert!(matches!(
            verify_package(
                &bundle,
                &trust_store(),
                Some(&registry),
                &environment(),
                &InstallTrustPolicy::default(),
                &PackageLimits::default(),
                &DigestVerifier,
                now,
            ),
            Err(PackageError::Revoked(_))
        ));
    }

    #[test]
    fn offline_state_is_explicit_and_mcp_remains_separate() {
        let bundle = first_party_package_fixtures().remove(0).bundle;
        let verified = verify_local(&bundle);
        assert_eq!(
            verified.trust().revocation,
            RevocationKnowledge::UnknownNeverDownloaded
        );
        assert!(install_preview(&verified, None)
            .expect("preview")
            .warnings
            .iter()
            .any(|warning| warning.contains("never been downloaded")));

        let mut invalid = bundle;
        invalid.manifest.mcp_requirements.push(McpRequirement {
            requirement_id: "remote".to_string(),
            kind: McpRequirementKind::RemoteHttp,
            server_id: None,
            remote_origin: Some("https://mcp.example.com".to_string()),
            required_tools: BTreeSet::new(),
            separate_install_approval_required: false,
            separate_oauth_approval_required: false,
        });
        assert!(matches!(
            invalid.manifest.validate(&PackageLimits::default()),
            Err(PackageError::InvalidManifest(_))
        ));
    }

    #[test]
    fn expanding_update_needs_exact_approval_then_pin_and_rollback_work() {
        let base = first_party_package_fixtures().remove(0).bundle;
        let directory = TestDirectory::new("update");
        let store = PackageStore::new(&directory.0).expect("store");
        store.install(&verify_local(&base)).expect("install");
        let mut next_bundle = base.clone();
        next_bundle.manifest.version = SemanticVersion::new(1, 1, 0);
        next_bundle.manifest.permissions.insert(PackagePermission {
            permission_id: "network-docs".to_string(),
            kind: PermissionKind::Network,
            scope: "https://docs.example.com".to_string(),
            reason: "Read explicitly requested documentation".to_string(),
        });
        let next = verify_local(&next_bundle);
        let PackageError::PermissionApprovalRequired(digest) =
            store.update(&next, None).expect_err("approval required")
        else {
            panic!("wrong update error");
        };
        store
            .update(
                &next,
                Some(&PermissionApproval {
                    package_id: base.manifest.package_id.clone(),
                    from_version: SemanticVersion::new(1, 0, 0),
                    to_version: SemanticVersion::new(1, 1, 0),
                    approval_digest: digest,
                    approved: true,
                }),
            )
            .expect("approved update");
        assert_eq!(
            store
                .export_version(&base.manifest.package_id, SemanticVersion::new(1, 0, 0))
                .expect("verified rollback export")
                .manifest
                .version,
            SemanticVersion::new(1, 0, 0)
        );
        store
            .pin(
                &base.manifest.package_id,
                Some(SemanticVersion::new(1, 1, 0)),
            )
            .expect("pin");
        let mut third_bundle = next_bundle.clone();
        third_bundle.manifest.version = SemanticVersion::new(1, 2, 0);
        let third = verify_local(&third_bundle);
        assert!(matches!(
            store.update(&third, None),
            Err(PackageError::Pinned(_))
        ));
        store.pin(&base.manifest.package_id, None).expect("unpin");
        store.update(&third, None).expect("update");
        assert_eq!(
            store
                .rollback(&base.manifest.package_id)
                .expect("rollback")
                .active_version,
            Some(SemanticVersion::new(1, 1, 0))
        );
    }

    #[test]
    fn local_install_count_survives_uninstall_and_reinstall_and_team_approved_toggles() {
        let bundle = first_party_package_fixtures().remove(0).bundle;
        let directory = TestDirectory::new("install-count");
        let store = PackageStore::new(&directory.0).expect("store");
        let package_id = bundle.manifest.package_id.clone();

        let first_install = store.install(&verify_local(&bundle)).expect("install");
        assert_eq!(first_install.local_install_count, 1);
        assert!(!first_install.team_approved);

        let approved = store
            .set_team_approved(&package_id, true)
            .expect("mark team approved");
        assert!(approved.team_approved);

        store.uninstall(&package_id).expect("uninstall");
        let second_install = store.install(&verify_local(&bundle)).expect("reinstall");
        assert_eq!(second_install.local_install_count, 2);
        // Reinstalling produces a fresh, non-tombstoned state; the local
        // install counter is the only piece of history that is intentionally
        // preserved across an uninstall/reinstall cycle.
        assert!(second_install.team_approved);
    }

    #[test]
    fn vulnerability_notices_are_validated_and_surfaced_on_the_manifest() {
        let mut bundle = first_party_package_fixtures().remove(0).bundle;
        bundle.manifest.vulnerability_notices.push(VulnerabilityNotice {
            notice_id: "notice-1".to_string(),
            severity: VulnerabilitySeverity::High,
            summary: "Sample dependency has a known issue".to_string(),
            affected_versions: BTreeSet::from([SemanticVersion::new(1, 0, 0)]),
            advisory_url: Some("https://example.com/advisories/1".to_string()),
        });
        bundle.manifest.validate(&PackageLimits::default()).expect("valid notice");
        let verified = verify_local(&bundle);
        assert_eq!(verified.manifest().vulnerability_notices.len(), 1);

        let mut empty_summary = bundle.clone();
        empty_summary.manifest.vulnerability_notices[0].summary = "   ".to_string();
        assert!(matches!(
            empty_summary.manifest.validate(&PackageLimits::default()),
            Err(PackageError::InvalidManifest(_))
        ));

        let mut insecure_advisory = bundle.clone();
        insecure_advisory.manifest.vulnerability_notices[0].advisory_url =
            Some("http://example.com/advisories/1".to_string());
        assert!(matches!(
            insecure_advisory.manifest.validate(&PackageLimits::default()),
            Err(PackageError::InvalidManifest(_))
        ));
    }

    #[test]
    fn additional_registry_sources_require_the_existing_verification_chain() {
        let directory = TestDirectory::new("registry-sources");
        let store = PackageStore::new(&directory.0).expect("store");
        assert!(store.list_registry_sources().expect("empty list").is_empty());

        let source = AdditionalRegistrySource {
            source_id: "team-catalog".to_string(),
            display_name: "Team Catalog".to_string(),
            location: "https://team.example.com/registry.json".to_string(),
            added_unix_ms: 1_000,
        };
        let record = store
            .add_registry_source(source.clone())
            .expect("add source");
        assert!(record.verified.is_none());

        assert!(matches!(
            store.add_registry_source(source.clone()),
            Err(PackageError::Conflict(_))
        ));

        let bundle = first_party_package_fixtures().remove(0).bundle;
        let bundle_digest = bundle
            .validate(&PackageLimits::default())
            .expect("bundle digest");
        let manifest_digest = sha256(&serde_json::to_vec(&bundle.manifest).expect("manifest"));
        let mut snapshot = RegistrySnapshot {
            schema_version: REGISTRY_SNAPSHOT_VERSION,
            registry_id: "team-catalog".to_string(),
            sequence: 1,
            generated_unix_ms: 900,
            refresh_after_unix_ms: 950,
            expires_unix_ms: 2_000,
            packages: BTreeMap::from([(
                bundle.manifest.package_id.clone(),
                vec![RegistryPackageVersion {
                    version: bundle.manifest.version,
                    bundle_sha256: bundle_digest,
                    manifest_sha256: manifest_digest,
                }],
            )]),
            revocations: Vec::new(),
            signature: RegistrySignature {
                trust_root_id: "littlemonkey-root".to_string(),
                key_id: "release-1".to_string(),
                algorithm: "fixture-sha256".to_string(),
                signature_hex: String::new(),
            },
        };
        sign_registry(&mut snapshot);

        // Tampering with the signature must fail verification, and a failed
        // verification must never mark the source as trusted.
        let mut tampered = snapshot.clone();
        tampered.signature.signature_hex = "00".repeat(32);
        let tampered_result =
            verify_registry_snapshot(&tampered, &trust_store(), None, &DigestVerifier, 1_000);
        assert!(tampered_result.is_err());
        store
            .record_registry_verification(
                &source.source_id,
                None,
                Some(tampered_result.unwrap_err().to_string()),
            )
            .expect("record failed verification");
        let after_failed = store
            .list_registry_sources()
            .expect("list after failed verification");
        assert_eq!(after_failed.len(), 1);
        assert!(after_failed[0].verified.is_none());
        assert!(after_failed[0].last_verification_error.is_some());

        let verified = verify_registry_snapshot(&snapshot, &trust_store(), None, &DigestVerifier, 1_000)
            .expect("verify team-catalog snapshot through the existing Ed25519 chain");
        let updated = store
            .record_registry_verification(&source.source_id, Some(verified), None)
            .expect("record success");
        assert!(updated.verified.is_some());
        assert!(updated.last_verification_error.is_none());
        assert_eq!(
            updated.verified.as_ref().unwrap().snapshot().registry_id,
            "team-catalog"
        );

        assert!(store
            .remove_registry_source(&source.source_id)
            .expect("remove"));
        assert!(store.list_registry_sources().expect("empty again").is_empty());
        assert!(!store
            .remove_registry_source(&source.source_id)
            .expect("remove missing is a no-op"));
    }
}

fn connector_fixture(slug: &str, kind: ConnectorKind, origin: &str) -> FirstPartyPackageFixture {
    let path = "connector.json".to_string();
    let bytes = format!("{{\"connector\":\"{slug}\"}}").into_bytes();
    let digest = sha256(&bytes);
    let permission = PackagePermission {
        permission_id: "network-read".to_string(),
        kind: PermissionKind::Network,
        scope: origin.to_string(),
        reason: "Read user-authorized service data".to_string(),
    };
    let connector = ConnectorDeclaration {
        contract_version: CONNECTOR_CONTRACT_VERSION,
        kind,
        auth: ConnectorAuthKind::OAuth,
        allowed_origins: [origin.to_string()].into_iter().collect(),
        operations: vec![ConnectorOperation {
            operation_id: "list".to_string(),
            method: "GET".to_string(),
            path_template: "/v1/items".to_string(),
            effect: ConnectorEffect::Read,
            required_permission_ids: [permission.permission_id.clone()].into_iter().collect(),
            idempotency_supported: true,
        }],
    };
    let manifest = PackageManifest {
        schema_version: PACKAGE_MANIFEST_VERSION,
        package_id: format!("com.littlemonkey.connector.{slug}"),
        version: SemanticVersion::new(1, 0, 0),
        kind: PackageKind::Connector,
        display_name: format!("{slug} connector"),
        description: format!("Declarative {slug} connector fixture"),
        content: vec![ContentReference {
            kind: ContentKind::Instructions,
            path: path.clone(),
            media_type: "application/json".to_string(),
            sha256: digest.clone(),
        }],
        assistant: None,
        connector: Some(connector),
        mcp_requirements: Vec::new(),
        ui_resources: Vec::new(),
        model_requirements: Vec::new(),
        permissions: [permission].into_iter().collect(),
        vulnerability_notices: Vec::new(),
        compatibility: fixture_compatibility(),
        file_checksums: BTreeMap::from([(path.clone(), digest)]),
        provenance: fixture_provenance(slug),
        signature: None,
    };
    FirstPartyPackageFixture {
        fixture_id: slug.to_string(),
        bundle: PackageBundle {
            manifest,
            files: BTreeMap::from([(path, bytes)]),
        },
    }
}
