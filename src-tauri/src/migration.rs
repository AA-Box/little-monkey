//! Moving a frozen process image to another owned node (roadmap K18).
//!
//! K13 already freezes a live process into a durable image at a tool boundary
//! and re-enters from it. Everything here is what that image needs in order to
//! cross a machine boundary, and nothing more:
//!
//! - **It copies where K13 references.** [`crate::checkpoints::ResumeState`]
//!   deliberately holds identifiers rather than contents, because on one machine
//!   a second copy could only disagree with the first. Across two machines there
//!   is no first copy to disagree with — the target has none of the workspace,
//!   none of the checkpoint's backups, and no row for the run — so the image has
//!   to carry bytes. That inversion is the whole difference between K13's image
//!   and this one, and it is why this is a separate type rather than a field.
//! - **The refusal is K13's, reused rather than restated.** A target node asking
//!   "can I resume this?" is asking exactly what [`crate::checkpoints::restorability`]
//!   answers, so [`admit`] calls it and adds only the blockers that are specific
//!   to a *move*: an unsupported protocol, a run this node already has, a
//!   payload that does not match its digest, a residency the move required,
//!   and a size the node will not accept.
//! - **The header is separable from the payload on purpose.** [`MigrationHeader`]
//!   is metadata only, so a target can refuse before a byte of workspace crosses
//!   the wire, and the *same* [`admit`] runs again on arrival against the same
//!   header — the preflight is an optimisation, never the authority.
//!
//! The chain that spans both nodes lives in [`crate::run_ledger`]
//! (`MigrationDeparture` / `MigrationArrival` / `join_migration_chain`), because
//! it is a fact about run events rather than about the image.

use std::collections::BTreeSet;
use std::path::{Component, Path, PathBuf};

use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::checkpoints::{
    restorability, CheckpointManifest, Restorability, RestoreBlocker, RestoreEnvironment,
    DETERMINISM_CAVEATS,
};
use crate::run_protocol::RunSpec;

pub const MIGRATION_PROTOCOL_VERSION: u32 = 1;

/// Per-file ceiling. A frozen process's workspace is source and notes, not
/// build output; a single file past this is a sign the wrong directory was
/// captured, which is worth refusing loudly rather than transferring quietly.
pub const MAX_MIGRATION_FILE_BYTES: u64 = 8 * 1024 * 1024;

/// Total decoded payload ceiling. Sized to stay under the remote transport's
/// own `MAX_REMOTE_BODY_BYTES` once base64 has inflated it by a third.
pub const MAX_MIGRATION_PAYLOAD_BYTES: u64 = 24 * 1024 * 1024;

pub const MAX_MIGRATION_FILES: usize = 4_096;

/// What a move does not carry, on top of what a resume does not reproduce.
///
/// Shipped beside the verdict for [`DETERMINISM_CAVEATS`]'s reason: the reader
/// who needs this is whoever is deciding to press Migrate. Every entry is a
/// thing that is **not** preserved, and there is deliberately no balancing
/// "preserved" list — the conversation, the workspace files and the policy are
/// preserved *because [`admit`] refuses when they cannot be*.
pub const MIGRATION_CAVEATS: &[&str] = &[
    "The machine changed. CPU, accelerator, memory and OS on the target are whatever they are; a process that fit on the origin is not thereby admitted here, which is what the target's own admission control decides.",
    "Absolute paths moved. The workspace is rebuilt under a path this node chose, so anything that recorded the origin's absolute path — in the conversation, in a config file, in a shell history — still names a directory that does not exist here.",
    "Nothing outside the recorded workspace travelled. Files elsewhere on the origin's disk, its running processes, its local services and its loopback endpoints stayed there.",
    "Credentials did not travel and were never in the image. A provider key, an OAuth token or an MCP server's stored secret is the origin's; the target uses its own or the run's next call to that service fails.",
    "The origin still holds its copy. A migration is a handover, not a delete: the workspace and checkpoint that were transferred are still on the origin, and taking them back means moving again, not undoing.",
];

