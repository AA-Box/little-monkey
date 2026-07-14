//! Deterministic exports, hostile-input preflight, encrypted snapshots, and
//! transport-neutral WebDAV synchronization.
//!
//! This module intentionally contains no Tauri or network client. Portable
//! bundles and DOCX files use a small deterministic ZIP implementation that
//! writes stored entries and accepts only stored entries on import. Snapshot
//! encryption is delegated to an injected production [`CryptoProvider`]; the
//! crate currently has no audited AEAD dependency, so this module does not
//! invent one.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::error::Error;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Component, Path, PathBuf};

#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

use base64::Engine as _;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::artifact_store::{ArtifactBlob, ArtifactStore, ArtifactStoreError};

pub const PORTABLE_BUNDLE_FORMAT: &str = "little-monkey-portable-bundle";
pub const PORTABLE_BUNDLE_SCHEMA_VERSION: u32 = 1;
pub const PORTABLE_DATA_SCHEMA_VERSION: u32 = 1;
pub const ENCRYPTED_SNAPSHOT_FORMAT: &str = "little-monkey-encrypted-snapshot";
pub const ENCRYPTED_SNAPSHOT_SCHEMA_VERSION: u32 = 1;

const MANIFEST_ENTRY: &str = "manifest.json";
const DATA_ENTRY: &str = "data.json";
const ARTIFACT_PREFIX: &str = "artifacts/";
const ZIP_LOCAL_SIGNATURE: u32 = 0x0403_4b50;
const ZIP_CENTRAL_SIGNATURE: u32 = 0x0201_4b50;
const ZIP_EOCD_SIGNATURE: u32 = 0x0605_4b50;
const ZIP_UTF8_FLAG: u16 = 0x0800;
const ZIP_STORED_METHOD: u16 = 0;
const ZIP_MINIMUM_DATE: u16 = 0x0021;
const MAX_ID_BYTES: usize = 256;
const MAX_LOCALE_BYTES: usize = 64;
const MAX_TITLE_BYTES: usize = 16 * 1024;
const MAX_TEXT_BYTES: usize = 8 * 1024 * 1024;
const MAX_METADATA_BYTES: usize = 2 * 1024 * 1024;
const MAX_MEDIA_TYPE_BYTES: usize = 255;
const MAX_REMOTE_PATH_BYTES: usize = 4_096;

pub type PortabilityResult<T> = Result<T, PortabilityError>;

#[derive(Debug)]
pub enum PortabilityError {
    Io {
        operation: &'static str,
        path: PathBuf,
        source: io::Error,
    },
    Json(serde_json::Error),
    Artifact(ArtifactStoreError),
    Invalid {
        path: String,
        message: String,
    },
    Limit {
        name: &'static str,
        observed: u64,
        max: u64,
    },
    CorruptArchive(String),
    UnsupportedCompression(u16),
    Crypto(String),
    Commit(String),
    Conflict(String),
    Transport(String),
}

impl fmt::Display for PortabilityError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io {
                operation,
                path,
                source,
            } => write!(f, "failed to {operation} {}: {source}", path.display()),
            Self::Json(error) => write!(f, "portable JSON error: {error}"),
            Self::Artifact(error) => write!(f, "artifact store error: {error}"),
            Self::Invalid { path, message } => {
                write!(f, "invalid portable value at {path}: {message}")
            }
            Self::Limit {
                name,
                observed,
                max,
            } => write!(f, "{name} is {observed}, exceeding the limit {max}"),
            Self::CorruptArchive(message) => write!(f, "corrupt portable archive: {message}"),
            Self::UnsupportedCompression(method) => {
                write!(f, "unsupported ZIP compression method {method}")
            }
            Self::Crypto(message) => write!(f, "snapshot cryptography failed: {message}"),
            Self::Commit(message) => write!(f, "portable import commit failed: {message}"),
            Self::Conflict(message) => write!(f, "portable restore conflict: {message}"),
            Self::Transport(message) => write!(f, "WebDAV transport failed: {message}"),
        }
    }
}

impl Error for PortabilityError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::Json(error) => Some(error),
            Self::Artifact(error) => Some(error),
            _ => None,
        }
    }
}

impl From<serde_json::Error> for PortabilityError {
    fn from(value: serde_json::Error) -> Self {
        Self::Json(value)
    }
}