/// Everything about a move that is not bytes.
///
/// Sent alone by the preflight so a target can refuse before the transfer, and
/// then sent again inside [`MigrationImage`] so the admission that matters runs
/// against the same facts. It carries the K13 manifest whole rather than a
/// digest of it, because [`admit`] needs to run K13's own restorability check
/// and that takes a manifest.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MigrationHeader {
    pub protocol_version: u32,
    /// The origin runner's id — the same `runner_id` the pairing pinned.
    pub origin_node_id: String,
    pub run_id: String,
    pub process_id: String,
    pub checkpoint_id: String,
    /// The K13 freeze image. Carries `resume`, which is what makes this a
    /// migration of a *process* rather than a copy of a directory.
    pub manifest: CheckpointManifest,
    /// The data-residency the origin *required of the target*, if any.
    ///
    /// K17's rule, unchanged: the placer states the rule it applied and the node
    /// checks it against its own label rather than trusting it. `None` means the
    /// origin stated no rule, which accepts any node — deliberately not the same
    /// as `Some("unspecified")`, which accepts only a node whose operator left
    /// the label unset.
    pub required_residency: Option<String>,
    /// Decoded size of the payload this header describes.
    pub payload_bytes: u64,
    /// SHA-256 over the payload's canonical JSON, so the target can tell that
    /// the bytes it received are the bytes it agreed to accept.
    pub payload_sha256: String,
}

/// One file, with its digest, relative to the root it was read from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MigrationFile {
    /// Always relative and always forward-slashed. Absolute paths, `..`, and
    /// Windows prefixes are rejected on both write *and* read: an image is
    /// attacker-controlled input on the target, and a path that escapes the
    /// destination root is the one bug that turns a migration into a write
    /// anywhere on the receiving machine.
    pub path: String,
    pub sha256: String,
    pub contents_base64: String,
}

impl MigrationFile {
    /// Reads one file relative to `root`, refusing anything oversized.
    pub fn read(root: &Path, absolute: &Path) -> Result<Self, String> {
        let relative = absolute
            .strip_prefix(root)
            .map_err(|_| format!("'{}' is outside the migrated root", absolute.display()))?;
        let bytes = std::fs::read(absolute)
            .map_err(|error| format!("Could not read '{}': {error}", absolute.display()))?;
        if bytes.len() as u64 > MAX_MIGRATION_FILE_BYTES {
            return Err(format!(
                "'{}' is {} bytes, over the {MAX_MIGRATION_FILE_BYTES}-byte per-file migration limit",
                absolute.display(),
                bytes.len()
            ));
        }
        Ok(Self {
            path: portable_relative(relative)?,
            sha256: sha256_hex(&bytes),
            contents_base64: STANDARD.encode(&bytes),
        })
    }

    pub fn decode(&self) -> Result<Vec<u8>, String> {
        validate_relative(&self.path)?;
        let bytes = STANDARD
            .decode(self.contents_base64.as_bytes())
            .map_err(|error| format!("'{}' is not valid base64: {error}", self.path))?;
        if sha256_hex(&bytes) != self.sha256 {
            return Err(format!(
                "'{}' does not match its recorded digest",
                self.path
            ));
        }
        Ok(bytes)
    }

    /// Materialises this file under `root`, creating parents.
    pub fn write(&self, root: &Path) -> Result<PathBuf, String> {
        let bytes = self.decode()?;
        let destination = root.join(&self.path);
        if let Some(parent) = destination.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|error| format!("Could not create '{}': {error}", parent.display()))?;
        }
        std::fs::write(&destination, &bytes)
            .map_err(|error| format!("Could not write '{}': {error}", destination.display()))?;
        Ok(destination)
    }
}

/// The bytes half of an image, hashed as one unit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MigrationPayload {
    /// The checkpoint directory's own files — the `before/` backups, `redo/`
    /// and `after/` snapshots. Without them the moved checkpoint could not be
    /// reverted on the target, which would silently make a resumed turn
    /// un-undoable.
    pub checkpoint_files: Vec<MigrationFile>,
    /// The K10 workspace tree the process was working in.
    pub workspace_files: Vec<MigrationFile>,
    /// The one session object out of the origin's `chat_sessions.json` that
    /// this process's turn belongs to.
    ///
    /// K13's image references the conversation because the profile store on
    /// that machine already holds it. On the target it holds nothing, and a
    /// resume with no conversation would continue a turn with no history — so
    /// this is the second thing the move has to copy where K13 could point.
    /// Carried opaquely, as the frontend's own shape: this crate does not own
    /// the session schema and re-declaring it here would create a second
    /// definition to keep in step.
    pub session: Option<serde_json::Value>,
}

impl MigrationPayload {
    /// The session's id, when the payload carries one.
    pub fn session_id(&self) -> Option<&str> {
        self.session.as_ref()?.get("id")?.as_str()
    }

    pub fn decoded_bytes(&self) -> u64 {
        self.files()
            .map(|file| decoded_len(&file.contents_base64))
            .sum()
    }

    pub fn files(&self) -> impl Iterator<Item = &MigrationFile> {
        self.checkpoint_files
            .iter()
            .chain(self.workspace_files.iter())
    }

    /// SHA-256 over the canonical JSON of the payload.
    ///
    /// Over the serialized form rather than a hand-rolled field walk, so adding
    /// a file list later cannot silently fall outside the digest — `serde_json`
    /// emits struct fields in declaration order, which makes this stable for a
    /// given schema without needing a canonicalisation pass.
    pub fn digest(&self) -> Result<String, String> {
        let bytes = serde_json::to_vec(self)
            .map_err(|error| format!("Could not canonicalise the migration payload: {error}"))?;
        Ok(sha256_hex(&bytes))
    }

    pub fn validate(&self) -> Result<(), String> {
        let count = self.checkpoint_files.len() + self.workspace_files.len();
        if count > MAX_MIGRATION_FILES {
            return Err(format!(
                "A migration image may carry at most {MAX_MIGRATION_FILES} files, not {count}"
            ));
        }
        let mut seen = BTreeSet::new();
        for file in self.files() {
            validate_relative(&file.path)?;
            if decoded_len(&file.contents_base64) > MAX_MIGRATION_FILE_BYTES {
                return Err(format!(
                    "'{}' is over the {MAX_MIGRATION_FILE_BYTES}-byte per-file migration limit",
                    file.path
                ));
            }
            if file.sha256.len() != 64 || !file.sha256.bytes().all(|b| b.is_ascii_hexdigit()) {
                return Err(format!("'{}' has an invalid digest", file.path));
            }
            if !seen.insert(file.path.as_str()) {
                return Err(format!("'{}' appears twice in the image", file.path));
            }
        }
        let bytes = self.decoded_bytes();
        if bytes > MAX_MIGRATION_PAYLOAD_BYTES {
            return Err(format!(
                "The migration payload is {bytes} bytes, over the {MAX_MIGRATION_PAYLOAD_BYTES}-byte limit"
            ));
        }
        Ok(())
    }
}

/// A complete, self-contained frozen process ready to cross the wire.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MigrationImage {
    pub header: MigrationHeader,
    /// The run's frozen spec, carrying its `PermissionPolicySnapshot` and
    /// `RunBudgets`. This is the policy travelling with the work: the target
    /// inserts *this* into its own ledger, so the allowlist and budgets it
    /// enforces are the ones the origin declared, not defaults it chose.
    pub spec: RunSpec,
    /// The origin's absolute workspace path, recorded so an operator can see
    /// what the paths in the conversation used to mean. Never used to place
    /// anything on the target.
    pub origin_workspace_root: Option<String>,
    /// The origin chain tip this move hangs off — the departure event's hash.
    /// The target repeats it in its arrival event, and that repetition is the
    /// single chain across both machines.
    pub origin_last_sequence: u64,
    pub origin_last_event_hash: String,
    pub payload: MigrationPayload,
}