impl From<ArtifactStoreError> for PortabilityError {
    fn from(value: ArtifactStoreError) -> Self {
        Self::Artifact(value)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum PortableContentBlock {
    Text {
        text: String,
    },
    Code {
        language: Option<String>,
        code: String,
    },
    Table {
        headers: Vec<String>,
        rows: Vec<Vec<String>>,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MessageTranslationRecord {
    pub locale: String,
    pub original_blocks: Vec<PortableContentBlock>,
    pub translated_blocks: Vec<PortableContentBlock>,
    pub source_sha256: String,
    pub created_at_ms: u64,
    pub metadata: Value,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ThreadTranslationRecord {
    pub locale: String,
    pub original_title: String,
    pub translated_title: String,
    pub source_sha256: String,
    pub translated_message_ids: Vec<String>,
    pub created_at_ms: u64,
    pub metadata: Value,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PortableMessage {
    pub id: String,
    pub role: String,
    pub ordinal: u64,
    pub created_at_ms: u64,
    pub blocks: Vec<PortableContentBlock>,
    pub attachment_ids: Vec<String>,
    pub external_references: Vec<String>,
    pub translations: Vec<MessageTranslationRecord>,
    pub metadata: Value,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PortableSession {
    pub id: String,
    pub title: String,
    pub ordinal: u64,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
    pub archived: bool,
    pub pinned: bool,
    pub model_key: Option<String>,
    pub persona_id: Option<String>,
    pub workspace_path: Option<String>,
    pub messages: Vec<PortableMessage>,
    pub translations: Vec<ThreadTranslationRecord>,
    pub metadata: Value,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PortableDataV1 {
    pub schema_version: u32,
    pub sessions: Vec<PortableSession>,
    pub metadata: Value,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PortableArtifactDescriptor {
    pub id: String,
    pub media_type: String,
    pub byte_size: u64,
    pub content_sha256: String,
    pub entry: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PortableManifestV1 {
    pub format: String,
    pub schema_version: u32,
    pub bundle_id: String,
    pub exported_at_ms: u64,
    pub app_version: String,
    pub data_entry: String,
    pub data_sha256: String,
    pub session_count: u64,
    pub message_count: u64,
    pub artifacts: Vec<PortableArtifactDescriptor>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PortableArtifactInput {
    pub id: String,
    pub media_type: String,
    pub bytes: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct PortableBundleInput {
    pub bundle_id: String,
    pub exported_at_ms: u64,
    pub app_version: String,
    pub data: PortableDataV1,
    pub artifacts: Vec<PortableArtifactInput>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ValidatedPortableBundle {
    pub manifest: PortableManifestV1,
    pub data: PortableDataV1,
    pub artifacts: BTreeMap<String, Vec<u8>>,
    pub archive_sha256: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase", deny_unknown_fields)]
pub struct ImportLimits {
    pub max_archive_bytes: u64,
    pub max_entries: usize,
    pub max_entry_compressed_bytes: u64,
    pub max_entry_expanded_bytes: u64,
    pub max_total_expanded_bytes: u64,
    pub max_decompression_ratio: u64,
    pub max_sessions: usize,
    pub max_messages: usize,
    pub max_artifacts: usize,
    pub max_external_references: usize,
}

impl Default for ImportLimits {
    fn default() -> Self {
        Self {
            max_archive_bytes: 512 * 1024 * 1024,
            max_entries: 10_000,
            max_entry_compressed_bytes: 256 * 1024 * 1024,
            max_entry_expanded_bytes: 256 * 1024 * 1024,
            max_total_expanded_bytes: 1024 * 1024 * 1024,
            max_decompression_ratio: 100,
            max_sessions: 100_000,
            max_messages: 2_000_000,
            max_artifacts: 100_000,
            max_external_references: 100_000,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportPreflightReport {
    pub archive_sha256: String,
    pub entry_count: usize,
    pub compressed_bytes: u64,
    pub expanded_bytes: u64,
    pub session_count: usize,
    pub message_count: usize,
    pub artifact_count: usize,
    pub external_reference_count: usize,
}

/// Serialize a versioned portable bundle into byte-deterministic ZIP. Entry
/// names, JSON object keys, timestamps, flags, and ZIP metadata are canonical.
pub fn export_portable_bundle(input: &PortableBundleInput) -> PortabilityResult<Vec<u8>> {
    validate_bundle_identity(&input.bundle_id, input.exported_at_ms, &input.app_version)?;
    let counts = validate_portable_data(&input.data, &ImportLimits::default())?;
    if input.artifacts.len() > ImportLimits::default().max_artifacts {
        return Err(limit(
            "artifact count",
            input.artifacts.len(),
            ImportLimits::default().max_artifacts,
        ));
    }

    let data_json = canonical_json(&input.data)?;
    let data_sha256 = sha256_hex(&data_json);
    let mut entries = BTreeMap::new();
    entries.insert(DATA_ENTRY.to_string(), data_json);
    let mut descriptors = Vec::with_capacity(input.artifacts.len());
    let mut artifact_ids = HashSet::new();
    for (index, artifact) in input.artifacts.iter().enumerate() {
        let path = format!("artifacts[{index}]");
        validate_sha256(&artifact.id, &format!("{path}.id"))?;
        if !artifact_ids.insert(artifact.id.clone()) {
            return Err(invalid(
                format!("{path}.id"),
                "duplicates an earlier artifact",
            ));
        }
        validate_media_type(&artifact.media_type, &format!("{path}.mediaType"))?;
        let digest = sha256_hex(&artifact.bytes);
        if digest != artifact.id {
            return Err(invalid(
                format!("{path}.id"),
                "does not equal the SHA-256 of its exact bytes",
            ));
        }
        let entry = format!("{ARTIFACT_PREFIX}{}", artifact.id);
        descriptors.push(PortableArtifactDescriptor {
            id: artifact.id.clone(),
            media_type: artifact.media_type.clone(),
            byte_size: usize_to_u64(artifact.bytes.len(), "artifact byte size")?,
            content_sha256: digest,
            entry: entry.clone(),
        });
        entries.insert(entry, artifact.bytes.clone());
    }
    descriptors.sort_by(|left, right| left.id.cmp(&right.id));

    let referenced = input
        .data
        .sessions
        .iter()
        .flat_map(|session| &session.messages)
        .flat_map(|message| &message.attachment_ids)
        .collect::<BTreeSet<_>>();
    let declared = artifact_ids.iter().collect::<BTreeSet<_>>();
    if referenced != declared {
        return Err(invalid(
            "data.sessions[*].messages[*].attachmentIds",
            "must reference every declared artifact exactly within the declared artifact set",
        ));
    }

    let manifest = PortableManifestV1 {
        format: PORTABLE_BUNDLE_FORMAT.to_string(),
        schema_version: PORTABLE_BUNDLE_SCHEMA_VERSION,
        bundle_id: input.bundle_id.clone(),
        exported_at_ms: input.exported_at_ms,
        app_version: input.app_version.clone(),
        data_entry: DATA_ENTRY.to_string(),
        data_sha256,
        session_count: usize_to_u64(counts.sessions, "session count")?,
        message_count: usize_to_u64(counts.messages, "message count")?,
        artifacts: descriptors,
    };
    entries.insert(MANIFEST_ENTRY.to_string(), canonical_json(&manifest)?);
    write_stored_zip(&entries)
}

/// Parse and fully validate a bundle before any artifact or profile commit.
pub fn preflight_portable_bundle(
    archive: &[u8],
    limits: &ImportLimits,
) -> PortabilityResult<(ValidatedPortableBundle, ImportPreflightReport)> {
    validate_import_limits(limits)?;
    let archive_size = usize_to_u64(archive.len(), "archive size")?;
    if archive_size > limits.max_archive_bytes {
        return Err(PortabilityError::Limit {
            name: "archive bytes",
            observed: archive_size,
            max: limits.max_archive_bytes,
        });
    }
    let parsed = read_stored_zip(archive, limits)?;
    let manifest_bytes = parsed
        .entries
        .get(MANIFEST_ENTRY)
        .ok_or_else(|| corrupt("manifest.json is missing"))?;
    let data_bytes = parsed
        .entries
        .get(DATA_ENTRY)
        .ok_or_else(|| corrupt("data.json is missing"))?;
    if manifest_bytes.len() > MAX_METADATA_BYTES {
        return Err(PortabilityError::Limit {
            name: "portable JSON bytes",
            observed: usize_to_u64(manifest_bytes.len(), "manifest JSON size")?,
            max: MAX_METADATA_BYTES as u64,
        });
    }
    let manifest: PortableManifestV1 = serde_json::from_slice(manifest_bytes)?;
    if manifest.format != PORTABLE_BUNDLE_FORMAT
        || manifest.schema_version != PORTABLE_BUNDLE_SCHEMA_VERSION
        || manifest.data_entry != DATA_ENTRY
    {
        return Err(invalid(
            "manifest",
            "has an unsupported format, schema version, or data entry",
        ));
    }
    validate_bundle_identity(
        &manifest.bundle_id,
        manifest.exported_at_ms,
        &manifest.app_version,
    )?;
    validate_sha256(&manifest.data_sha256, "manifest.dataSha256")?;
    if sha256_hex(data_bytes) != manifest.data_sha256 {
        return Err(corrupt("data.json digest does not match the manifest"));
    }
    let data: PortableDataV1 = serde_json::from_slice(data_bytes)?;
    let counts = validate_portable_data(&data, limits)?;
    if manifest.session_count != usize_to_u64(counts.sessions, "session count")?
        || manifest.message_count != usize_to_u64(counts.messages, "message count")?
    {
        return Err(corrupt(
            "manifest session/message counts do not match data.json",
        ));
    }
    if manifest.artifacts.len() > limits.max_artifacts {
        return Err(limit(
            "artifact count",
            manifest.artifacts.len(),
            limits.max_artifacts,
        ));
    }

    let mut artifacts = BTreeMap::new();
    let mut descriptor_ids = HashSet::new();
    let mut descriptor_entries = HashSet::new();
    for (index, descriptor) in manifest.artifacts.iter().enumerate() {
        let path = format!("manifest.artifacts[{index}]");
        validate_sha256(&descriptor.id, &format!("{path}.id"))?;
        validate_sha256(&descriptor.content_sha256, &format!("{path}.contentSha256"))?;
        if descriptor.id != descriptor.content_sha256 {
            return Err(invalid(
                format!("{path}.contentSha256"),
                "must equal the content-addressed artifact id",
            ));
        }
        validate_media_type(&descriptor.media_type, &format!("{path}.mediaType"))?;
        let expected_entry = format!("{ARTIFACT_PREFIX}{}", descriptor.id);
        if descriptor.entry != expected_entry {
            return Err(invalid(
                format!("{path}.entry"),
                "must be the canonical artifacts/<sha256> path",
            ));
        }
        if !descriptor_ids.insert(descriptor.id.clone())
            || !descriptor_entries.insert(descriptor.entry.clone())
        {
            return Err(invalid(path, "duplicates another artifact descriptor"));
        }
        let bytes = parsed
            .entries
            .get(&descriptor.entry)
            .ok_or_else(|| corrupt(format!("{} is missing", descriptor.entry)))?;
        if descriptor.byte_size != usize_to_u64(bytes.len(), "artifact byte size")?
            || sha256_hex(bytes) != descriptor.content_sha256
        {
            return Err(corrupt(format!(
                "{} size or digest does not match its descriptor",
                descriptor.entry
            )));
        }
        artifacts.insert(descriptor.id.clone(), bytes.clone());
    }
    for entry in parsed.entries.keys() {
        if entry != MANIFEST_ENTRY && entry != DATA_ENTRY && !descriptor_entries.contains(entry) {
            return Err(corrupt(format!("unexpected archive entry {entry:?}")));
        }
    }

    let referenced = data
        .sessions
        .iter()
        .flat_map(|session| &session.messages)
        .flat_map(|message| &message.attachment_ids)
        .collect::<BTreeSet<_>>();
    let declared = descriptor_ids.iter().collect::<BTreeSet<_>>();
    if referenced != declared {
        return Err(invalid(
            "data.sessions[*].messages[*].attachmentIds",
            "contains missing references or leaves undeclared artifact payloads",
        ));
    }

    let archive_sha256 = sha256_hex(archive);
    let report = ImportPreflightReport {
        archive_sha256: archive_sha256.clone(),
        entry_count: parsed.entries.len(),
        compressed_bytes: parsed.compressed_bytes,
        expanded_bytes: parsed.expanded_bytes,
        session_count: counts.sessions,
        message_count: counts.messages,
        artifact_count: artifacts.len(),
        external_reference_count: counts.external_references,
    };
    Ok((
        ValidatedPortableBundle {
            manifest,
            data,
            artifacts,
            archive_sha256,
        },
        report,
    ))
}

#[derive(Default)]
struct DataCounts {
    sessions: usize,
    messages: usize,
    external_references: usize,
}

fn validate_portable_data(
    data: &PortableDataV1,
    limits: &ImportLimits,
) -> PortabilityResult<DataCounts> {
    if data.schema_version != PORTABLE_DATA_SCHEMA_VERSION {
        return Err(invalid("data.schemaVersion", "is unsupported"));
    }
    validate_secret_free_metadata(&data.metadata, "data.metadata", 0)?;
    if data.sessions.len() > limits.max_sessions {
        return Err(limit(
            "session count",
            data.sessions.len(),
            limits.max_sessions,
        ));
    }
    let mut counts = DataCounts {
        sessions: data.sessions.len(),
        ..DataCounts::default()
    };
    let mut session_ids = HashSet::new();
    let mut session_ordinals = HashSet::new();
    let mut global_message_ids = HashSet::new();
    for (session_index, session) in data.sessions.iter().enumerate() {
        let path = format!("data.sessions[{session_index}]");
        validate_id(&session.id, &format!("{path}.id"))?;
        if !session_ids.insert(session.id.clone()) {
            return Err(invalid(format!("{path}.id"), "duplicates another session"));
        }
        if !session_ordinals.insert(session.ordinal) {
            return Err(invalid(
                format!("{path}.ordinal"),
                "duplicates another session ordinal",
            ));
        }
        validate_text(&session.title, &format!("{path}.title"), MAX_TITLE_BYTES)?;
        validate_positive_timestamp(session.created_at_ms, &format!("{path}.createdAtMs"))?;
        validate_positive_timestamp(session.updated_at_ms, &format!("{path}.updatedAtMs"))?;
        validate_optional_bounded(
            &session.model_key,
            &format!("{path}.modelKey"),
            MAX_ID_BYTES,
        )?;
        validate_optional_bounded(
            &session.persona_id,
            &format!("{path}.personaId"),
            MAX_ID_BYTES,
        )?;
        validate_optional_bounded(
            &session.workspace_path,
            &format!("{path}.workspacePath"),
            MAX_TITLE_BYTES,
        )?;
        validate_secret_free_metadata(&session.metadata, &format!("{path}.metadata"), 0)?;
        let mut message_ids = HashSet::new();
        let mut message_ordinals = HashSet::new();
        counts.messages = counts
            .messages
            .checked_add(session.messages.len())
            .ok_or_else(|| invalid(&path, "message count overflow"))?;
        if counts.messages > limits.max_messages {
            return Err(limit("message count", counts.messages, limits.max_messages));
        }
        for (message_index, message) in session.messages.iter().enumerate() {
            let message_path = format!("{path}.messages[{message_index}]");
            validate_id(&message.id, &format!("{message_path}.id"))?;
            if !message_ids.insert(message.id.clone()) {
                return Err(invalid(
                    format!("{message_path}.id"),
                    "duplicates another message in the session",
                ));
            }
            if !global_message_ids.insert(message.id.clone()) {
                return Err(invalid(
                    format!("{message_path}.id"),
                    "duplicates another message in the bundle",
                ));
            }
            if !message_ordinals.insert(message.ordinal) {
                return Err(invalid(
                    format!("{message_path}.ordinal"),
                    "duplicates another message ordinal",
                ));
            }
            validate_role(&message.role, &format!("{message_path}.role"))?;
            validate_positive_timestamp(
                message.created_at_ms,
                &format!("{message_path}.createdAtMs"),
            )?;
            validate_blocks(&message.blocks, &format!("{message_path}.blocks"))?;
            validate_secret_free_metadata(
                &message.metadata,
                &format!("{message_path}.metadata"),
                0,
            )?;
            let mut attachments = HashSet::new();
            for (attachment_index, attachment_id) in message.attachment_ids.iter().enumerate() {
                validate_sha256(
                    attachment_id,
                    &format!("{message_path}.attachmentIds[{attachment_index}]"),
                )?;
                if !attachments.insert(attachment_id) {
                    return Err(invalid(
                        format!("{message_path}.attachmentIds[{attachment_index}]"),
                        "duplicates an attachment reference",
                    ));
                }
            }
            counts.external_references = counts
                .external_references
                .checked_add(message.external_references.len())
                .ok_or_else(|| invalid(&message_path, "external reference count overflow"))?;
            if counts.external_references > limits.max_external_references {
                return Err(limit(
                    "external reference count",
                    counts.external_references,
                    limits.max_external_references,
                ));
            }
            for (reference_index, reference) in message.external_references.iter().enumerate() {
                validate_external_reference(
                    reference,
                    &format!("{message_path}.externalReferences[{reference_index}]"),
                )?;
            }
            validate_message_translations(message, &message_path)?;
        }
        validate_thread_translations(session, &message_ids, &path)?;
    }
    Ok(counts)
}

fn validate_message_translations(
    message: &PortableMessage,
    message_path: &str,
) -> PortabilityResult<()> {
    let source_json = canonical_json(&message.blocks)?;
    let source_sha256 = sha256_hex(&source_json);
    let mut locales = HashSet::new();
    for (index, translation) in message.translations.iter().enumerate() {
        let path = format!("{message_path}.translations[{index}]");
        validate_locale(&translation.locale, &format!("{path}.locale"))?;
        if !locales.insert(translation.locale.to_ascii_lowercase()) {
            return Err(invalid(
                format!("{path}.locale"),
                "duplicates another locale",
            ));
        }
        if translation.original_blocks != message.blocks {
            return Err(invalid(
                format!("{path}.originalBlocks"),
                "must preserve an exact copy of the original blocks",
            ));
        }
        validate_blocks(
            &translation.translated_blocks,
            &format!("{path}.translatedBlocks"),
        )?;
        validate_sha256(&translation.source_sha256, &format!("{path}.sourceSha256"))?;
        if translation.source_sha256 != source_sha256 {
            return Err(invalid(
                format!("{path}.sourceSha256"),
                "does not match the canonical original blocks",
            ));
        }
        validate_positive_timestamp(translation.created_at_ms, &format!("{path}.createdAtMs"))?;
        validate_secret_free_metadata(&translation.metadata, &format!("{path}.metadata"), 0)?;
    }
    Ok(())
}

fn validate_thread_translations(
    session: &PortableSession,
    message_ids: &HashSet<String>,
    session_path: &str,
) -> PortabilityResult<()> {
    let source_sha256 = sha256_hex(session.title.as_bytes());
    let mut locales = HashSet::new();
    for (index, translation) in session.translations.iter().enumerate() {
        let path = format!("{session_path}.translations[{index}]");
        validate_locale(&translation.locale, &format!("{path}.locale"))?;
        if !locales.insert(translation.locale.to_ascii_lowercase()) {
            return Err(invalid(
                format!("{path}.locale"),
                "duplicates another locale",
            ));
        }
        if translation.original_title != session.title {
            return Err(invalid(
                format!("{path}.originalTitle"),
                "must preserve the exact original session title",
            ));
        }
        validate_text(
            &translation.translated_title,
            &format!("{path}.translatedTitle"),
            MAX_TITLE_BYTES,
        )?;
        validate_sha256(&translation.source_sha256, &format!("{path}.sourceSha256"))?;
        if translation.source_sha256 != source_sha256 {
            return Err(invalid(
                format!("{path}.sourceSha256"),
                "does not match the original title",
            ));
        }
        validate_positive_timestamp(translation.created_at_ms, &format!("{path}.createdAtMs"))?;
        let mut translated_ids = HashSet::new();
        for message_id in &translation.translated_message_ids {
            if !message_ids.contains(message_id) || !translated_ids.insert(message_id) {
                return Err(invalid(
                    format!("{path}.translatedMessageIds"),
                    "must contain unique message ids from this session",
                ));
            }
        }
        validate_secret_free_metadata(&translation.metadata, &format!("{path}.metadata"), 0)?;
    }
    Ok(())
}

fn validate_blocks(blocks: &[PortableContentBlock], path: &str) -> PortabilityResult<()> {
    if blocks.is_empty() || blocks.len() > 100_000 {
        return Err(invalid(path, "must contain 1..=100000 content blocks"));
    }
    for (index, block) in blocks.iter().enumerate() {
        let block_path = format!("{path}[{index}]");
        match block {
            PortableContentBlock::Text { text } => {
                validate_text(text, &format!("{block_path}.text"), MAX_TEXT_BYTES)?;
            }
            PortableContentBlock::Code { language, code } => {
                validate_optional_bounded(language, &format!("{block_path}.language"), 128)?;
                validate_text(code, &format!("{block_path}.code"), MAX_TEXT_BYTES)?;
            }
            PortableContentBlock::Table { headers, rows } => {
                if headers.is_empty() || headers.len() > 256 || rows.len() > 100_000 {
                    return Err(invalid(
                        &block_path,
                        "table must have 1..=256 columns and at most 100000 rows",
                    ));
                }
                for (column, header) in headers.iter().enumerate() {
                    validate_text(
                        header,
                        &format!("{block_path}.headers[{column}]"),
                        MAX_TITLE_BYTES,
                    )?;
                }
                for (row_index, row) in rows.iter().enumerate() {
                    if row.len() != headers.len() {
                        return Err(invalid(
                            format!("{block_path}.rows[{row_index}]"),
                            "must have the same column count as headers",
                        ));
                    }
                    for (column, cell) in row.iter().enumerate() {
                        validate_text(
                            cell,
                            &format!("{block_path}.rows[{row_index}][{column}]"),
                            MAX_TEXT_BYTES,
                        )?;
                    }
                }
            }
        }
    }
    Ok(())
}

fn validate_secret_free_metadata(value: &Value, path: &str, depth: usize) -> PortabilityResult<()> {
    if depth > 128 {
        return Err(invalid(path, "metadata nesting exceeds 128 levels"));
    }
    let encoded = if depth == 0 {
        Some(canonical_json(value)?)
    } else {
        None
    };
    if encoded
        .as_ref()
        .is_some_and(|bytes| bytes.len() > MAX_METADATA_BYTES)
    {
        return Err(limit(
            "metadata bytes",
            encoded.as_ref().map_or(0, Vec::len),
            MAX_METADATA_BYTES,
        ));
    }
    match value {
        Value::Object(object) => {
            for (key, child) in object {
                if is_secret_key(key) {
                    return Err(invalid(
                        format!("{path}.{key}"),
                        "credential, token, cookie, password, authorization, and secret fields are forbidden",
                    ));
                }
                validate_secret_free_metadata(child, &format!("{path}.{key}"), depth + 1)?;
            }
        }
        Value::Array(values) => {
            for (index, child) in values.iter().enumerate() {
                validate_secret_free_metadata(child, &format!("{path}[{index}]"), depth + 1)?;
            }
        }
        Value::String(string) => {
            if string.len() > MAX_TEXT_BYTES {
                return Err(limit("metadata string bytes", string.len(), MAX_TEXT_BYTES));
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
    Ok(())
}

fn is_secret_key(key: &str) -> bool {
    let normalized = key
        .chars()
        .filter(|character| character.is_ascii_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect::<String>();
    [
        "credential",
        "apikey",
        "accesstoken",
        "refreshtoken",
        "authtoken",
        "authorization",
        "password",
        "privatekey",
        "clientsecret",
        "cookie",
        "sessiontoken",
    ]
    .iter()
    .any(|forbidden| normalized.contains(forbidden))
        || normalized == "secret"
        || normalized.ends_with("secret")
}

fn canonical_json<T: Serialize + ?Sized>(value: &T) -> PortabilityResult<Vec<u8>> {
    let mut value = serde_json::to_value(value)?;
    canonicalize_json_value(&mut value);
    Ok(serde_json::to_vec(&value)?)
}

fn canonicalize_json_value(value: &mut Value) {
    match value {
        Value::Array(values) => {
            for value in values {
                canonicalize_json_value(value);
            }
        }
        Value::Object(object) => {
            let previous = std::mem::take(object);
            let mut sorted = previous.into_iter().collect::<Vec<_>>();
            sorted.sort_by(|left, right| left.0.cmp(&right.0));
            for (key, mut value) in sorted {
                canonicalize_json_value(&mut value);
                object.insert(key, value);
            }
        }
        _ => {}
    }
}

#[derive(Debug)]
struct ParsedZip {
    entries: BTreeMap<String, Vec<u8>>,
    compressed_bytes: u64,
    expanded_bytes: u64,
}

fn write_stored_zip(entries: &BTreeMap<String, Vec<u8>>) -> PortabilityResult<Vec<u8>> {
    if entries.len() > usize::from(u16::MAX) {
        return Err(limit("ZIP entry count", entries.len(), u16::MAX as usize));
    }
    let mut output = Vec::new();
    let mut central_records = Vec::with_capacity(entries.len());
    for (name, bytes) in entries {
        validate_archive_entry_name(name)?;
        let name_bytes = name.as_bytes();
        let name_len = u16::try_from(name_bytes.len())
            .map_err(|_| invalid("zip.entry", "entry name exceeds ZIP32"))?;
        let size =
            u32::try_from(bytes.len()).map_err(|_| invalid("zip.entry", "entry exceeds ZIP32"))?;
        let offset = u32::try_from(output.len())
            .map_err(|_| invalid("zip", "archive offset exceeds ZIP32"))?;
        let crc = crc32(bytes);
        push_u32(&mut output, ZIP_LOCAL_SIGNATURE);
        push_u16(&mut output, 20);
        push_u16(&mut output, ZIP_UTF8_FLAG);
        push_u16(&mut output, ZIP_STORED_METHOD);
        push_u16(&mut output, 0);
        push_u16(&mut output, ZIP_MINIMUM_DATE);
        push_u32(&mut output, crc);
        push_u32(&mut output, size);
        push_u32(&mut output, size);
        push_u16(&mut output, name_len);
        push_u16(&mut output, 0);
        output.extend_from_slice(name_bytes);
        output.extend_from_slice(bytes);
        central_records.push((name, crc, size, offset));
    }
    let central_offset = u32::try_from(output.len())
        .map_err(|_| invalid("zip", "central directory offset exceeds ZIP32"))?;
    for (name, crc, size, offset) in &central_records {
        push_u32(&mut output, ZIP_CENTRAL_SIGNATURE);
        push_u16(&mut output, 0x0314);
        push_u16(&mut output, 20);
        push_u16(&mut output, ZIP_UTF8_FLAG);
        push_u16(&mut output, ZIP_STORED_METHOD);
        push_u16(&mut output, 0);
        push_u16(&mut output, ZIP_MINIMUM_DATE);
        push_u32(&mut output, *crc);
        push_u32(&mut output, *size);
        push_u32(&mut output, *size);
        push_u16(
            &mut output,
            u16::try_from(name.len())
                .map_err(|_| invalid("zip.entry", "entry name exceeds ZIP32"))?,
        );
        push_u16(&mut output, 0);
        push_u16(&mut output, 0);
        push_u16(&mut output, 0);
        push_u16(&mut output, 0);
        push_u32(&mut output, 0);
        push_u32(&mut output, *offset);
        output.extend_from_slice(name.as_bytes());
    }
    let central_size = u32::try_from(output.len())
        .ok()
        .and_then(|end| end.checked_sub(central_offset))
        .ok_or_else(|| invalid("zip", "central directory exceeds ZIP32"))?;
    push_u32(&mut output, ZIP_EOCD_SIGNATURE);
    push_u16(&mut output, 0);
    push_u16(&mut output, 0);
    let count =
        u16::try_from(entries.len()).map_err(|_| invalid("zip", "entry count exceeds ZIP32"))?;
    push_u16(&mut output, count);
    push_u16(&mut output, count);
    push_u32(&mut output, central_size);
    push_u32(&mut output, central_offset);
    push_u16(&mut output, 0);
    Ok(output)
}

fn read_stored_zip(archive: &[u8], limits: &ImportLimits) -> PortabilityResult<ParsedZip> {
    let eocd = find_eocd(archive)?;
    let disk = read_u16(archive, eocd + 4)?;
    let central_disk = read_u16(archive, eocd + 6)?;
    let disk_entries = read_u16(archive, eocd + 8)?;
    let total_entries = read_u16(archive, eocd + 10)?;
    let central_size = usize::try_from(read_u32(archive, eocd + 12)?)
        .map_err(|_| corrupt("central directory size overflow"))?;
    let central_offset = usize::try_from(read_u32(archive, eocd + 16)?)
        .map_err(|_| corrupt("central directory offset overflow"))?;
    let comment_len = usize::from(read_u16(archive, eocd + 20)?);
    if disk != 0 || central_disk != 0 || disk_entries != total_entries {
        return Err(corrupt("multi-disk ZIP archives are forbidden"));
    }
    if total_entries == u16::MAX {
        return Err(corrupt("ZIP64 archives are unsupported"));
    }
    let entry_count = usize::from(total_entries);
    if entry_count > limits.max_entries {
        return Err(limit("ZIP entry count", entry_count, limits.max_entries));
    }
    if eocd
        .checked_add(22)
        .and_then(|end| end.checked_add(comment_len))
        != Some(archive.len())
    {
        return Err(corrupt("EOCD comment length is inconsistent"));
    }
    let central_end = central_offset
        .checked_add(central_size)
        .ok_or_else(|| corrupt("central directory bounds overflow"))?;
    if central_end != eocd || central_end > archive.len() {
        return Err(corrupt("central directory does not end at EOCD"));
    }

    let mut cursor = central_offset;
    let mut metadata = Vec::with_capacity(entry_count);
    let mut names = HashSet::new();
    let mut compressed_total = 0_u64;
    let mut expanded_total = 0_u64;
    for _ in 0..entry_count {
        if read_u32(archive, cursor)? != ZIP_CENTRAL_SIGNATURE {
            return Err(corrupt("invalid central directory signature"));
        }
        let flags = read_u16(archive, cursor + 8)?;
        let method = read_u16(archive, cursor + 10)?;
        let crc = read_u32(archive, cursor + 16)?;
        let compressed = read_u32(archive, cursor + 20)?;
        let expanded = read_u32(archive, cursor + 24)?;
        let name_len = usize::from(read_u16(archive, cursor + 28)?);
        let extra_len = usize::from(read_u16(archive, cursor + 30)?);
        let entry_comment_len = usize::from(read_u16(archive, cursor + 32)?);
        let start_disk = read_u16(archive, cursor + 34)?;
        let local_offset = usize::try_from(read_u32(archive, cursor + 42)?)
            .map_err(|_| corrupt("local header offset overflow"))?;
        if start_disk != 0 || flags & 0x0001 != 0 || flags & 0x0008 != 0 {
            return Err(corrupt(
                "encrypted entries, data descriptors, and multi-disk entries are forbidden",
            ));
        }
        if flags & !ZIP_UTF8_FLAG != 0 {
            return Err(corrupt("unsupported ZIP general-purpose flags"));
        }
        let variable_start = cursor
            .checked_add(46)
            .ok_or_else(|| corrupt("central entry bounds overflow"))?;
        let variable_len = name_len
            .checked_add(extra_len)
            .and_then(|size| size.checked_add(entry_comment_len))
            .ok_or_else(|| corrupt("central variable fields overflow"))?;
        let next_cursor = variable_start
            .checked_add(variable_len)
            .ok_or_else(|| corrupt("central entry end overflow"))?;
        if next_cursor > central_end {
            return Err(corrupt("central entry exceeds directory bounds"));
        }
        let name_bytes = archive
            .get(variable_start..variable_start + name_len)
            .ok_or_else(|| corrupt("central filename is truncated"))?;
        let name = std::str::from_utf8(name_bytes)
            .map_err(|_| corrupt("entry filename is not UTF-8"))?
            .to_string();
        validate_archive_entry_name(&name)?;
        if !names.insert(name.to_ascii_lowercase()) {
            return Err(corrupt(format!("duplicate archive entry {name:?}")));
        }
        let compressed_u64 = u64::from(compressed);
        let expanded_u64 = u64::from(expanded);
        enforce_entry_limits(compressed_u64, expanded_u64, limits)?;
        compressed_total = compressed_total
            .checked_add(compressed_u64)
            .ok_or_else(|| corrupt("compressed byte total overflow"))?;
        expanded_total = expanded_total
            .checked_add(expanded_u64)
            .ok_or_else(|| corrupt("expanded byte total overflow"))?;
        if expanded_total > limits.max_total_expanded_bytes {
            return Err(PortabilityError::Limit {
                name: "total expanded bytes",
                observed: expanded_total,
                max: limits.max_total_expanded_bytes,
            });
        }
        if method != ZIP_STORED_METHOD {
            return Err(PortabilityError::UnsupportedCompression(method));
        }
        metadata.push(ZipEntryMetadata {
            name,
            flags,
            method,
            crc,
            compressed,
            expanded,
            local_offset,
        });
        cursor = next_cursor;
    }
    if cursor != central_end {
        return Err(corrupt("central directory contains trailing bytes"));
    }

    let mut entries = BTreeMap::new();
    for entry in metadata {
        let bytes = read_local_entry(archive, central_offset, &entry)?;
        entries.insert(entry.name, bytes);
    }
    Ok(ParsedZip {
        entries,
        compressed_bytes: compressed_total,
        expanded_bytes: expanded_total,
    })
}

struct ZipEntryMetadata {
    name: String,
    flags: u16,
    method: u16,
    crc: u32,
    compressed: u32,
    expanded: u32,
    local_offset: usize,
}

fn read_local_entry(
    archive: &[u8],
    central_offset: usize,
    expected: &ZipEntryMetadata,
) -> PortabilityResult<Vec<u8>> {
    let offset = expected.local_offset;
    if offset >= central_offset || read_u32(archive, offset)? != ZIP_LOCAL_SIGNATURE {
        return Err(corrupt(format!(
            "{} has an invalid local header",
            expected.name
        )));
    }
    let flags = read_u16(archive, offset + 6)?;
    let method = read_u16(archive, offset + 8)?;
    let crc = read_u32(archive, offset + 14)?;
    let compressed = read_u32(archive, offset + 18)?;
    let expanded = read_u32(archive, offset + 22)?;
    let name_len = usize::from(read_u16(archive, offset + 26)?);
    let extra_len = usize::from(read_u16(archive, offset + 28)?);
    if flags != expected.flags
        || method != expected.method
        || crc != expected.crc
        || compressed != expected.compressed
        || expanded != expected.expanded
    {
        return Err(corrupt(format!(
            "{} local and central metadata differ",
            expected.name
        )));
    }
    let name_start = offset
        .checked_add(30)
        .ok_or_else(|| corrupt("local filename offset overflow"))?;
    let data_start = name_start
        .checked_add(name_len)
        .and_then(|start| start.checked_add(extra_len))
        .ok_or_else(|| corrupt("local data offset overflow"))?;
    let data_end = data_start
        .checked_add(
            usize::try_from(compressed).map_err(|_| corrupt("compressed entry size overflow"))?,
        )
        .ok_or_else(|| corrupt("local data end overflow"))?;
    if data_end > central_offset {
        return Err(corrupt(format!(
            "{} overlaps the central directory",
            expected.name
        )));
    }
    let local_name = archive
        .get(name_start..name_start + name_len)
        .ok_or_else(|| corrupt("local filename is truncated"))?;
    if local_name != expected.name.as_bytes() {
        return Err(corrupt("local and central filenames differ"));
    }
    let bytes = archive
        .get(data_start..data_end)
        .ok_or_else(|| corrupt("entry payload is truncated"))?;
    if compressed != expanded || crc32(bytes) != expected.crc {
        return Err(corrupt(format!(
            "{} failed size or CRC validation",
            expected.name
        )));
    }
    Ok(bytes.to_vec())
}

fn find_eocd(archive: &[u8]) -> PortabilityResult<usize> {
    if archive.len() < 22 {
        return Err(corrupt("archive is shorter than EOCD"));
    }
    let last = archive.len() - 22;
    let first = archive.len().saturating_sub(22 + usize::from(u16::MAX));
    for offset in (first..=last).rev() {
        if read_u32(archive, offset).ok() == Some(ZIP_EOCD_SIGNATURE) {
            return Ok(offset);
        }
    }
    Err(corrupt("EOCD signature was not found"))
}

fn enforce_entry_limits(
    compressed: u64,
    expanded: u64,
    limits: &ImportLimits,
) -> PortabilityResult<()> {
    if compressed > limits.max_entry_compressed_bytes {
        return Err(PortabilityError::Limit {
            name: "entry compressed bytes",
            observed: compressed,
            max: limits.max_entry_compressed_bytes,
        });
    }
    if expanded > limits.max_entry_expanded_bytes {
        return Err(PortabilityError::Limit {
            name: "entry expanded bytes",
            observed: expanded,
            max: limits.max_entry_expanded_bytes,
        });
    }
    if expanded > 0
        && (compressed == 0 || expanded > compressed.saturating_mul(limits.max_decompression_ratio))
    {
        return Err(PortabilityError::Limit {
            name: "entry decompression ratio",
            observed: expanded.checked_div(compressed).unwrap_or(u64::MAX),
            max: limits.max_decompression_ratio,
        });
    }
    Ok(())
}

fn validate_archive_entry_name(name: &str) -> PortabilityResult<()> {
    if name.is_empty()
        || name.len() > 4_096
        || !name.is_ascii()
        || name.contains('\0')
        || name.contains('\\')
        || name.contains(':')
        || name.starts_with('/')
        || name.ends_with('/')
        || name
            .split('/')
            .any(|component| component.is_empty() || matches!(component, "." | ".."))
    {
        return Err(invalid("zip.entry", format!("unsafe entry name {name:?}")));
    }
    let path = Path::new(name);
    for component in path.components() {
        match component {
            Component::Normal(value) if value != "." && value != ".." => {}
            _ => return Err(invalid("zip.entry", format!("unsafe entry name {name:?}"))),
        }
    }
    Ok(())
}

fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = 0xffff_ffff_u32;
    for byte in bytes {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            let mask = 0_u32.wrapping_sub(crc & 1);
            crc = (crc >> 1) ^ (0xedb8_8320 & mask);
        }
    }
    !crc
}

fn push_u16(output: &mut Vec<u8>, value: u16) {
    output.extend_from_slice(&value.to_le_bytes());
}

fn push_u32(output: &mut Vec<u8>, value: u32) {
    output.extend_from_slice(&value.to_le_bytes());
}

fn read_u16(bytes: &[u8], offset: usize) -> PortabilityResult<u16> {
    let value = bytes
        .get(offset..offset + 2)
        .ok_or_else(|| corrupt("truncated u16 field"))?;
    Ok(u16::from_le_bytes([value[0], value[1]]))
}

fn read_u32(bytes: &[u8], offset: usize) -> PortabilityResult<u32> {
    let value = bytes
        .get(offset..offset + 4)
        .ok_or_else(|| corrupt("truncated u32 field"))?;
    Ok(u32::from_le_bytes([value[0], value[1], value[2], value[3]]))
}

/// Export one session as deterministic Markdown while retaining the exact
/// text/code/table block sequence.
pub fn export_session_markdown(session: &PortableSession) -> PortabilityResult<String> {
    validate_session_for_document(session)?;
    let mut output = String::new();
    output.push_str("# ");
    output.push_str(&session.title);
    output.push_str("\n\n");
    for message in &session.messages {
        output.push_str("## ");
        output.push_str(&display_role(&message.role));
        output.push_str("\n\n");
        for block in &message.blocks {
            match block {
                PortableContentBlock::Text { text } => {
                    output.push_str(text);
                    output.push_str("\n\n");
                }
                PortableContentBlock::Code { language, code } => {
                    let fence = markdown_fence(code);
                    output.push_str(&fence);
                    if let Some(language) = language {
                        output.push_str(language);
                    }
                    output.push('\n');
                    output.push_str(code);
                    if !code.ends_with('\n') {
                        output.push('\n');
                    }
                    output.push_str(&fence);
                    output.push_str("\n\n");
                }
                PortableContentBlock::Table { headers, rows } => {
                    markdown_table_row(&mut output, headers);
                    let divider = headers
                        .iter()
                        .map(|_| "---".to_string())
                        .collect::<Vec<_>>();
                    markdown_table_row(&mut output, &divider);
                    for row in rows {
                        markdown_table_row(&mut output, row);
                    }
                    output.push('\n');
                }
            }
        }
    }
    while output.ends_with("\n\n\n") {
        output.pop();
    }
    Ok(output)
}

/// Create a minimal, standards-valid DOCX package with deterministic stored
/// ZIP entries. The document preserves original text/code/table ordering.
pub fn export_session_docx(session: &PortableSession) -> PortabilityResult<Vec<u8>> {
    validate_session_for_document(session)?;
    let document_xml = build_document_xml(session);
    let mut entries = BTreeMap::new();
    entries.insert(
        "[Content_Types].xml".to_string(),
        br#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/><Override PartName="/word/document.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/><Override PartName="/word/styles.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.styles+xml"/></Types>"#.to_vec(),
    );
    entries.insert(
        "_rels/.rels".to_string(),
        br#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="word/document.xml"/></Relationships>"#.to_vec(),
    );
    entries.insert("word/document.xml".to_string(), document_xml.into_bytes());
    entries.insert(
        "word/_rels/document.xml.rels".to_string(),
        br#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles" Target="styles.xml"/></Relationships>"#.to_vec(),
    );
    entries.insert(
        "word/styles.xml".to_string(),
        br#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><w:styles xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:style w:type="paragraph" w:default="1" w:styleId="Normal"><w:name w:val="Normal"/></w:style><w:style w:type="paragraph" w:styleId="Title"><w:name w:val="Title"/><w:basedOn w:val="Normal"/><w:rPr><w:b/><w:sz w:val="36"/></w:rPr></w:style><w:style w:type="paragraph" w:styleId="Heading1"><w:name w:val="heading 1"/><w:basedOn w:val="Normal"/><w:rPr><w:b/><w:sz w:val="28"/></w:rPr></w:style><w:style w:type="paragraph" w:styleId="Code"><w:name w:val="Code"/><w:basedOn w:val="Normal"/><w:rPr><w:rFonts w:ascii="Courier New" w:hAnsi="Courier New"/></w:rPr></w:style></w:styles>"#.to_vec(),
    );
    write_stored_zip(&entries)
}

fn validate_session_for_document(session: &PortableSession) -> PortabilityResult<()> {
    validate_id(&session.id, "session.id")?;
    validate_text(&session.title, "session.title", MAX_TITLE_BYTES)?;
    for (index, message) in session.messages.iter().enumerate() {
        validate_role(&message.role, &format!("session.messages[{index}].role"))?;
        validate_blocks(
            &message.blocks,
            &format!("session.messages[{index}].blocks"),
        )?;
    }
    Ok(())
}

fn build_document_xml(session: &PortableSession) -> String {
    let mut xml = String::from(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body>"#,
    );
    word_paragraph(&mut xml, "Title", &session.title);
    for message in &session.messages {
        word_paragraph(&mut xml, "Heading1", &display_role(&message.role));
        for block in &message.blocks {
            match block {
                PortableContentBlock::Text { text } => {
                    for line in text.split('\n') {
                        word_paragraph(&mut xml, "Normal", line);
                    }
                }
                PortableContentBlock::Code { code, .. } => {
                    for line in code.split('\n') {
                        word_paragraph(&mut xml, "Code", line);
                    }
                }
                PortableContentBlock::Table { headers, rows } => {
                    xml.push_str("<w:tbl><w:tblPr><w:tblBorders><w:top w:val=\"single\"/><w:left w:val=\"single\"/><w:bottom w:val=\"single\"/><w:right w:val=\"single\"/><w:insideH w:val=\"single\"/><w:insideV w:val=\"single\"/></w:tblBorders></w:tblPr>");
                    word_table_row(&mut xml, headers, true);
                    for row in rows {
                        word_table_row(&mut xml, row, false);
                    }
                    xml.push_str("</w:tbl>");
                }
            }
        }
    }
    xml.push_str("<w:sectPr><w:pgSz w:w=\"12240\" w:h=\"15840\"/><w:pgMar w:top=\"1440\" w:right=\"1440\" w:bottom=\"1440\" w:left=\"1440\"/></w:sectPr></w:body></w:document>");
    xml
}

fn word_paragraph(output: &mut String, style: &str, text: &str) {
    output.push_str("<w:p><w:pPr><w:pStyle w:val=\"");
    output.push_str(style);
    output.push_str("\"/></w:pPr><w:r><w:t xml:space=\"preserve\">");
    output.push_str(&xml_escape(text));
    output.push_str("</w:t></w:r></w:p>");
}

fn word_table_row(output: &mut String, cells: &[String], bold: bool) {
    output.push_str("<w:tr>");
    for cell in cells {
        output.push_str("<w:tc><w:p><w:r>");
        if bold {
            output.push_str("<w:rPr><w:b/></w:rPr>");
        }
        output.push_str("<w:t xml:space=\"preserve\">");
        output.push_str(&xml_escape(cell));
        output.push_str("</w:t></w:r></w:p></w:tc>");
    }
    output.push_str("</w:tr>");
}

fn xml_escape(value: &str) -> String {
    let mut output = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '&' => output.push_str("&amp;"),
            '<' => output.push_str("&lt;"),
            '>' => output.push_str("&gt;"),
            '"' => output.push_str("&quot;"),
            '\'' => output.push_str("&apos;"),
            '\t' | '\n' | '\r' => output.push(character),
            character if character >= ' ' && character != '\u{fffe}' && character != '\u{ffff}' => {
                output.push(character)
            }
            _ => output.push('\u{fffd}'),
        }
    }
    output
}

fn display_role(role: &str) -> String {
    let mut chars = role.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

fn markdown_fence(code: &str) -> String {
    let longest = code
        .split(|character| character != '`')
        .map(str::len)
        .max()
        .unwrap_or(0);
    "`".repeat(longest.saturating_add(1).max(3))
}

fn markdown_table_row(output: &mut String, values: &[String]) {
    output.push('|');
    for value in values {
        output.push(' ');
        output.push_str(
            &value
                .replace('\\', "\\\\")
                .replace('|', "\\|")
                .replace('\n', "<br>"),
        );
        output.push_str(" |");
    }
    output.push('\n');
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PublishedPortableArtifact {
    pub id: String,
    pub media_type: String,
    pub blob: ArtifactBlob,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PortableImportOutcome {
    pub commit_id: String,
    pub preflight: ImportPreflightReport,
    pub published_artifacts: Vec<PublishedPortableArtifact>,
}

/// Transaction boundary supplied by the owning profile implementation. The
/// method must commit all session/message/translation rows atomically and
/// return an opaque durable commit id. Content-addressed artifact publication
/// occurs first; a failed commit can leave only harmless unreferenced blobs.
pub trait PortableImportTarget {
    fn commit_validated_bundle(
        &mut self,
        bundle: &ValidatedPortableBundle,
        published_artifacts: &[PublishedPortableArtifact],
    ) -> Result<String, String>;
}

/// Complete import flow: hostile-input preflight first, content-addressed
/// artifact publication second, and one injected atomic profile commit last.
pub fn import_portable_bundle<T: PortableImportTarget>(
    archive: &[u8],
    limits: &ImportLimits,
    artifact_store: &ArtifactStore,
    target: &mut T,
) -> PortabilityResult<PortableImportOutcome> {
    let (bundle, preflight) = preflight_portable_bundle(archive, limits)?;
    let mut published_artifacts = Vec::with_capacity(bundle.artifacts.len());
    for descriptor in &bundle.manifest.artifacts {
        let bytes = bundle
            .artifacts
            .get(&descriptor.id)
            .ok_or_else(|| corrupt("validated artifact disappeared"))?;
        if usize_to_u64(bytes.len(), "artifact byte size")? > artifact_store.max_blob_size() {
            return Err(PortabilityError::Limit {
                name: "artifact store blob bytes",
                observed: usize_to_u64(bytes.len(), "artifact byte size")?,
                max: artifact_store.max_blob_size(),
            });
        }
    }
    for descriptor in &bundle.manifest.artifacts {
        let bytes = bundle
            .artifacts
            .get(&descriptor.id)
            .ok_or_else(|| corrupt("validated artifact disappeared"))?;
        let blob = artifact_store.put(bytes)?;
        if blob.id != descriptor.id || blob.size != descriptor.byte_size {
            return Err(corrupt(
                "artifact store returned an unexpected digest or size",
            ));
        }
        published_artifacts.push(PublishedPortableArtifact {
            id: descriptor.id.clone(),
            media_type: descriptor.media_type.clone(),
            blob,
        });
    }
    let commit_id = target
        .commit_validated_bundle(&bundle, &published_artifacts)
        .map_err(PortabilityError::Commit)?;
    validate_id(&commit_id, "commitId")?;
    Ok(PortableImportOutcome {
        commit_id,
        preflight,
        published_artifacts,
    })
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CryptoSeal {
    pub nonce: Vec<u8>,
    pub ciphertext: Vec<u8>,
    pub tag: Vec<u8>,
}

/// Injected authenticated-encryption boundary. Production implementations
/// must use an audited AEAD, a fresh nonce per seal, and bind every `aad` byte.
/// No fallback or home-grown production cipher exists in this module.
pub trait CryptoProvider {
    fn algorithm_id(&self) -> &str;
    fn key_id(&self) -> &str;
    fn seal(&self, aad: &[u8], plaintext: &[u8]) -> Result<CryptoSeal, String>;
    fn open(
        &self,
        aad: &[u8],
        nonce: &[u8],
        ciphertext: &[u8],
        tag: &[u8],
    ) -> Result<Vec<u8>, String>;
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EncryptedSnapshotEnvelopeV1 {
    pub format: String,
    pub schema_version: u32,
    pub created_at_ms: u64,
    pub source_revision_sha256: String,
    pub bundle_sha256: String,
    pub algorithm: String,
    pub key_id: String,
    pub nonce_base64: String,
    pub ciphertext_base64: String,
    pub tag_base64: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SnapshotAad<'a> {
    format: &'a str,
    schema_version: u32,
    created_at_ms: u64,
    source_revision_sha256: &'a str,
    bundle_sha256: &'a str,
    algorithm: &'a str,
    key_id: &'a str,
}

#[derive(Clone, Debug, PartialEq)]
pub struct OpenedEncryptedSnapshot {
    pub envelope: EncryptedSnapshotEnvelopeV1,
    pub bundle: ValidatedPortableBundle,
    pub preflight: ImportPreflightReport,
}

pub fn seal_encrypted_snapshot(
    bundle_archive: &[u8],
    source_revision_sha256: &str,
    created_at_ms: u64,
    limits: &ImportLimits,
    crypto: &dyn CryptoProvider,
) -> PortabilityResult<Vec<u8>> {
    validate_sha256(source_revision_sha256, "sourceRevisionSha256")?;
    validate_positive_timestamp(created_at_ms, "createdAtMs")?;
    let _ = preflight_portable_bundle(bundle_archive, limits)?;
    validate_crypto_identity(crypto.algorithm_id(), crypto.key_id())?;
    let bundle_sha256 = sha256_hex(bundle_archive);
    let aad = canonical_json(&SnapshotAad {
        format: ENCRYPTED_SNAPSHOT_FORMAT,
        schema_version: ENCRYPTED_SNAPSHOT_SCHEMA_VERSION,
        created_at_ms,
        source_revision_sha256,
        bundle_sha256: &bundle_sha256,
        algorithm: crypto.algorithm_id(),
        key_id: crypto.key_id(),
    })?;
    let sealed = crypto
        .seal(&aad, bundle_archive)
        .map_err(PortabilityError::Crypto)?;
    validate_seal_parts(&sealed, limits)?;
    let envelope = EncryptedSnapshotEnvelopeV1 {
        format: ENCRYPTED_SNAPSHOT_FORMAT.to_string(),
        schema_version: ENCRYPTED_SNAPSHOT_SCHEMA_VERSION,
        created_at_ms,
        source_revision_sha256: source_revision_sha256.to_string(),
        bundle_sha256,
        algorithm: crypto.algorithm_id().to_string(),
        key_id: crypto.key_id().to_string(),
        nonce_base64: base64::engine::general_purpose::STANDARD.encode(sealed.nonce),
        ciphertext_base64: base64::engine::general_purpose::STANDARD.encode(sealed.ciphertext),
        tag_base64: base64::engine::general_purpose::STANDARD.encode(sealed.tag),
    };
    canonical_json(&envelope)
}

pub fn open_encrypted_snapshot(
    envelope_json: &[u8],
    limits: &ImportLimits,
    crypto: &dyn CryptoProvider,
) -> PortabilityResult<OpenedEncryptedSnapshot> {
    let envelope_limit = limits.max_archive_bytes.saturating_mul(2);
    if usize_to_u64(envelope_json.len(), "snapshot envelope bytes")? > envelope_limit {
        return Err(PortabilityError::Limit {
            name: "snapshot envelope bytes",
            observed: usize_to_u64(envelope_json.len(), "snapshot envelope bytes")?,
            max: envelope_limit,
        });
    }
    let envelope: EncryptedSnapshotEnvelopeV1 = serde_json::from_slice(envelope_json)?;
    if envelope.format != ENCRYPTED_SNAPSHOT_FORMAT
        || envelope.schema_version != ENCRYPTED_SNAPSHOT_SCHEMA_VERSION
    {
        return Err(invalid(
            "snapshot",
            "has an unsupported format or schema version",
        ));
    }
    validate_positive_timestamp(envelope.created_at_ms, "snapshot.createdAtMs")?;
    validate_sha256(
        &envelope.source_revision_sha256,
        "snapshot.sourceRevisionSha256",
    )?;
    validate_sha256(&envelope.bundle_sha256, "snapshot.bundleSha256")?;
    validate_crypto_identity(&envelope.algorithm, &envelope.key_id)?;
    if envelope.algorithm != crypto.algorithm_id() || envelope.key_id != crypto.key_id() {
        return Err(PortabilityError::Crypto(
            "snapshot algorithm or key id does not match the provider".to_string(),
        ));
    }
    let nonce = decode_bounded_base64(&envelope.nonce_base64, 4_096, "snapshot.nonceBase64")?;
    let ciphertext = decode_bounded_base64(
        &envelope.ciphertext_base64,
        limits.max_archive_bytes,
        "snapshot.ciphertextBase64",
    )?;
    let tag = decode_bounded_base64(&envelope.tag_base64, 4_096, "snapshot.tagBase64")?;
    if nonce.is_empty() || tag.len() < 12 {
        return Err(invalid(
            "snapshot",
            "authenticated encryption requires a non-empty nonce and at least a 96-bit tag",
        ));
    }
    let aad = canonical_json(&SnapshotAad {
        format: &envelope.format,
        schema_version: envelope.schema_version,
        created_at_ms: envelope.created_at_ms,
        source_revision_sha256: &envelope.source_revision_sha256,
        bundle_sha256: &envelope.bundle_sha256,
        algorithm: &envelope.algorithm,
        key_id: &envelope.key_id,
    })?;
    let plaintext = crypto
        .open(&aad, &nonce, &ciphertext, &tag)
        .map_err(PortabilityError::Crypto)?;
    if sha256_hex(&plaintext) != envelope.bundle_sha256 {
        return Err(PortabilityError::Crypto(
            "decrypted bundle digest does not match the authenticated envelope".to_string(),
        ));
    }
    let (bundle, preflight) = preflight_portable_bundle(&plaintext, limits)?;
    Ok(OpenedEncryptedSnapshot {
        envelope,
        bundle,
        preflight,
    })
}

fn validate_crypto_identity(algorithm: &str, key_id: &str) -> PortabilityResult<()> {
    validate_text(algorithm, "crypto.algorithm", 128)?;
    validate_id(key_id, "crypto.keyId")?;
    if algorithm.trim().is_empty() || algorithm.eq_ignore_ascii_case("none") {
        return Err(invalid(
            "crypto.algorithm",
            "must identify a production authenticated-encryption algorithm",
        ));
    }
    Ok(())
}

fn validate_seal_parts(sealed: &CryptoSeal, limits: &ImportLimits) -> PortabilityResult<()> {
    if sealed.nonce.is_empty()
        || sealed.nonce.len() > 4_096
        || sealed.tag.len() < 12
        || sealed.tag.len() > 4_096
    {
        return Err(invalid(
            "crypto.seal",
            "nonce must be 1..=4096 bytes and authentication tag 12..=4096 bytes",
        ));
    }
    if usize_to_u64(sealed.ciphertext.len(), "ciphertext bytes")? > limits.max_archive_bytes {
        return Err(PortabilityError::Limit {
            name: "ciphertext bytes",
            observed: usize_to_u64(sealed.ciphertext.len(), "ciphertext bytes")?,
            max: limits.max_archive_bytes,
        });
    }
    Ok(())
}

fn decode_bounded_base64(value: &str, max_decoded: u64, path: &str) -> PortabilityResult<Vec<u8>> {
    let estimated = value
        .len()
        .checked_add(3)
        .and_then(|size| size.checked_div(4))
        .and_then(|size| size.checked_mul(3))
        .ok_or_else(|| invalid(path, "base64 size overflow"))?;
    if usize_to_u64(estimated, "base64 decoded estimate")? > max_decoded {
        return Err(PortabilityError::Limit {
            name: "base64 decoded bytes",
            observed: usize_to_u64(estimated, "base64 decoded estimate")?,
            max: max_decoded,
        });
    }
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(value)
        .map_err(|error| invalid(path, format!("invalid base64: {error}")))?;
    if usize_to_u64(decoded.len(), "decoded bytes")? > max_decoded {
        return Err(PortabilityError::Limit {
            name: "decoded bytes",
            observed: usize_to_u64(decoded.len(), "decoded bytes")?,
            max: max_decoded,
        });
    }
    Ok(decoded)
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase", deny_unknown_fields)]
pub struct SnapshotRetentionPolicy {
    pub max_count: usize,
    pub max_total_bytes: u64,
    pub max_age_ms: Option<u64>,
}

impl Default for SnapshotRetentionPolicy {
    fn default() -> Self {
        Self {
            max_count: 30,
            max_total_bytes: 10 * 1024 * 1024 * 1024,
            max_age_ms: Some(90 * 24 * 60 * 60 * 1_000),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SnapshotFileInfo {
    pub path: PathBuf,
    pub created_at_ms: u64,
    pub byte_size: u64,
    pub sha256: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SnapshotWriteOutcome {
    pub snapshot: SnapshotFileInfo,
    pub already_existed: bool,
    pub pruned: Vec<PathBuf>,
}

pub fn write_snapshot_with_retention(
    root: impl AsRef<Path>,
    envelope_json: &[u8],
    policy: &SnapshotRetentionPolicy,
    now_ms: u64,
) -> PortabilityResult<SnapshotWriteOutcome> {
    validate_retention_policy(policy)?;
    validate_positive_timestamp(now_ms, "nowMs")?;
    let envelope: EncryptedSnapshotEnvelopeV1 = serde_json::from_slice(envelope_json)?;
    if envelope.format != ENCRYPTED_SNAPSHOT_FORMAT
        || envelope.schema_version != ENCRYPTED_SNAPSHOT_SCHEMA_VERSION
    {
        return Err(invalid(
            "snapshot",
            "has an unsupported format or schema version",
        ));
    }
    validate_positive_timestamp(envelope.created_at_ms, "snapshot.createdAtMs")?;
    validate_sha256(
        &envelope.source_revision_sha256,
        "snapshot.sourceRevisionSha256",
    )?;
    validate_sha256(&envelope.bundle_sha256, "snapshot.bundleSha256")?;
    validate_crypto_identity(&envelope.algorithm, &envelope.key_id)?;
    let limits = ImportLimits::default();
    let nonce = decode_bounded_base64(&envelope.nonce_base64, 4_096, "snapshot.nonceBase64")?;
    let _ciphertext = decode_bounded_base64(
        &envelope.ciphertext_base64,
        limits.max_archive_bytes,
        "snapshot.ciphertextBase64",
    )?;
    let tag = decode_bounded_base64(&envelope.tag_base64, 4_096, "snapshot.tagBase64")?;
    if nonce.is_empty() || tag.len() < 12 {
        return Err(invalid(
            "snapshot",
            "authenticated encryption requires a non-empty nonce and at least a 96-bit tag",
        ));
    }
    if canonical_json(&envelope)? != envelope_json {
        return Err(invalid(
            "snapshot",
            "envelope JSON must use the canonical serialized representation",
        ));
    }
    if envelope.created_at_ms > now_ms {
        return Err(invalid(
            "snapshot.createdAtMs",
            "must not be later than the retention clock",
        ));
    }
    if policy
        .max_age_ms
        .is_some_and(|max_age| now_ms - envelope.created_at_ms > max_age)
    {
        return Err(invalid(
            "snapshot.createdAtMs",
            "is already outside the configured retention window",
        ));
    }
    let envelope_size = usize_to_u64(envelope_json.len(), "snapshot byte size")?;
    if envelope_size > policy.max_total_bytes {
        return Err(PortabilityError::Limit {
            name: "snapshot byte size",
            observed: envelope_size,
            max: policy.max_total_bytes,
        });
    }
    let root = root.as_ref();
    ensure_private_directory(root)?;
    let digest = sha256_hex(envelope_json);
    let filename = format!(
        "snapshot-{:020}-{}.lmsnapshot",
        envelope.created_at_ms,
        &digest[..16]
    );
    let destination = root.join(filename);
    let mut already_existed = false;
    match fs::symlink_metadata(&destination) {
        Ok(metadata) => {
            if !metadata.file_type().is_file() {
                return Err(invalid(
                    "snapshot.destination",
                    "existing destination is not a regular file",
                ));
            }
            let existing = fs::read(&destination)
                .map_err(|source| io_error("read existing snapshot", &destination, source))?;
            if existing != envelope_json {
                return Err(PortabilityError::Conflict(format!(
                    "{} exists with different bytes",
                    destination.display()
                )));
            }
            already_existed = true;
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            let temporary = root.join(format!(".snapshot-{}.tmp", Uuid::new_v4()));
            let mut options = OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            options.mode(0o600);
            let mut file = options
                .open(&temporary)
                .map_err(|source| io_error("create snapshot staging file", &temporary, source))?;
            if let Err(source) = file.write_all(envelope_json).and_then(|_| file.sync_all()) {
                let _ = fs::remove_file(&temporary);
                return Err(io_error("write snapshot staging file", &temporary, source));
            }
            if let Err(source) = fs::rename(&temporary, &destination) {
                let _ = fs::remove_file(&temporary);
                return Err(io_error("publish snapshot", &destination, source));
            }
            sync_directory(root)?;
        }
        Err(source) => {
            return Err(io_error(
                "inspect snapshot destination",
                &destination,
                source,
            ))
        }
    }
    harden_private_file(&destination)?;

    let snapshot = SnapshotFileInfo {
        path: destination.clone(),
        created_at_ms: envelope.created_at_ms,
        byte_size: envelope_size,
        sha256: digest,
    };
    let pruned = prune_snapshots(root, &destination, policy, now_ms)?;
    Ok(SnapshotWriteOutcome {
        snapshot,
        already_existed,
        pruned,
    })
}

pub fn list_snapshot_files(root: impl AsRef<Path>) -> PortabilityResult<Vec<SnapshotFileInfo>> {
    let root = root.as_ref();
    match fs::symlink_metadata(root) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(source) => return Err(io_error("inspect snapshot directory", root, source)),
        Ok(metadata) if !metadata.file_type().is_dir() => {
            return Err(invalid("snapshot.root", "is not a directory"))
        }
        Ok(_) => {}
    }
    let mut snapshots = Vec::new();
    for entry in fs::read_dir(root).map_err(|source| io_error("list snapshots", root, source))? {
        let entry =
            entry.map_err(|source| io_error("read snapshot directory entry", root, source))?;
        let path = entry.path();
        let Some(created_at_ms) = snapshot_timestamp_from_name(&entry.file_name()) else {
            continue;
        };
        let metadata = fs::symlink_metadata(&path)
            .map_err(|source| io_error("inspect snapshot", &path, source))?;
        if !metadata.file_type().is_file() {
            continue;
        }
        let bytes = fs::read(&path).map_err(|source| io_error("read snapshot", &path, source))?;
        snapshots.push(SnapshotFileInfo {
            path,
            created_at_ms,
            byte_size: metadata.len(),
            sha256: sha256_hex(&bytes),
        });
    }
    snapshots.sort_by(|left, right| {
        right
            .created_at_ms
            .cmp(&left.created_at_ms)
            .then_with(|| right.path.cmp(&left.path))
    });
    Ok(snapshots)
}

fn prune_snapshots(
    root: &Path,
    just_written: &Path,
    policy: &SnapshotRetentionPolicy,
    now_ms: u64,
) -> PortabilityResult<Vec<PathBuf>> {
    let mut snapshots = list_snapshot_files(root)?;
    snapshots.sort_by(|left, right| {
        let left_priority = left.path == just_written;
        let right_priority = right.path == just_written;
        right_priority.cmp(&left_priority).then_with(|| {
            right
                .created_at_ms
                .cmp(&left.created_at_ms)
                .then_with(|| right.path.cmp(&left.path))
        })
    });
    let mut kept_count = 0_usize;
    let mut kept_bytes = 0_u64;
    let mut pruned = Vec::new();
    for snapshot in snapshots {
        let age_expired = policy
            .max_age_ms
            .is_some_and(|max_age| now_ms.saturating_sub(snapshot.created_at_ms) > max_age);
        let count_exceeded = kept_count >= policy.max_count;
        let bytes_exceeded = kept_bytes
            .checked_add(snapshot.byte_size)
            .is_none_or(|total| total > policy.max_total_bytes);
        if age_expired || count_exceeded || bytes_exceeded {
            fs::remove_file(&snapshot.path)
                .map_err(|source| io_error("prune snapshot", &snapshot.path, source))?;
            pruned.push(snapshot.path);
        } else {
            kept_count += 1;
            kept_bytes = kept_bytes.saturating_add(snapshot.byte_size);
        }
    }
    if !pruned.is_empty() {
        sync_directory(root)?;
    }
    Ok(pruned)
}

fn snapshot_timestamp_from_name(name: &std::ffi::OsStr) -> Option<u64> {
    let name = name.to_str()?;
    let rest = name.strip_prefix("snapshot-")?;
    let (timestamp, suffix) = rest.split_once('-')?;
    if timestamp.len() != 20 || suffix.len() != 27 || !suffix.ends_with(".lmsnapshot") {
        return None;
    }
    timestamp.parse().ok()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RestoreConflictPolicy {
    Abort,
    KeepBoth,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum SnapshotRestoreOutcome {
    Replaced {
        commit_id: String,
    },
    Conflict {
        expected_revision: String,
        current_revision: String,
    },
    ConflictCopy {
        commit_id: String,
        conflict_id: String,
        current_revision: String,
    },
}

/// Restore target methods must perform compare-and-swap atomically. A
/// `commit_conflict_copy` must never alter the current profile.
pub trait SnapshotRestoreTarget {
    fn current_revision_sha256(&self) -> Result<String, String>;
    fn commit_replace(
        &mut self,
        bundle: &ValidatedPortableBundle,
        expected_current_revision: &str,
    ) -> Result<String, String>;
    fn commit_conflict_copy(
        &mut self,
        bundle: &ValidatedPortableBundle,
        conflict_id: &str,
        observed_current_revision: &str,
    ) -> Result<String, String>;
}

pub fn restore_opened_snapshot<T: SnapshotRestoreTarget>(
    snapshot: &OpenedEncryptedSnapshot,
    policy: RestoreConflictPolicy,
    target: &mut T,
) -> PortabilityResult<SnapshotRestoreOutcome> {
    let expected = &snapshot.envelope.source_revision_sha256;
    let current = target
        .current_revision_sha256()
        .map_err(PortabilityError::Commit)?;
    validate_sha256(&current, "restore.currentRevision")?;
    if &current == expected {
        let commit_id = target
            .commit_replace(&snapshot.bundle, expected)
            .map_err(PortabilityError::Commit)?;
        validate_id(&commit_id, "restore.commitId")?;
        return Ok(SnapshotRestoreOutcome::Replaced { commit_id });
    }
    match policy {
        RestoreConflictPolicy::Abort => Ok(SnapshotRestoreOutcome::Conflict {
            expected_revision: expected.clone(),
            current_revision: current,
        }),
        RestoreConflictPolicy::KeepBoth => {
            let conflict_id = format!(
                "restore-conflict-{}-{}",
                snapshot.envelope.created_at_ms,
                &snapshot.bundle.archive_sha256[..16]
            );
            let commit_id = target
                .commit_conflict_copy(&snapshot.bundle, &conflict_id, &current)
                .map_err(PortabilityError::Commit)?;
            validate_id(&commit_id, "restore.commitId")?;
            Ok(SnapshotRestoreOutcome::ConflictCopy {
                commit_id,
                conflict_id,
                current_revision: current,
            })
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WebDavPutCondition {
    IfMatch(String),
    IfNoneMatch,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WebDavPutResult {
    Stored { etag: String },
    PreconditionFailed { current_etag: Option<String> },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WebDavGetResult {
    Found { etag: String, bytes: Vec<u8> },
    NotModified,
    Missing,
}

/// Synchronous, injected WebDAV boundary. Implementations own HTTP/auth; this
/// module supplies safe paths, ETag conditions, validation, and conflict flow.
pub trait WebDavTransport {
    fn put(
        &mut self,
        remote_path: &str,
        bytes: &[u8],
        condition: WebDavPutCondition,
    ) -> Result<WebDavPutResult, String>;
    fn get(
        &mut self,
        remote_path: &str,
        if_none_match: Option<&str>,
    ) -> Result<WebDavGetResult, String>;
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum WebDavUploadOutcome {
    Uploaded {
        remote_path: String,
        etag: String,
    },
    ConflictCopy {
        remote_path: String,
        etag: String,
        conflicting_etag: Option<String>,
    },
}

pub fn upload_snapshot_webdav<T: WebDavTransport>(
    transport: &mut T,
    remote_path: &str,
    snapshot_bytes: &[u8],
    known_etag: Option<&str>,
    device_id: &str,
    now_ms: u64,
) -> PortabilityResult<WebDavUploadOutcome> {
    validate_remote_path(remote_path)?;
    validate_id(device_id, "webdav.deviceId")?;
    validate_positive_timestamp(now_ms, "webdav.nowMs")?;
    if snapshot_bytes.is_empty() {
        return Err(invalid("webdav.snapshot", "must not be empty"));
    }
    let condition = match known_etag {
        Some(etag) => {
            validate_etag(etag)?;
            WebDavPutCondition::IfMatch(etag.to_string())
        }
        None => WebDavPutCondition::IfNoneMatch,
    };
    match transport
        .put(remote_path, snapshot_bytes, condition)
        .map_err(PortabilityError::Transport)?
    {
        WebDavPutResult::Stored { etag } => {
            validate_etag(&etag)?;
            Ok(WebDavUploadOutcome::Uploaded {
                remote_path: remote_path.to_string(),
                etag,
            })
        }
        WebDavPutResult::PreconditionFailed { current_etag } => {
            if let Some(etag) = current_etag.as_deref() {
                validate_etag(etag)?;
            }
            let conflict_path =
                conflict_remote_path(remote_path, device_id, now_ms, &sha256_hex(snapshot_bytes))?;
            match transport
                .put(
                    &conflict_path,
                    snapshot_bytes,
                    WebDavPutCondition::IfNoneMatch,
                )
                .map_err(PortabilityError::Transport)?
            {
                WebDavPutResult::Stored { etag } => {
                    validate_etag(&etag)?;
                    Ok(WebDavUploadOutcome::ConflictCopy {
                        remote_path: conflict_path,
                        etag,
                        conflicting_etag: current_etag,
                    })
                }
                WebDavPutResult::PreconditionFailed { .. } => Err(PortabilityError::Conflict(
                    "deterministic WebDAV conflict-copy path already exists".to_string(),
                )),
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum WebDavDownloadOutcome {
    Downloaded {
        remote_path: String,
        etag: String,
        snapshot: Box<OpenedEncryptedSnapshot>,
    },
    NotModified,
    Missing,
}

pub fn download_snapshot_webdav<T: WebDavTransport>(
    transport: &mut T,
    remote_path: &str,
    known_etag: Option<&str>,
    limits: &ImportLimits,
    crypto: &dyn CryptoProvider,
) -> PortabilityResult<WebDavDownloadOutcome> {
    validate_remote_path(remote_path)?;
    if let Some(etag) = known_etag {
        validate_etag(etag)?;
    }
    match transport
        .get(remote_path, known_etag)
        .map_err(PortabilityError::Transport)?
    {
        WebDavGetResult::Found { etag, bytes } => {
            validate_etag(&etag)?;
            let snapshot = open_encrypted_snapshot(&bytes, limits, crypto)?;
            Ok(WebDavDownloadOutcome::Downloaded {
                remote_path: remote_path.to_string(),
                etag,
                snapshot: Box::new(snapshot),
            })
        }
        WebDavGetResult::NotModified => Ok(WebDavDownloadOutcome::NotModified),
        WebDavGetResult::Missing => Ok(WebDavDownloadOutcome::Missing),
    }
}

fn conflict_remote_path(
    remote_path: &str,
    device_id: &str,
    now_ms: u64,
    digest: &str,
) -> PortabilityResult<String> {
    let file_start = remote_path.rfind('/').map_or(0, |slash| slash + 1);
    let extension_split = remote_path[file_start..]
        .rfind('.')
        .filter(|dot| *dot > 0 && file_start + dot + 1 < remote_path.len())
        .map(|dot| file_start + dot);
    let (stem, extension) = extension_split.map_or((remote_path, ""), |dot| {
        (&remote_path[..dot], &remote_path[dot + 1..])
    });
    let device_hash = sha256_hex(device_id.as_bytes());
    let path = if extension.is_empty() {
        format!(
            "{stem}.conflict-{}-{now_ms}-{}",
            &device_hash[..12],
            &digest[..12]
        )
    } else {
        format!(
            "{stem}.conflict-{}-{now_ms}-{}.{}",
            &device_hash[..12],
            &digest[..12],
            extension
        )
    };
    validate_remote_path(&path)?;
    Ok(path)
}

fn validate_remote_path(path: &str) -> PortabilityResult<()> {
    if path.is_empty()
        || path.len() > MAX_REMOTE_PATH_BYTES
        || path.starts_with('/')
        || path.ends_with('/')
        || path.contains('\\')
        || path.contains('\0')
        || path.contains('?')
        || path.contains('#')
        || path
            .split('/')
            .any(|component| component.is_empty() || matches!(component, "." | ".."))
    {
        return Err(invalid("webdav.remotePath", "is unsafe or malformed"));
    }
    for component in Path::new(path).components() {
        match component {
            Component::Normal(value) if value != "." && value != ".." => {}
            _ => return Err(invalid("webdav.remotePath", "contains traversal")),
        }
    }
    Ok(())
}

fn validate_etag(etag: &str) -> PortabilityResult<()> {
    if etag.is_empty() || etag.len() > 1_024 || etag.chars().any(char::is_control) {
        return Err(invalid(
            "webdav.etag",
            "is empty, oversized, or contains controls",
        ));
    }
    Ok(())
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LaunchCatchUpScheduler {
    pub anchor_ms: u64,
    pub interval_ms: u64,
    pub max_catch_up_runs: usize,
    pub last_successful_slot_ms: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ScheduledSyncRun {
    pub scheduled_for_ms: u64,
    pub skipped_older_slots: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CatchUpExecutionReport {
    pub planned: usize,
    pub completed: usize,
    pub last_successful_slot_ms: Option<u64>,
}

impl LaunchCatchUpScheduler {
    pub fn validate(&self) -> PortabilityResult<()> {
        validate_positive_timestamp(self.anchor_ms, "scheduler.anchorMs")?;
        if self.interval_ms == 0 || self.interval_ms > 365 * 24 * 60 * 60 * 1_000 {
            return Err(invalid(
                "scheduler.intervalMs",
                "must be between 1 ms and 365 days",
            ));
        }
        if self.max_catch_up_runs == 0 || self.max_catch_up_runs > 1_000 {
            return Err(invalid(
                "scheduler.maxCatchUpRuns",
                "must be between 1 and 1000",
            ));
        }
        if self
            .last_successful_slot_ms
            .is_some_and(|last| last < self.anchor_ms)
        {
            return Err(invalid(
                "scheduler.lastSuccessfulSlotMs",
                "must not precede the anchor",
            ));
        }
        if self
            .last_successful_slot_ms
            .is_some_and(|last| !(last - self.anchor_ms).is_multiple_of(self.interval_ms))
        {
            return Err(invalid(
                "scheduler.lastSuccessfulSlotMs",
                "must align to an interval slot from the anchor",
            ));
        }
        Ok(())
    }

    pub fn plan_launch_catch_up(&self, now_ms: u64) -> PortabilityResult<Vec<ScheduledSyncRun>> {
        self.validate()?;
        validate_positive_timestamp(now_ms, "scheduler.nowMs")?;
        if now_ms < self.anchor_ms {
            return Ok(Vec::new());
        }
        let first_due = self
            .last_successful_slot_ms
            .map_or(self.anchor_ms, |last| last.saturating_add(self.interval_ms));
        if first_due > now_ms {
            return Ok(Vec::new());
        }
        let total_due = (now_ms - first_due) / self.interval_ms + 1;
        let retained = total_due.min(self.max_catch_up_runs as u64);
        let skipped = total_due - retained;
        let start = first_due.saturating_add(skipped.saturating_mul(self.interval_ms));
        let mut runs = Vec::with_capacity(retained as usize);
        for index in 0..retained {
            runs.push(ScheduledSyncRun {
                scheduled_for_ms: start.saturating_add(index.saturating_mul(self.interval_ms)),
                skipped_older_slots: if index == 0 { skipped } else { 0 },
            });
        }
        Ok(runs)
    }

    /// Execute due launch catch-up slots serially. State advances only after
    /// each successful callback; an error leaves the failed slot due.
    pub fn execute_launch_catch_up<F>(
        &mut self,
        now_ms: u64,
        mut run: F,
    ) -> PortabilityResult<CatchUpExecutionReport>
    where
        F: FnMut(&ScheduledSyncRun) -> Result<(), String>,
    {
        let planned = self.plan_launch_catch_up(now_ms)?;
        let mut completed = 0;
        for scheduled in &planned {
            run(scheduled).map_err(PortabilityError::Transport)?;
            self.last_successful_slot_ms = Some(scheduled.scheduled_for_ms);
            completed += 1;
        }
        Ok(CatchUpExecutionReport {
            planned: planned.len(),
            completed,
            last_successful_slot_ms: self.last_successful_slot_ms,
        })
    }
}

fn validate_bundle_identity(
    bundle_id: &str,
    exported_at_ms: u64,
    app_version: &str,
) -> PortabilityResult<()> {
    validate_id(bundle_id, "bundleId")?;
    validate_positive_timestamp(exported_at_ms, "exportedAtMs")?;
    validate_text(app_version, "appVersion", 256)?;
    if app_version.trim().is_empty() {
        return Err(invalid("appVersion", "must not be blank"));
    }
    Ok(())
}

fn validate_import_limits(limits: &ImportLimits) -> PortabilityResult<()> {
    for (name, value) in [
        ("max_archive_bytes", limits.max_archive_bytes),
        (
            "max_entry_compressed_bytes",
            limits.max_entry_compressed_bytes,
        ),
        ("max_entry_expanded_bytes", limits.max_entry_expanded_bytes),
        ("max_total_expanded_bytes", limits.max_total_expanded_bytes),
        ("max_decompression_ratio", limits.max_decompression_ratio),
    ] {
        if value == 0 {
            return Err(invalid(format!("limits.{name}"), "must be positive"));
        }
    }
    for (name, value) in [
        ("max_entries", limits.max_entries),
        ("max_sessions", limits.max_sessions),
        ("max_messages", limits.max_messages),
        ("max_artifacts", limits.max_artifacts),
        ("max_external_references", limits.max_external_references),
    ] {
        if value == 0 {
            return Err(invalid(format!("limits.{name}"), "must be positive"));
        }
    }
    if limits.max_entries > usize::from(u16::MAX) {
        return Err(invalid(
            "limits.maxEntries",
            "must fit the supported ZIP32 entry count",
        ));
    }
    Ok(())
}

fn validate_id(value: &str, path: &str) -> PortabilityResult<()> {
    if value.is_empty() || value.len() > MAX_ID_BYTES || value.chars().any(char::is_control) {
        return Err(invalid(
            path,
            format!("must contain 1..={MAX_ID_BYTES} bytes without controls"),
        ));
    }
    Ok(())
}

fn validate_sha256(value: &str, path: &str) -> PortabilityResult<()> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(invalid(path, "must be a lowercase SHA-256 hex digest"));
    }
    Ok(())
}

fn validate_text(value: &str, path: &str, max_bytes: usize) -> PortabilityResult<()> {
    if value.len() > max_bytes || value.contains('\0') {
        return Err(invalid(
            path,
            format!("exceeds {max_bytes} bytes or contains NUL"),
        ));
    }
    Ok(())
}

fn validate_optional_bounded(
    value: &Option<String>,
    path: &str,
    max_bytes: usize,
) -> PortabilityResult<()> {
    if let Some(value) = value {
        validate_text(value, path, max_bytes)?;
    }
    Ok(())
}

fn validate_positive_timestamp(value: u64, path: &str) -> PortabilityResult<()> {
    if value == 0 || value > i64::MAX as u64 {
        Err(invalid(
            path,
            "must be a positive timestamp within signed 64-bit range",
        ))
    } else {
        Ok(())
    }
}

fn validate_locale(value: &str, path: &str) -> PortabilityResult<()> {
    if value.is_empty()
        || value.len() > MAX_LOCALE_BYTES
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err(invalid(path, "must be a bounded BCP-47-like locale"));
    }
    Ok(())
}

fn validate_role(value: &str, path: &str) -> PortabilityResult<()> {
    if matches!(value, "system" | "user" | "assistant" | "tool" | "notice") {
        Ok(())
    } else {
        Err(invalid(
            path,
            "must be system, user, assistant, tool, or notice",
        ))
    }
}

fn validate_media_type(value: &str, path: &str) -> PortabilityResult<()> {
    if value.is_empty()
        || value.len() > MAX_MEDIA_TYPE_BYTES
        || !value.is_ascii()
        || !value.contains('/')
        || value
            .bytes()
            .any(|byte| byte.is_ascii_control() || byte.is_ascii_whitespace())
    {
        return Err(invalid(path, "contains an invalid media type"));
    }
    Ok(())
}

fn validate_external_reference(value: &str, path: &str) -> PortabilityResult<()> {
    validate_text(value, path, 8_192)?;
    let parsed =
        url::Url::parse(value).map_err(|error| invalid(path, format!("invalid URL: {error}")))?;
    if !matches!(parsed.scheme(), "http" | "https")
        || !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.host_str().is_none()
    {
        return Err(invalid(
            path,
            "must be an absolute http(s) URL without embedded credentials",
        ));
    }
    for (key, _) in parsed.query_pairs() {
        let normalized = key
            .chars()
            .filter(|character| character.is_ascii_alphanumeric())
            .flat_map(char::to_lowercase)
            .collect::<String>();
        if is_secret_key(&key)
            || matches!(
                normalized.as_str(),
                "token" | "key" | "sig" | "signature" | "oauth" | "bearer" | "jwt"
            )
            || normalized.ends_with("credential")
            || normalized.ends_with("signature")
        {
            return Err(invalid(
                path,
                "URL query contains a credential, token, key, or signature field",
            ));
        }
    }
    Ok(())
}

fn validate_retention_policy(policy: &SnapshotRetentionPolicy) -> PortabilityResult<()> {
    if policy.max_count == 0 || policy.max_count > 10_000 {
        return Err(invalid("retention.maxCount", "must be between 1 and 10000"));
    }
    if policy.max_total_bytes == 0 {
        return Err(invalid("retention.maxTotalBytes", "must be positive"));
    }
    if policy.max_age_ms == Some(0) {
        return Err(invalid(
            "retention.maxAgeMs",
            "must be positive when present",
        ));
    }
    Ok(())
}

fn ensure_private_directory(path: &Path) -> PortabilityResult<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_dir() => Ok(()),
        Ok(_) => Err(invalid(
            "snapshot.root",
            "exists but is not a real directory",
        )),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            fs::create_dir_all(path)
                .map_err(|source| io_error("create snapshot directory", path, source))?;
            let metadata = fs::symlink_metadata(path)
                .map_err(|source| io_error("inspect snapshot directory", path, source))?;
            if metadata.file_type().is_dir() {
                Ok(())
            } else {
                Err(invalid(
                    "snapshot.root",
                    "created path is not a real directory",
                ))
            }
        }
        Err(source) => Err(io_error("inspect snapshot directory", path, source)),
    }?;
    #[cfg(unix)]
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
        .map_err(|source| io_error("set private directory permissions", path, source))?;
    Ok(())
}

fn harden_private_file(path: &Path) -> PortabilityResult<()> {
    #[cfg(unix)]
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
        .map_err(|source| io_error("set private snapshot permissions", path, source))?;
    Ok(())
}

fn sync_directory(path: &Path) -> PortabilityResult<()> {
    #[cfg(unix)]
    {
        File::open(path)
            .and_then(|directory| directory.sync_all())
            .map_err(|source| io_error("sync directory", path, source))?;
    }
    Ok(())
}

fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn usize_to_u64(value: usize, name: &'static str) -> PortabilityResult<u64> {
    u64::try_from(value).map_err(|_| invalid(name, "exceeds u64"))
}

fn limit(name: &'static str, observed: usize, max: usize) -> PortabilityError {
    PortabilityError::Limit {
        name,
        observed: u64::try_from(observed).unwrap_or(u64::MAX),
        max: u64::try_from(max).unwrap_or(u64::MAX),
    }
}

fn invalid(path: impl Into<String>, message: impl Into<String>) -> PortabilityError {
    PortabilityError::Invalid {
        path: path.into(),
        message: message.into(),
    }
}

fn corrupt(message: impl Into<String>) -> PortabilityError {
    PortabilityError::CorruptArchive(message.into())
}

fn io_error(operation: &'static str, path: &Path, source: io::Error) -> PortabilityError {
    PortabilityError::Io {
        operation,
        path: path.to_path_buf(),
        source,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::cell::Cell;

    const ARTIFACT_BYTES: &[u8] = b"\0little-monkey-portable-artifact\xff";

    struct TestDirectory {
        path: PathBuf,
    }

    impl TestDirectory {
        fn new(label: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "little-monkey-portability-{label}-{}",
                Uuid::new_v4()
            ));
            fs::create_dir_all(&path).expect("create test directory");
            Self { path }
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    fn sample_input() -> PortableBundleInput {
        let artifact_id = sha256_hex(ARTIFACT_BYTES);
        let blocks = vec![
            PortableContentBlock::Text {
                text: "first <text> & more".to_string(),
            },
            PortableContentBlock::Code {
                language: Some("rust".to_string()),
                code: "let marker = \"```\";\nprintln!(\"{marker}\");".to_string(),
            },
            PortableContentBlock::Table {
                headers: vec!["Name".to_string(), "Value".to_string()],
                rows: vec![vec!["alpha | beta".to_string(), "one\ntwo".to_string()]],
            },
        ];
        let message_id = "message-1".to_string();
        let message = PortableMessage {
            id: message_id.clone(),
            role: "assistant".to_string(),
            ordinal: 0,
            created_at_ms: 1_700_000_000_100,
            blocks: blocks.clone(),
            attachment_ids: vec![artifact_id.clone()],
            external_references: vec!["https://example.com/docs?page=1".to_string()],
            translations: vec![MessageTranslationRecord {
                locale: "sv-SE".to_string(),
                original_blocks: blocks.clone(),
                translated_blocks: vec![PortableContentBlock::Text {
                    text: "forsta texten".to_string(),
                }],
                source_sha256: sha256_hex(&canonical_json(&blocks).expect("canonical blocks")),
                created_at_ms: 1_700_000_000_200,
                metadata: json!({"engine": "local"}),
            }],
            metadata: json!({"provider": "ollama", "temperature": 0.2}),
        };
        let title = "Portable & ordered session".to_string();
        let session = PortableSession {
            id: "session-1".to_string(),
            title: title.clone(),
            ordinal: 0,
            created_at_ms: 1_700_000_000_000,
            updated_at_ms: 1_700_000_000_300,
            archived: false,
            pinned: true,
            model_key: Some("ollama/llama3".to_string()),
            persona_id: Some("persona-reviewer".to_string()),
            workspace_path: None,
            messages: vec![message],
            translations: vec![ThreadTranslationRecord {
                locale: "sv-SE".to_string(),
                original_title: title.clone(),
                translated_title: "Portabel och ordnad session".to_string(),
                source_sha256: sha256_hex(title.as_bytes()),
                translated_message_ids: vec![message_id],
                created_at_ms: 1_700_000_000_200,
                metadata: json!({"engine": "local"}),
            }],
            metadata: json!({"source": "portability-test"}),
        };
        PortableBundleInput {
            bundle_id: "bundle-1".to_string(),
            exported_at_ms: 1_700_000_001_000,
            app_version: "1.0.0-test".to_string(),
            data: PortableDataV1 {
                schema_version: PORTABLE_DATA_SCHEMA_VERSION,
                sessions: vec![session],
                metadata: json!({"product": "Little Monkey"}),
            },
            artifacts: vec![PortableArtifactInput {
                id: artifact_id,
                media_type: "application/octet-stream".to_string(),
                bytes: ARTIFACT_BYTES.to_vec(),
            }],
        }
    }

    fn rebuild_with_data(archive: &[u8], data: &PortableDataV1) -> Vec<u8> {
        let mut entries = read_stored_zip(archive, &ImportLimits::default())
            .expect("parse seed archive")
            .entries;
        let mut manifest: PortableManifestV1 =
            serde_json::from_slice(entries.get(MANIFEST_ENTRY).expect("manifest"))
                .expect("decode manifest");
        let data_json = canonical_json(data).expect("canonical malicious data");
        manifest.data_sha256 = sha256_hex(&data_json);
        manifest.session_count = data.sessions.len() as u64;
        manifest.message_count = data
            .sessions
            .iter()
            .map(|session| session.messages.len() as u64)
            .sum();
        entries.insert(DATA_ENTRY.to_string(), data_json);
        entries.insert(
            MANIFEST_ENTRY.to_string(),
            canonical_json(&manifest).expect("canonical manifest"),
        );
        write_stored_zip(&entries).expect("write rebuilt archive")
    }

    fn replace_all_same_length(bytes: &mut [u8], from: &[u8], to: &[u8]) -> usize {
        assert_eq!(from.len(), to.len());
        let positions = bytes
            .windows(from.len())
            .enumerate()
            .filter_map(|(index, window)| (window == from).then_some(index))
            .collect::<Vec<_>>();
        for position in &positions {
            bytes[*position..*position + to.len()].copy_from_slice(to);
        }
        positions.len()
    }

    #[test]
    fn bundle_export_is_deterministic_and_round_trips_canonically() {
        let input = sample_input();
        let first = export_portable_bundle(&input).expect("first export");
        let second = export_portable_bundle(&input).expect("second export");
        assert_eq!(first, second);

        let (validated, report) =
            preflight_portable_bundle(&first, &ImportLimits::default()).expect("preflight");
        assert_eq!(validated.data, input.data);
        assert_eq!(report.session_count, 1);
        assert_eq!(report.message_count, 1);
        assert_eq!(report.artifact_count, 1);
        assert_eq!(report.external_reference_count, 1);
        assert_eq!(report.archive_sha256, sha256_hex(&first));

        let parsed = read_stored_zip(&first, &ImportLimits::default()).expect("parse export");
        assert_eq!(
            parsed.entries.get(DATA_ENTRY).expect("data entry"),
            &canonical_json(&input.data).expect("canonical data")
        );
        let reconstructed = PortableBundleInput {
            bundle_id: validated.manifest.bundle_id.clone(),
            exported_at_ms: validated.manifest.exported_at_ms,
            app_version: validated.manifest.app_version.clone(),
            data: validated.data.clone(),
            artifacts: validated
                .manifest
                .artifacts
                .iter()
                .map(|descriptor| PortableArtifactInput {
                    id: descriptor.id.clone(),
                    media_type: descriptor.media_type.clone(),
                    bytes: validated
                        .artifacts
                        .get(&descriptor.id)
                        .expect("validated artifact")
                        .clone(),
                })
                .collect(),
        };
        assert_eq!(
            export_portable_bundle(&reconstructed).expect("round-trip export"),
            first
        );
    }

    #[test]
    fn preflight_rejects_duplicate_ids_secrets_and_reference_overflow() {
        let input = sample_input();
        let archive = export_portable_bundle(&input).expect("seed export");

        let mut duplicate_message_data = input.data.clone();
        let mut second_session = duplicate_message_data.sessions[0].clone();
        second_session.id = "session-2".to_string();
        second_session.ordinal = 1;
        second_session.title = "Second".to_string();
        second_session.translations.clear();
        duplicate_message_data.sessions.push(second_session);
        let duplicate_message = rebuild_with_data(&archive, &duplicate_message_data);
        let error = preflight_portable_bundle(&duplicate_message, &ImportLimits::default())
            .expect_err("global duplicate message id must fail");
        assert!(error.to_string().contains("duplicates another message"));

        let mut duplicate_session_data = input.data.clone();
        duplicate_session_data
            .sessions
            .push(duplicate_session_data.sessions[0].clone());
        let duplicate_session = rebuild_with_data(&archive, &duplicate_session_data);
        let error = preflight_portable_bundle(&duplicate_session, &ImportLimits::default())
            .expect_err("duplicate session id must fail");
        assert!(error.to_string().contains("duplicates another session"));

        let mut secret_input = input.clone();
        secret_input.data.metadata = json!({"apiKey": "must-not-export"});
        let error = export_portable_bundle(&secret_input).expect_err("secret export must fail");
        assert!(error.to_string().contains("forbidden"));
        let secret_archive = rebuild_with_data(&archive, &secret_input.data);
        preflight_portable_bundle(&secret_archive, &ImportLimits::default())
            .expect_err("secret import must fail");

        let mut token_url = input.clone();
        token_url.data.sessions[0].messages[0].external_references =
            vec!["https://example.com/file?access_token=secret".to_string()];
        export_portable_bundle(&token_url).expect_err("credential URL must fail");

        let mut two_references = input.clone();
        two_references.data.sessions[0].messages[0]
            .external_references
            .push("https://example.org/another".to_string());
        let reference_archive = export_portable_bundle(&two_references).expect("two refs export");
        let limits = ImportLimits {
            max_external_references: 1,
            ..ImportLimits::default()
        };
        let error = preflight_portable_bundle(&reference_archive, &limits)
            .expect_err("external reference limit must fail");
        assert!(matches!(error, PortabilityError::Limit { .. }));
    }

    #[test]
    fn hostile_zip_names_sizes_ratios_and_methods_are_rejected() {
        let mut one = BTreeMap::new();
        one.insert("aa/evil".to_string(), vec![1, 2, 3, 4, 5]);
        let mut traversal = write_stored_zip(&one).expect("safe seed zip");
        assert_eq!(
            replace_all_same_length(&mut traversal, b"aa/evil", b"../evil"),
            2
        );
        read_stored_zip(&traversal, &ImportLimits::default()).expect_err("zip-slip path must fail");

        let mut two = BTreeMap::new();
        two.insert("a.json".to_string(), vec![1]);
        two.insert("b.json".to_string(), vec![2]);
        let mut duplicate = write_stored_zip(&two).expect("duplicate seed zip");
        assert_eq!(
            replace_all_same_length(&mut duplicate, b"b.json", b"a.json"),
            2
        );
        let error = read_stored_zip(&duplicate, &ImportLimits::default())
            .expect_err("duplicate entry names must fail");
        assert!(error.to_string().contains("duplicate archive entry"));

        let stored = write_stored_zip(&one).expect("size seed zip");
        let compressed_limit = ImportLimits {
            max_entry_compressed_bytes: 4,
            ..ImportLimits::default()
        };
        assert!(matches!(
            read_stored_zip(&stored, &compressed_limit),
            Err(PortabilityError::Limit { .. })
        ));

        let mut ratio = stored.clone();
        let eocd = find_eocd(&ratio).expect("EOCD");
        let central = read_u32(&ratio, eocd + 16).expect("central offset") as usize;
        ratio[central + 24..central + 28].copy_from_slice(&506_u32.to_le_bytes());
        let error = read_stored_zip(&ratio, &ImportLimits::default())
            .expect_err("decompression ratio must fail before expansion");
        assert!(matches!(
            error,
            PortabilityError::Limit {
                name: "entry decompression ratio",
                ..
            }
        ));

        let mut compressed_method = stored;
        let eocd = find_eocd(&compressed_method).expect("EOCD");
        let central = read_u32(&compressed_method, eocd + 16).expect("central offset") as usize;
        compressed_method[central + 10..central + 12].copy_from_slice(&8_u16.to_le_bytes());
        assert!(matches!(
            read_stored_zip(&compressed_method, &ImportLimits::default()),
            Err(PortabilityError::UnsupportedCompression(8))
        ));
        assert_eq!(crc32(b"123456789"), 0xcbf4_3926);
    }

    #[test]
    fn markdown_and_docx_preserve_text_code_table_order() {
        let session = &sample_input().data.sessions[0];
        let markdown = export_session_markdown(session).expect("markdown export");
        let text = markdown.find("first <text>").expect("markdown text");
        let code = markdown.find("let marker").expect("markdown code");
        let table = markdown.find("| Name | Value |").expect("markdown table");
        assert!(text < code && code < table);
        assert!(markdown.contains("````rust"));
        assert!(markdown.contains("alpha \\| beta"));
        assert!(markdown.contains("one<br>two"));

        let first = export_session_docx(session).expect("DOCX export");
        let second = export_session_docx(session).expect("DOCX repeat export");
        assert_eq!(first, second);
        let parsed = read_stored_zip(&first, &ImportLimits::default()).expect("parse DOCX ZIP");
        for required in [
            "[Content_Types].xml",
            "_rels/.rels",
            "word/document.xml",
            "word/_rels/document.xml.rels",
            "word/styles.xml",
        ] {
            assert!(parsed.entries.contains_key(required), "missing {required}");
        }
        let document = std::str::from_utf8(
            parsed
                .entries
                .get("word/document.xml")
                .expect("document XML"),
        )
        .expect("UTF-8 document XML");
        let text = document.find("first &lt;text&gt;").expect("DOCX text");
        let code = document.find("let marker").expect("DOCX code");
        let table = document.find("<w:tbl>").expect("DOCX table");
        assert!(text < code && code < table);
        assert!(document.contains("Portable &amp; ordered session"));
    }

    #[derive(Default)]
    struct RecordingImportTarget {
        calls: usize,
    }

    impl PortableImportTarget for RecordingImportTarget {
        fn commit_validated_bundle(
            &mut self,
            bundle: &ValidatedPortableBundle,
            published_artifacts: &[PublishedPortableArtifact],
        ) -> Result<String, String> {
            self.calls += 1;
            assert_eq!(bundle.manifest.bundle_id, "bundle-1");
            assert_eq!(published_artifacts.len(), 1);
            Ok("commit-1".to_string())
        }
    }

    #[test]
    fn corrupt_import_is_atomic_and_valid_import_commits_once() {
        let input = sample_input();
        let archive = export_portable_bundle(&input).expect("export");
        let mut corrupt_archive = archive.clone();
        let payload = corrupt_archive
            .windows(ARTIFACT_BYTES.len())
            .position(|window| window == ARTIFACT_BYTES)
            .expect("artifact payload offset");
        corrupt_archive[payload + 1] ^= 0x40;

        let directory = TestDirectory::new("atomic-import");
        let store = ArtifactStore::new(directory.path.join("artifacts")).expect("artifact store");
        let artifact_path = store
            .blob_path(&input.artifacts[0].id)
            .expect("artifact path");
        let mut target = RecordingImportTarget::default();
        import_portable_bundle(
            &corrupt_archive,
            &ImportLimits::default(),
            &store,
            &mut target,
        )
        .expect_err("corrupt archive must be rejected");
        assert_eq!(target.calls, 0);
        assert!(!artifact_path.exists());

        let outcome =
            import_portable_bundle(&archive, &ImportLimits::default(), &store, &mut target)
                .expect("valid atomic import");
        assert_eq!(outcome.commit_id, "commit-1");
        assert_eq!(target.calls, 1);
        assert_eq!(
            store.read(&input.artifacts[0].id).expect("stored blob"),
            ARTIFACT_BYTES
        );
    }

    /// Test-only deterministic authenticated transform. Production callers
    /// must inject an audited AEAD through `CryptoProvider`.
    #[derive(Default)]
    struct TestCrypto {
        opens: Cell<usize>,
    }

    impl TestCrypto {
        fn tag(aad: &[u8], nonce: &[u8], ciphertext: &[u8]) -> Vec<u8> {
            let mut digest = Sha256::new();
            digest.update((aad.len() as u64).to_le_bytes());
            digest.update(aad);
            digest.update((nonce.len() as u64).to_le_bytes());
            digest.update(nonce);
            digest.update(ciphertext);
            digest.finalize()[..16].to_vec()
        }
    }

    impl CryptoProvider for TestCrypto {
        fn algorithm_id(&self) -> &str {
            "test-only-authenticated-transform"
        }

        fn key_id(&self) -> &str {
            "test-key-1"
        }

        fn seal(&self, aad: &[u8], plaintext: &[u8]) -> Result<CryptoSeal, String> {
            let nonce = vec![0x5a; 12];
            let ciphertext = plaintext.iter().map(|byte| byte ^ 0xa5).collect::<Vec<_>>();
            let tag = Self::tag(aad, &nonce, &ciphertext);
            Ok(CryptoSeal {
                nonce,
                ciphertext,
                tag,
            })
        }

        fn open(
            &self,
            aad: &[u8],
            nonce: &[u8],
            ciphertext: &[u8],
            tag: &[u8],
        ) -> Result<Vec<u8>, String> {
            self.opens.set(self.opens.get() + 1);
            if tag != Self::tag(aad, nonce, ciphertext) {
                return Err("authentication failed".to_string());
            }
            Ok(ciphertext.iter().map(|byte| byte ^ 0xa5).collect())
        }
    }

    fn sealed_sample(created_at_ms: u64) -> (Vec<u8>, String, Vec<u8>, TestCrypto) {
        let bundle = export_portable_bundle(&sample_input()).expect("sample export");
        let revision = sha256_hex(b"profile-revision-one");
        let crypto = TestCrypto::default();
        let envelope = seal_encrypted_snapshot(
            &bundle,
            &revision,
            created_at_ms,
            &ImportLimits::default(),
            &crypto,
        )
        .expect("seal snapshot");
        (envelope, revision, bundle, crypto)
    }

    #[test]
    fn encrypted_snapshot_binds_header_ciphertext_and_bundle() {
        let (envelope_json, revision, bundle, crypto) = sealed_sample(5_000);
        let opened = open_encrypted_snapshot(&envelope_json, &ImportLimits::default(), &crypto)
            .expect("open snapshot");
        assert_eq!(opened.envelope.source_revision_sha256, revision);
        assert_eq!(opened.bundle.archive_sha256, sha256_hex(&bundle));
        assert_eq!(crypto.opens.get(), 1);

        let mut header_tamper: EncryptedSnapshotEnvelopeV1 =
            serde_json::from_slice(&envelope_json).expect("decode envelope");
        header_tamper.created_at_ms += 1;
        let header_tamper = canonical_json(&header_tamper).expect("canonical tamper");
        assert!(matches!(
            open_encrypted_snapshot(&header_tamper, &ImportLimits::default(), &crypto),
            Err(PortabilityError::Crypto(_))
        ));

        let mut ciphertext_tamper: EncryptedSnapshotEnvelopeV1 =
            serde_json::from_slice(&envelope_json).expect("decode envelope");
        let mut ciphertext = base64::engine::general_purpose::STANDARD
            .decode(&ciphertext_tamper.ciphertext_base64)
            .expect("decode ciphertext");
        ciphertext[0] ^= 1;
        ciphertext_tamper.ciphertext_base64 =
            base64::engine::general_purpose::STANDARD.encode(ciphertext);
        let ciphertext_tamper = canonical_json(&ciphertext_tamper).expect("canonical tamper");
        assert!(matches!(
            open_encrypted_snapshot(&ciphertext_tamper, &ImportLimits::default(), &crypto),
            Err(PortabilityError::Crypto(_))
        ));
    }

    #[test]
    fn snapshot_retention_is_atomic_private_and_idempotent() {
        let directory = TestDirectory::new("retention");
        let snapshot_root = directory.path.join("snapshots");
        let policy = SnapshotRetentionPolicy {
            max_count: 2,
            max_total_bytes: 32 * 1024 * 1024,
            max_age_ms: None,
        };
        let (first, _, _, _) = sealed_sample(1_000);
        let (second, _, _, _) = sealed_sample(2_000);
        let (third, _, _, _) = sealed_sample(3_000);
        write_snapshot_with_retention(&snapshot_root, &first, &policy, 1_000).expect("write first");
        write_snapshot_with_retention(&snapshot_root, &second, &policy, 2_000)
            .expect("write second");
        let outcome = write_snapshot_with_retention(&snapshot_root, &third, &policy, 3_000)
            .expect("write third");
        assert_eq!(outcome.pruned.len(), 1);
        let snapshots = list_snapshot_files(&snapshot_root).expect("list snapshots");
        assert_eq!(
            snapshots
                .iter()
                .map(|snapshot| snapshot.created_at_ms)
                .collect::<Vec<_>>(),
            vec![3_000, 2_000]
        );
        let repeated = write_snapshot_with_retention(&snapshot_root, &third, &policy, 3_000)
            .expect("idempotent write");
        assert!(repeated.already_existed);

        #[cfg(unix)]
        {
            let directory_mode = fs::metadata(&snapshot_root)
                .expect("snapshot directory metadata")
                .permissions()
                .mode()
                & 0o777;
            let file_mode = fs::metadata(&repeated.snapshot.path)
                .expect("snapshot metadata")
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(directory_mode, 0o700);
            assert_eq!(file_mode, 0o600);
        }

        fs::write(&repeated.snapshot.path, b"different bytes").expect("inject collision");
        assert!(matches!(
            write_snapshot_with_retention(&snapshot_root, &third, &policy, 3_000),
            Err(PortabilityError::Conflict(_))
        ));
    }

    struct RecordingRestoreTarget {
        revision: String,
        replace_calls: usize,
        conflict_calls: usize,
    }

    impl SnapshotRestoreTarget for RecordingRestoreTarget {
        fn current_revision_sha256(&self) -> Result<String, String> {
            Ok(self.revision.clone())
        }

        fn commit_replace(
            &mut self,
            _bundle: &ValidatedPortableBundle,
            expected_current_revision: &str,
        ) -> Result<String, String> {
            assert_eq!(expected_current_revision, self.revision);
            self.replace_calls += 1;
            Ok("replace-commit".to_string())
        }

        fn commit_conflict_copy(
            &mut self,
            _bundle: &ValidatedPortableBundle,
            conflict_id: &str,
            observed_current_revision: &str,
        ) -> Result<String, String> {
            assert!(conflict_id.starts_with("restore-conflict-"));
            assert_eq!(observed_current_revision, self.revision);
            self.conflict_calls += 1;
            Ok("conflict-copy-commit".to_string())
        }
    }

    #[test]
    fn restore_uses_atomic_revision_check_and_conflict_copy_policy() {
        let (envelope, expected, _, crypto) = sealed_sample(7_000);
        let opened = open_encrypted_snapshot(&envelope, &ImportLimits::default(), &crypto)
            .expect("open snapshot");
        let mut matching = RecordingRestoreTarget {
            revision: expected,
            replace_calls: 0,
            conflict_calls: 0,
        };
        assert!(matches!(
            restore_opened_snapshot(&opened, RestoreConflictPolicy::Abort, &mut matching),
            Ok(SnapshotRestoreOutcome::Replaced { .. })
        ));
        assert_eq!((matching.replace_calls, matching.conflict_calls), (1, 0));

        let current = sha256_hex(b"newer-current-revision");
        let mut abort = RecordingRestoreTarget {
            revision: current.clone(),
            replace_calls: 0,
            conflict_calls: 0,
        };
        assert!(matches!(
            restore_opened_snapshot(&opened, RestoreConflictPolicy::Abort, &mut abort),
            Ok(SnapshotRestoreOutcome::Conflict { .. })
        ));
        assert_eq!((abort.replace_calls, abort.conflict_calls), (0, 0));

        let mut keep_both = RecordingRestoreTarget {
            revision: current,
            replace_calls: 0,
            conflict_calls: 0,
        };
        assert!(matches!(
            restore_opened_snapshot(&opened, RestoreConflictPolicy::KeepBoth, &mut keep_both),
            Ok(SnapshotRestoreOutcome::ConflictCopy { .. })
        ));
        assert_eq!((keep_both.replace_calls, keep_both.conflict_calls), (0, 1));
    }

    #[derive(Default)]
    struct MemoryWebDav {
        objects: BTreeMap<String, (String, Vec<u8>)>,
        etag_counter: u64,
    }

    impl WebDavTransport for MemoryWebDav {
        fn put(
            &mut self,
            remote_path: &str,
            bytes: &[u8],
            condition: WebDavPutCondition,
        ) -> Result<WebDavPutResult, String> {
            let current = self.objects.get(remote_path).map(|(etag, _)| etag.clone());
            let accepted = match &condition {
                WebDavPutCondition::IfNoneMatch => current.is_none(),
                WebDavPutCondition::IfMatch(expected) => current.as_ref() == Some(expected),
            };
            if !accepted {
                return Ok(WebDavPutResult::PreconditionFailed {
                    current_etag: current,
                });
            }
            self.etag_counter += 1;
            let etag = format!("etag-{}", self.etag_counter);
            self.objects
                .insert(remote_path.to_string(), (etag.clone(), bytes.to_vec()));
            Ok(WebDavPutResult::Stored { etag })
        }

        fn get(
            &mut self,
            remote_path: &str,
            if_none_match: Option<&str>,
        ) -> Result<WebDavGetResult, String> {
            let Some((etag, bytes)) = self.objects.get(remote_path) else {
                return Ok(WebDavGetResult::Missing);
            };
            if if_none_match == Some(etag.as_str()) {
                return Ok(WebDavGetResult::NotModified);
            }
            Ok(WebDavGetResult::Found {
                etag: etag.clone(),
                bytes: bytes.clone(),
            })
        }
    }

    #[test]
    fn webdav_etags_download_validation_and_conflict_copies_work() {
        let (snapshot, _, _, crypto) = sealed_sample(8_000);
        let path = "folder.with.dot/snapshot.lmsnapshot";
        let mut transport = MemoryWebDav::default();
        let uploaded =
            upload_snapshot_webdav(&mut transport, path, &snapshot, None, "desktop-one", 8_000)
                .expect("first upload");
        let WebDavUploadOutcome::Uploaded { etag, .. } = uploaded else {
            panic!("expected uploaded outcome");
        };

        assert!(matches!(
            download_snapshot_webdav(
                &mut transport,
                path,
                None,
                &ImportLimits::default(),
                &crypto
            ),
            Ok(WebDavDownloadOutcome::Downloaded { .. })
        ));
        assert!(matches!(
            download_snapshot_webdav(
                &mut transport,
                path,
                Some(&etag),
                &ImportLimits::default(),
                &crypto
            ),
            Ok(WebDavDownloadOutcome::NotModified)
        ));
        assert!(matches!(
            download_snapshot_webdav(
                &mut transport,
                "missing/snapshot.lmsnapshot",
                None,
                &ImportLimits::default(),
                &crypto
            ),
            Ok(WebDavDownloadOutcome::Missing)
        ));

        transport.objects.insert(
            path.to_string(),
            ("external-etag".to_string(), b"remote changed".to_vec()),
        );
        let conflict = upload_snapshot_webdav(
            &mut transport,
            path,
            &snapshot,
            Some(&etag),
            "desktop-one",
            8_100,
        )
        .expect("conflict-copy upload");
        let WebDavUploadOutcome::ConflictCopy {
            remote_path,
            conflicting_etag,
            ..
        } = conflict
        else {
            panic!("expected conflict-copy outcome");
        };
        assert!(remote_path.starts_with("folder.with.dot/snapshot.conflict-"));
        assert!(remote_path.ends_with(".lmsnapshot"));
        assert_eq!(conflicting_etag.as_deref(), Some("external-etag"));
        assert_eq!(
            transport.objects.get(path).expect("original remote").1,
            b"remote changed"
        );
        assert_eq!(
            transport
                .objects
                .get(&remote_path)
                .expect("conflict remote")
                .1,
            snapshot
        );
    }

    #[test]
    fn launch_catch_up_is_bounded_aligned_and_resumes_after_failure() {
        let mut scheduler = LaunchCatchUpScheduler {
            anchor_ms: 100,
            interval_ms: 10,
            max_catch_up_runs: 3,
            last_successful_slot_ms: None,
        };
        let plan = scheduler.plan_launch_catch_up(160).expect("catch-up plan");
        assert_eq!(
            plan.iter()
                .map(|run| (run.scheduled_for_ms, run.skipped_older_slots))
                .collect::<Vec<_>>(),
            vec![(140, 4), (150, 0), (160, 0)]
        );
        let error = scheduler
            .execute_launch_catch_up(160, |run| {
                if run.scheduled_for_ms == 150 {
                    Err("simulated network failure".to_string())
                } else {
                    Ok(())
                }
            })
            .expect_err("middle run should fail");
        assert!(matches!(error, PortabilityError::Transport(_)));
        assert_eq!(scheduler.last_successful_slot_ms, Some(140));

        let report = scheduler
            .execute_launch_catch_up(160, |_| Ok(()))
            .expect("resume catch-up");
        assert_eq!((report.planned, report.completed), (2, 2));
        assert_eq!(scheduler.last_successful_slot_ms, Some(160));

        let misaligned = LaunchCatchUpScheduler {
            anchor_ms: 100,
            interval_ms: 10,
            max_catch_up_runs: 3,
            last_successful_slot_ms: Some(105),
        };
        misaligned
            .validate()
            .expect_err("misaligned state must fail");
    }
}