impl MigrationImage {
    /// Structural checks that do not depend on the receiving node.
    ///
    /// Separate from [`admit`], which answers "can *this* node run it": a
    /// malformed image is a bad request on any node, and conflating the two
    /// would report a broken wire message as a capability refusal.
    pub fn validate(&self) -> Result<(), String> {
        if self.header.protocol_version != MIGRATION_PROTOCOL_VERSION {
            return Err(format!(
                "Unsupported migration protocol version {}",
                self.header.protocol_version
            ));
        }
        if self.spec.run_id != self.header.run_id {
            return Err("The image's spec and header name different runs".to_string());
        }
        self.spec
            .validate()
            .map_err(|error| format!("The migrated run spec is invalid: {error}"))?;
        self.payload.validate()?;
        let digest = self.payload.digest()?;
        if digest != self.header.payload_sha256 {
            return Err(
                "The migration payload does not match the digest its header declares".to_string(),
            );
        }
        if self.payload.decoded_bytes() != self.header.payload_bytes {
            return Err("The migration payload is not the size its header declares".to_string());
        }
        if self.origin_last_sequence == 0 || self.origin_last_event_hash.len() != 64 {
            return Err("The image does not name a usable origin chain tip".to_string());
        }
        match self.header.manifest.resume.as_ref() {
            Some(resume) if resume.process_id == self.header.process_id => Ok(()),
            Some(_) => Err(
                "The image's manifest freezes a different process than its header names"
                    .to_string(),
            ),
            // Left to `admit` as `NotAFreeze` rather than raised here: "this is
            // an ordinary turn checkpoint" is a refusal a user can act on, and
            // K13 already words it.
            None => Ok(()),
        }
    }
}

/// Why a target node will not take an image.
///
/// A closed set with stable codes, for [`RestoreBlocker`]'s reason. The first
/// three are K13's own, re-exported through here so a caller reading a
/// migration refusal never has to know which half produced it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum MigrationBlocker {
    NotAFreeze,
    ModelNotResident,
    ApprovalExpired,
    ProtocolUnsupported,
    /// The runtime the process was running under is not installed here. K13 has
    /// no equivalent because on one machine the runtime that froze the process
    /// is by construction still the local one.
    RuntimeMissing,
    /// This node already has a run with that id, so admitting would fork one
    /// run's history across two chains that both claim to be it.
    RunAlreadyPresent,
    /// The image is larger than this node accepts.
    ImageTooLarge,
    /// This node's residency label is not the one the origin required.
    DataResidencyRefused,
}

impl MigrationBlocker {
    #[must_use]
    pub fn code(self) -> &'static str {
        match self {
            Self::NotAFreeze => "not-a-freeze",
            Self::ModelNotResident => "model-not-resident",
            Self::ApprovalExpired => "approval-expired",
            Self::ProtocolUnsupported => "protocol-unsupported",
            Self::RuntimeMissing => "runtime-missing",
            Self::RunAlreadyPresent => "run-already-present",
            Self::ImageTooLarge => "image-too-large",
            Self::DataResidencyRefused => "data-residency-refused",
        }
    }

    #[must_use]
    pub fn explanation(self) -> &'static str {
        match self {
            Self::NotAFreeze | Self::ModelNotResident | Self::ApprovalExpired => {
                self.restore_blocker()
                    .expect("the three shared codes always map back")
                    .explanation()
            }
            Self::ProtocolUnsupported => {
                "This image was written by a migration protocol this node does not implement. Accepting it would mean guessing at fields it cannot read, so it is refused instead."
            }
            Self::RuntimeMissing => {
                "The runtime this process was running under is not installed on this node. Resuming under a different one would change how the model is executed, so the move is refused instead."
            }
            Self::RunAlreadyPresent => {
                "This node already holds a run with that id. Admitting the image would fork one run's history into two chains that both claim to be it, so the move is refused instead."
            }
            Self::ImageTooLarge => {
                "The image is larger than this node accepts. Transferring it would be refused part-way, so it is refused before anything moves."
            }
            Self::DataResidencyRefused => {
                "This node's data residency is not the one the move required. The move is refused rather than quietly relocating the run's data into another jurisdiction."
            }
        }
    }

    /// The K13 blocker this code came from, when it is one of the shared three.
    #[must_use]
    pub fn restore_blocker(self) -> Option<RestoreBlocker> {
        match self {
            Self::NotAFreeze => Some(RestoreBlocker::NotAFreeze),
            Self::ModelNotResident => Some(RestoreBlocker::ModelNotResident),
            Self::ApprovalExpired => Some(RestoreBlocker::ApprovalExpired),
            _ => None,
        }
    }

    fn from_restore(blocker: RestoreBlocker) -> Option<Self> {
        match blocker {
            RestoreBlocker::NotAFreeze => Some(Self::NotAFreeze),
            RestoreBlocker::ModelNotResident => Some(Self::ModelNotResident),
            RestoreBlocker::ApprovalExpired => Some(Self::ApprovalExpired),
            // Impossible here and dropped rather than translated: the image
            // *carries* the workspace, so a target that has decoded a header
            // has not yet failed to find one. Reporting it would be reporting a
            // check that has not run.
            RestoreBlocker::WorkspaceGone => None,
        }
    }
}

/// What this node can currently offer an incoming image.
///
/// Supplied by the caller for [`RestoreEnvironment`]'s reason: none of it is
/// knowable from the image, and gathering it here would make the decision
/// untestable without a live host.
#[derive(Debug, Clone, Default)]
pub struct TargetNode<'a> {
    pub node_id: &'a str,
    pub resident_models: &'a [String],
    pub runtime_ids: &'a [String],
    /// `request_id`s this node currently grants. Empty is the normal case, and
    /// correctly refuses any image frozen with an outstanding approval — this
    /// node grants none of the origin's permissions.
    pub live_approvals: &'a [String],
    /// This node's own data-residency label, as its operator set it. Compared
    /// against the image's `required_residency`; nothing infers it.
    pub residency: &'a str,
    pub max_payload_bytes: u64,
    /// Whether this node already holds the incoming run id.
    pub run_present: bool,
}

/// Whether this node will take the image, or every reason it will not.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase", tag = "state")]
pub enum MigrationVerdict {
    Acceptable {
        run_id: String,
        process_id: String,
        /// Repeated in the verdict so a caller that shows Accept is holding the
        /// same statement the operator will be asked to trust.
        caveats: Vec<String>,
    },
    Refused {
        blockers: Vec<MigrationBlocker>,
    },
}

impl MigrationVerdict {
    #[must_use]
    pub fn is_acceptable(&self) -> bool {
        matches!(self, Self::Acceptable { .. })
    }
}

/// Every reason this node cannot take the image, or the process it will resume.
///
/// Collects **all** blockers rather than returning the first, for
/// [`restorability`]'s reason: an operator told the model is missing, who
/// installs it and is then told the runtime is too, has been made to discover
/// the refusals one at a time — across two machines, where each round trip is a
/// transfer.
#[must_use]
pub fn admit(header: &MigrationHeader, target: &TargetNode<'_>) -> MigrationVerdict {
    let mut blockers = Vec::new();
    if header.protocol_version != MIGRATION_PROTOCOL_VERSION {
        // Returned alone. Every other check reads fields whose meaning this
        // version defines, so reporting them beside a version mismatch would be
        // reporting guesses.
        return MigrationVerdict::Refused {
            blockers: vec![MigrationBlocker::ProtocolUnsupported],
        };
    }
    // K13's own question, asked with `workspace_exists: true` because the image
    // carries the workspace — the target creates it, so it cannot be the thing
    // that is missing.
    let verdict = restorability(
        &header.manifest,
        &RestoreEnvironment {
            // A migration really is the case residency was written for: this
            // node is about to run the image itself, so what it has loaded is
            // the authority over whether it can.
            resident_models: Some(target.resident_models),
            live_approvals: target.live_approvals,
            workspace_exists: true,
        },
    );
    if let Restorability::Blocked { blockers: shared } = &verdict {
        blockers.extend(
            shared
                .iter()
                .copied()
                .filter_map(MigrationBlocker::from_restore),
        );
    }
    if let Some(runtime) = header
        .manifest
        .resume
        .as_ref()
        .and_then(|resume| resume.runtime_id.as_ref())
    {
        if !target.runtime_ids.iter().any(|entry| entry == runtime) {
            blockers.push(MigrationBlocker::RuntimeMissing);
        }
    }
    if target.run_present {
        blockers.push(MigrationBlocker::RunAlreadyPresent);
    }
    if header.payload_bytes > target.max_payload_bytes.min(MAX_MIGRATION_PAYLOAD_BYTES) {
        blockers.push(MigrationBlocker::ImageTooLarge);
    }
    if header
        .required_residency
        .as_ref()
        .is_some_and(|required| required != target.residency)
    {
        blockers.push(MigrationBlocker::DataResidencyRefused);
    }
    if blockers.is_empty() {
        MigrationVerdict::Acceptable {
            run_id: header.run_id.clone(),
            process_id: header.process_id.clone(),
            caveats: caveats(),
        }
    } else {
        blockers.sort_unstable();
        blockers.dedup();
        MigrationVerdict::Refused { blockers }
    }
}

/// Everything a move does not preserve: K13's resume caveats plus K18's.
///
/// One list rather than two, because the person deciding to migrate is deciding
/// to resume as well — splitting them would let a caller show half.
#[must_use]
pub fn caveats() -> Vec<String> {
    DETERMINISM_CAVEATS
        .iter()
        .chain(MIGRATION_CAVEATS.iter())
        .map(|entry| (*entry).to_string())
        .collect()
}

/// Collects a directory tree into migration files, depth-first and bounded.
///
/// Symlinks are skipped rather than followed: a link is a reference to
/// something outside the image by definition, and following one would either
/// escape the root or silently duplicate a file already in it.
pub fn collect_tree(root: &Path) -> Result<Vec<MigrationFile>, String> {
    let mut files = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    let mut total: u64 = 0;
    while let Some(directory) = stack.pop() {
        let entries = std::fs::read_dir(&directory)
            .map_err(|error| format!("Could not read '{}': {error}", directory.display()))?;
        for entry in entries {
            let entry =
                entry.map_err(|error| format!("Could not read a directory entry: {error}"))?;
            let metadata = entry
                .metadata()
                .map_err(|error| format!("Could not stat '{}': {error}", entry.path().display()))?;
            if metadata.is_symlink() {
                continue;
            }
            if metadata.is_dir() {
                stack.push(entry.path());
                continue;
            }
            if files.len() >= MAX_MIGRATION_FILES {
                return Err(format!(
                    "The migrated tree has more than {MAX_MIGRATION_FILES} files"
                ));
            }
            total = total.saturating_add(metadata.len());
            if total > MAX_MIGRATION_PAYLOAD_BYTES {
                return Err(format!(
                    "The migrated tree is over the {MAX_MIGRATION_PAYLOAD_BYTES}-byte image limit"
                ));
            }
            files.push(MigrationFile::read(root, &entry.path())?);
        }
    }
    files.sort_by(|left, right| left.path.cmp(&right.path));
    Ok(files)
}

fn portable_relative(relative: &Path) -> Result<String, String> {
    let mut parts = Vec::new();
    for component in relative.components() {
        match component {
            Component::Normal(part) => parts.push(
                part.to_str()
                    .ok_or_else(|| "A migrated path is not valid UTF-8".to_string())?
                    .to_string(),
            ),
            _ => {
                return Err(format!(
                    "'{}' is not a plain relative path",
                    relative.display()
                ))
            }
        }
    }
    if parts.is_empty() {
        return Err("A migrated path is empty".to_string());
    }
    Ok(parts.join("/"))
}

/// Refuses anything that could write outside the destination root.
fn validate_relative(path: &str) -> Result<(), String> {
    if path.is_empty() || path.len() > 1_024 {
        return Err("A migrated path has an invalid length".to_string());
    }
    if path.contains('\\') || path.contains('\0') {
        return Err(format!(
            "'{path}' contains a path separator this image may not use"
        ));
    }
    if Path::new(path).is_absolute() {
        return Err(format!("'{path}' is absolute"));
    }
    for segment in path.split('/') {
        if segment.is_empty() || segment == "." || segment == ".." {
            return Err(format!("'{path}' does not stay inside the destination"));
        }
    }
    Ok(())
}

fn decoded_len(base64: &str) -> u64 {
    // Exact for canonical padded base64, which `STANDARD.encode` always emits.
    let padding = base64
        .bytes()
        .rev()
        .take_while(|byte| *byte == b'=')
        .count();
    (base64.len() as u64 / 4) * 3 - padding as u64
}

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut encoded = String::with_capacity(digest.len() * 2);
    for byte in digest.iter() {
        encoded.push_str(&format!("{byte:02x}"));
    }
    encoded
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::checkpoints::ResumeState;

    fn manifest(resume: Option<ResumeState>) -> CheckpointManifest {
        CheckpointManifest {
            version: 3,
            created_at_ms: 1,
            session_id: "session-01".to_string(),
            anchor_index: 0,
            label: "a turn".to_string(),
            shell_ran: false,
            external_effects: vec![],
            committed_effects: None,
            reverted: false,
            prev_id: None,
            entries: vec![],
            remembered_facts: vec![],
            staged_task_suggestions: vec![],
            resume,
        }
    }

    fn resume() -> ResumeState {
        ResumeState {
            process_id: "turn-01".to_string(),
            frozen_at_ms: 10,
            model: Some("qwen3-8b".to_string()),
            runtime_id: Some("llama-local".to_string()),
            workspace: Some("/origin/work".to_string()),
            pending_approvals: vec![],
        }
    }

    fn header(resume: Option<ResumeState>) -> MigrationHeader {
        MigrationHeader {
            protocol_version: MIGRATION_PROTOCOL_VERSION,
            origin_node_id: "node-a".to_string(),
            run_id: "run-01".to_string(),
            process_id: "turn-01".to_string(),
            checkpoint_id: "cp-1".to_string(),
            manifest: manifest(resume),
            required_residency: Some("eu".to_string()),
            payload_bytes: 4,
            payload_sha256: "0".repeat(64),
        }
    }

    fn models() -> Vec<String> {
        vec!["qwen3-8b".to_string()]
    }

    fn runtimes() -> Vec<String> {
        vec!["llama-local".to_string()]
    }

    fn target<'a>(models: &'a [String], runtimes: &'a [String]) -> TargetNode<'a> {
        TargetNode {
            node_id: "node-b",
            resident_models: models,
            runtime_ids: runtimes,
            live_approvals: &[],
            residency: "eu",
            max_payload_bytes: MAX_MIGRATION_PAYLOAD_BYTES,
            run_present: false,
        }
    }

    #[test]
    fn admits_a_freeze_whose_requirements_the_node_meets() {
        let verdict = admit(&header(Some(resume())), &target(&models(), &runtimes()));
        match verdict {
            MigrationVerdict::Acceptable {
                run_id,
                process_id,
                caveats,
            } => {
                assert_eq!(run_id, "run-01");
                assert_eq!(process_id, "turn-01");
                // The determinism statement travels with the verdict, not in a doc.
                assert!(caveats.len() >= DETERMINISM_CAVEATS.len() + MIGRATION_CAVEATS.len());
            }
            other => panic!("expected acceptance, got {other:?}"),
        }
    }

    #[test]
    fn reports_every_refusal_at_once_rather_than_the_first() {
        let mut node_models = Vec::new();
        node_models.push("something-else".to_string());
        let mut node = target(&node_models, &[]);
        node.run_present = true;
        node.residency = "us";
        let MigrationVerdict::Refused { blockers } = admit(&header(Some(resume())), &node) else {
            panic!("expected a refusal");
        };
        assert!(blockers.contains(&MigrationBlocker::ModelNotResident));
        assert!(blockers.contains(&MigrationBlocker::RuntimeMissing));
        assert!(blockers.contains(&MigrationBlocker::RunAlreadyPresent));
        assert!(blockers.contains(&MigrationBlocker::DataResidencyRefused));
    }

    #[test]
    fn a_turn_checkpoint_is_not_a_migratable_process() {
        let MigrationVerdict::Refused { blockers } =
            admit(&header(None), &target(&models(), &runtimes()))
        else {
            panic!("expected a refusal");
        };
        assert_eq!(blockers, vec![MigrationBlocker::NotAFreeze]);
    }

    #[test]
    fn an_outstanding_approval_the_target_does_not_grant_is_refused() {
        let mut state = resume();
        state.pending_approvals = vec!["req-01".to_string()];
        let MigrationVerdict::Refused { blockers } =
            admit(&header(Some(state)), &target(&models(), &runtimes()))
        else {
            panic!("expected a refusal");
        };
        assert!(blockers.contains(&MigrationBlocker::ApprovalExpired));
    }

    #[test]
    fn an_unknown_protocol_version_refuses_alone() {
        let mut value = header(Some(resume()));
        value.protocol_version = MIGRATION_PROTOCOL_VERSION + 1;
        // Deliberately also unsatisfiable on every other axis: the point is that
        // none of those checks are reported, because their fields are not
        // trustworthy under an unknown version.
        let verdict = admit(&value, &target(&[], &[]));
        assert_eq!(
            verdict,
            MigrationVerdict::Refused {
                blockers: vec![MigrationBlocker::ProtocolUnsupported]
            }
        );
    }

    #[test]
    fn a_path_that_escapes_the_destination_is_refused() {
        for path in ["../escape", "/etc/passwd", "a/../../b", "", "a\\b"] {
            assert!(
                validate_relative(path).is_err(),
                "'{path}' should not be writable"
            );
        }
        assert!(validate_relative("src/main.rs").is_ok());
    }

    #[test]
    fn a_payload_digest_covers_the_bytes() {
        let file = MigrationFile {
            path: "a.txt".to_string(),
            sha256: sha256_hex(b"hello"),
            contents_base64: STANDARD.encode(b"hello"),
        };
        let payload = MigrationPayload {
            checkpoint_files: vec![],
            workspace_files: vec![file.clone()],
            session: None,
        };
        let before = payload.digest().expect("digest");
        assert_eq!(payload.decoded_bytes(), 5);
        assert_eq!(file.decode().expect("decode"), b"hello");

        let mut tampered = payload;
        tampered.workspace_files[0].contents_base64 = STANDARD.encode(b"hellp");
        assert_ne!(before, tampered.digest().expect("digest"));
        // And the per-file digest catches it even if the payload digest were
        // recomputed by whoever tampered.
        assert!(tampered.workspace_files[0].decode().is_err());
    }

    #[test]
    fn decoded_len_matches_real_base64() {
        for size in 0..8usize {
            let bytes = vec![7u8; size];
            assert_eq!(decoded_len(&STANDARD.encode(&bytes)), size as u64);
        }
    }
}
