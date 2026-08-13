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
/// A capture may contain a 32 MiB binary artifact encoded as base64 plus
/// bounded JSON metadata. Hyper still enforces this before allocating the
/// complete body; endpoint-specific validation applies the tighter artifact
/// grant after authentication.
pub const MAX_REMOTE_BODY_BYTES: usize = 48 * 1024 * 1024;
pub const MAX_REMOTE_ARTIFACT_BYTES: u64 = 32 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RemoteAction {
    ViewRuns,
    ViewEvents,
    ReadArtifacts,
    Approve,
    Cancel,
    /// Suspend and resume a run without ending it. Separate from `Cancel`
    /// because it is strictly weaker — a paused run keeps its place and can be
    /// resumed — so a controller trusted to pause is not thereby trusted to
    /// destroy work.
    Pause,
    Kill,
    /// Drive the runner's real keyboard/mouse through the gated
    /// `little_monkey_lib::desktop_control` core. Distinct from every other
    /// action: it never touches a run, it always requires local visible
    /// consent on the runner before a session is created, and it can be
    /// force-stopped instantly by revoke or the kill switch.
    ControlDesktop,
}

/// Capabilities exposed to first-party mobile controllers. Legacy remote
/// actions remain in `RemoteScopes.actions` for wire compatibility; this
/// separate grant lets a pairing invite and accept response down-scope the
/// mobile-only surface without ever widening the underlying run scope.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeviceCapability {
    ViewRuns,
    ViewEvents,
    ReadArtifacts,
    Approve,
    Cancel,
    Pause,
    Kill,
    ControlDesktop,
    ViewSessions,
    Chat,
    ViewTasks,
    RunWorkflows,
    Capture,
    Admin,
    /// Read this node's identity, hardware, backends, resident models and
    /// residency label (roadmap K17 S1). Read-only and strictly weaker than
    /// [`Self::PlaceRuns`]: a scheduler needs to know what a node *is* long
    /// before it is allowed to put work on it, and an operator who wants only
    /// the inventory should not have to grant placement to get it.
    DescribeNode,
    /// Submit a frozen `RunSpec` to this node and read the placements made with
    /// it (roadmap K17 S2).
    ///
    /// **The only capability in this enum that lets a remote device cause this
    /// machine to execute a run it did not author**, which is why it is separate
    /// from every existing grant and why nothing implies it. `RunWorkflows` is
    /// its nearest neighbour and is not the same thing: that launches a workflow
    /// this node already holds, under this node's own policy.
    PlaceRuns,
    /// Hand this node a *frozen process image* from another owned node and let
    /// it resume the run (roadmap K18).
    ///
    /// Strictly more than [`Self::PlaceRuns`], which is why it is not folded
    /// into it: placing a run submits a spec the node then executes under its
    /// own workspace and its own conversation. A migration additionally writes a
    /// workspace tree, a checkpoint and a *conversation* onto this machine —
    /// into the same session list the local user reads. An operator who wanted a
    /// scheduler to place work should not have granted, by implication, the
    /// ability to add transcripts to their own chat history.
    Migrate,
    // --- Physical device capabilities -----------------------------------
    //
    // Every capability above answers "what may this device do to the runner".
    // The eight below are the other direction: what the runner may ask of the
    // *phone's own hardware*. They are separate variants rather than a flag on
    // the existing ones because they invert who is acted upon — and because
    // `legacy_capabilities` cannot produce any of them, an already-paired
    // device gains none of them from an app update.
    /// Read the device's own identity, platform and advertised surface.
    /// Strictly weaker than every other physical capability and implied by
    /// none of them: an operator who wants an inventory should not have to
    /// grant a camera to get it.
    DeviceInfo,
    CameraCapture,
    MicrophoneCapture,
    LocationRead,
    NotificationPost,
    ScreenCapture,
    AudioPlayback,
    /// A continuous microphone stream, rather than one bounded recording.
    ///
    /// Dispatched as a *control* command — "open the microphone for this
    /// session" — whose audio never travels in the command's own result: the
    /// device posts it in chunks to the voice routes while the command is
    /// running, and the command's terminal report is the summary. That split is
    /// why this is not reachable through the discrete `device_action` tool: the
    /// tool's contract is one request, one answer, and a stream has neither.
    VoiceStream,
}

/// The capabilities whose subject is the device's own hardware rather than the
/// runner's state. Only these are intersected with the advertised surface and
/// the OS permission — a legacy pairing that has never advertised anything
/// still exercises its run-facing grants unchanged.
pub const PHYSICAL_DEVICE_CAPABILITIES: &[DeviceCapability] = &[
    DeviceCapability::DeviceInfo,
    DeviceCapability::CameraCapture,
    DeviceCapability::MicrophoneCapture,
    DeviceCapability::LocationRead,
    DeviceCapability::NotificationPost,
    DeviceCapability::ScreenCapture,
    DeviceCapability::AudioPlayback,
    DeviceCapability::VoiceStream,
];

impl DeviceCapability {
    /// Whether this capability acts on the device's own hardware.
    pub fn is_physical(self) -> bool {
        PHYSICAL_DEVICE_CAPABILITIES.contains(&self)
    }

    pub fn for_action(action: RemoteAction) -> Self {
        match action {
            RemoteAction::ViewRuns => Self::ViewRuns,
            RemoteAction::ViewEvents => Self::ViewEvents,
            RemoteAction::ReadArtifacts => Self::ReadArtifacts,
            RemoteAction::Approve => Self::Approve,
            RemoteAction::Cancel => Self::Cancel,
            RemoteAction::Pause => Self::Pause,
            RemoteAction::Kill => Self::Kill,
            RemoteAction::ControlDesktop => Self::ControlDesktop,
        }
    }
}

pub fn legacy_capabilities(scopes: &RemoteScopes) -> BTreeSet<DeviceCapability> {
    scopes
        .actions
        .iter()
        .copied()
        .map(DeviceCapability::for_action)
        .collect()
}

pub fn validate_capabilities(
    capabilities: &BTreeSet<DeviceCapability>,
    scopes: &RemoteScopes,
) -> Result<(), String> {
    if !legacy_capabilities(scopes).is_subset(capabilities) {
        return Err(
            "Device capabilities must include every granted legacy remote action".to_string(),
        );
    }
    if capabilities.contains(&DeviceCapability::Chat)
        && !capabilities.contains(&DeviceCapability::ViewSessions)
    {
        return Err("Mobile chat also requires view_sessions".to_string());
    }
    if capabilities.contains(&DeviceCapability::RunWorkflows)
        && !capabilities.contains(&DeviceCapability::ViewTasks)
    {
        return Err("Mobile workflow launch also requires view_tasks".to_string());
    }
    // Same shape as every other dependency here: a device that may place work
    // must also be able to see what it placed it on, and the run rows its
    // placements create. Granting placement without either would produce a
    // scheduler that can start work on a machine it cannot describe and cannot
    // then observe.
    if capabilities.contains(&DeviceCapability::PlaceRuns)
        && !capabilities.contains(&DeviceCapability::DescribeNode)
    {
        return Err("Placing runs on this node also requires describe_node".to_string());
    }
    if capabilities.contains(&DeviceCapability::PlaceRuns)
        && !capabilities.contains(&DeviceCapability::ViewRuns)
    {
        return Err("Placing runs on this node also requires view_runs".to_string());
    }
    // A migration *is* a placement — it hands this node a `RunSpec` it did not
    // author — plus the image. Requiring the weaker grant keeps one answer to
    // "may this device cause work here", instead of two that could disagree.
    if capabilities.contains(&DeviceCapability::Migrate)
        && !capabilities.contains(&DeviceCapability::PlaceRuns)
    {
        return Err("Migrating a process onto this node also requires place_runs".to_string());
    }
    // A continuous microphone stream is a superset of one bounded recording, so
    // it cannot be the *only* microphone grant: an operator revoking
    // `microphone_capture` and believing the microphone is now closed would
    // otherwise be wrong. Nothing else here implies another physical
    // capability — a camera grant must never carry a location grant with it.
    if capabilities.contains(&DeviceCapability::VoiceStream)
        && !capabilities.contains(&DeviceCapability::MicrophoneCapture)
    {
        return Err("Streaming voice also requires microphone_capture".to_string());
    }
    Ok(())
}

/// What the device reports about one OS permission right now.
///
/// `Unsupported` and `Denied` are deliberately distinct: an operator looking at
/// a phone that cannot capture the screen at all should not be told to go turn
/// a permission on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OsPermission {
    Granted,
    Denied,
    /// The OS has not been asked yet — the device will prompt when a command
    /// first needs it.
    Undetermined,
    /// This platform/build has no such facility.
    Unsupported,
}

/// Bounds the device says it will enforce on its own, so the runner can refuse
/// an impossible command before it ever leases one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeviceConstraints {
    pub max_artifact_bytes: u64,
    pub max_recording_ms: u64,
    pub max_notification_chars: u32,
    /// Camera positions this device actually has (`front`, `back`).
    #[serde(default)]
    pub camera_positions: BTreeSet<String>,
}

impl Default for DeviceConstraints {
    fn default() -> Self {
        Self {
            max_artifact_bytes: 8 * 1024 * 1024,
            max_recording_ms: 60_000,
            max_notification_chars: 512,
            camera_positions: BTreeSet::new(),
        }
    }
}

/// What a paired physical device says it is and can do, refreshed each time it
/// connects.
///
/// Deliberately *not* folded into `node_placement::NodeDescriptor`: that
/// describes a compute node's hardware for placement decisions (queue depth,
/// backends, resident models), and this describes a phone's sensors and OS
/// permissions. One table each keeps a query for "where can this run go" from
/// having to filter out phones.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeviceSurface {
    pub protocol_version: u32,
    /// `ios`, `android`, `web`, … — the device's own word for itself, only
    /// ever displayed and never used to decide authority.
    pub platform: String,
    pub platform_version: String,
    pub app_version: String,
    pub device_model: String,
    /// What this build can actually do, regardless of what was granted.
    pub capabilities: BTreeSet<DeviceCapability>,
    /// Current OS permission per capability. A capability absent from this map
    /// is treated as [`OsPermission::Undetermined`].
    #[serde(default)]
    pub permissions: BTreeMap<DeviceCapability, OsPermission>,
    #[serde(default)]
    pub constraints: DeviceConstraints,
    pub reported_at_ms: u64,
}

impl DeviceSurface {
    pub fn validate(&self) -> Result<(), String> {
        if self.protocol_version != REMOTE_PROTOCOL_VERSION {
            return Err(format!(
                "Unsupported remote protocol version {}",
                self.protocol_version
            ));
        }
        for (label, value) in [
            ("platform", &self.platform),
            ("platform version", &self.platform_version),
            ("app version", &self.app_version),
            ("device model", &self.device_model),
        ] {
            if value.trim().is_empty() || value.len() > 128 || value.contains(['\r', '\n']) {
                return Err(format!(
                    "Device {label} must be a non-empty single line under 128 bytes"
                ));
            }
        }
        if self.capabilities.len() > 64 || self.permissions.len() > 64 {
            return Err("Device surface advertises too many capabilities".to_string());
        }
        if self.constraints.max_artifact_bytes == 0
            || self.constraints.max_artifact_bytes > MAX_REMOTE_ARTIFACT_BYTES
        {
            return Err(format!(
                "Device artifact bound must be between 1 and {MAX_REMOTE_ARTIFACT_BYTES} bytes"
            ));
        }
        if self.constraints.max_recording_ms == 0 || self.constraints.max_recording_ms > 600_000 {
            return Err("Device recording bound must be between 1 ms and 10 minutes".to_string());
        }
        if self.constraints.max_notification_chars == 0
            || self.constraints.max_notification_chars > 4_096
        {
            return Err(
                "Device notification bound must be between 1 and 4096 characters".to_string(),
            );
        }
        if self.constraints.camera_positions.len() > 8
            || self
                .constraints
                .camera_positions
                .iter()
                .any(|position| !matches!(position.as_str(), "front" | "back" | "external"))
        {
            return Err("Camera positions must be front, back or external".to_string());
        }
        Ok(())
    }

    pub fn permission(&self, capability: DeviceCapability) -> OsPermission {
        self.permissions
            .get(&capability)
            .copied()
            .unwrap_or(OsPermission::Undetermined)
    }
}

/// `operator grant ∩ advertised support ∩ current OS permission`, evaluated for
/// the physical capabilities and only for them.
///
/// A run-facing grant (`view_runs`, `chat`, `place_runs`, …) passes through
/// untouched: those act on the runner, not on the phone, and every device
/// paired before this existed advertises nothing at all. Without a surface, no
/// physical capability is effective — a device that has never said it has a
/// camera is not asked to open one.
pub fn effective_capabilities(
    granted: &BTreeSet<DeviceCapability>,
    surface: Option<&DeviceSurface>,
) -> BTreeSet<DeviceCapability> {
    granted
        .iter()
        .copied()
        .filter(|capability| {
            if !capability.is_physical() {
                return true;
            }
            surface.is_some_and(|surface| {
                surface.capabilities.contains(capability)
                    && surface.permission(*capability) == OsPermission::Granted
            })
        })
        .collect()
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
        // Same rule the other run-targeting actions carry: a controller that
        // cannot see which runs exist has no business suspending one.
        if self.actions.contains(&RemoteAction::Pause)
            && !self.actions.contains(&RemoteAction::ViewRuns)
        {
            return Err("Pause scope also requires view_runs".to_string());
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
    #[serde(default)]
    pub capabilities: BTreeSet<DeviceCapability>,
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
        self.scopes.validate()?;
        let capabilities = if self.capabilities.is_empty() {
            legacy_capabilities(&self.scopes)
        } else {
            self.capabilities.clone()
        };
        validate_capabilities(&capabilities, &self.scopes)
    }
}

/// Everything a phone needs to reach this runner and pin it, small enough to
/// scan.
///
/// The full [`PairingInvitation`] carries the server certificate as PEM, which
/// is several kilobytes — past what a phone camera reads reliably from a screen.
/// This carries the SHA-256 fingerprint instead. That is not weaker pinning: the
/// fingerprint is exactly what `validate_invitation` compares the presented
/// certificate against, and the PEM was only ever a convenience copy of the
/// certificate the runner presents on the wire anyway. The short field names are
/// the reason this fits — see `QR_BYTE_TARGET`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PairingBootstrap {
    #[serde(rename = "v")]
    pub protocol_version: u32,
    #[serde(rename = "r")]
    pub runner_id: String,
    #[serde(rename = "u")]
    pub runner_url: String,
    #[serde(rename = "p")]
    pub pairing_id: String,
    #[serde(rename = "t")]
    pub pairing_token: String,
    /// SHA-256 of the runner's DER certificate — the pin.
    #[serde(rename = "f")]
    pub certificate_sha256: String,
    #[serde(rename = "e")]
    pub expires_at_ms: u64,
}

/// What a normal compact QR must stay under, so it scans from a phone at arm's
/// length. Enforced by a regression test rather than at runtime: an unusually
/// long operator-chosen URL should still pair, just from a denser code.
pub const QR_BYTE_TARGET: usize = 900;

/// URI scheme for the compact code. A scheme rather than bare JSON so a phone's
/// camera app can offer to open it, and so a scanned blob that is plainly not a
/// pairing code is rejected before parsing.
pub const PAIRING_URI_SCHEME: &str = "littlemonkey://pair/";

impl PairingBootstrap {
    /// The scannable string: the scheme, then URL-safe base64 of the compact
    /// JSON. Base64 rather than raw JSON because a QR payload containing `{`,
    /// `"` and `/` forces byte mode and a percent-encoded URI is longer still.
    pub fn to_uri(&self) -> Result<String, String> {
        let json = serde_json::to_vec(self).map_err(|error| error.to_string())?;
        Ok(format!(
            "{PAIRING_URI_SCHEME}{}",
            URL_SAFE_NO_PAD.encode(json)
        ))
    }

    pub fn from_uri(value: &str) -> Result<Self, String> {
        let encoded = value
            .trim()
            .strip_prefix(PAIRING_URI_SCHEME)
            .ok_or_else(|| "Not a Little Monkey pairing code".to_string())?;
        if encoded.len() > 8 * 1024 {
            return Err("Pairing code is too large".to_string());
        }
        let json = URL_SAFE_NO_PAD
            .decode(encoded)
            .map_err(|error| format!("Pairing code is not valid base64: {error}"))?;
        serde_json::from_slice(&json).map_err(|error| format!("Pairing code is invalid: {error}"))
    }

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
        validate_sha256(&self.certificate_sha256)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PairAcceptRequest {
    pub protocol_version: u32,
    pub pairing_id: String,
    pub pairing_token: String,
    pub device_name: String,
    /// Optional explicit subset selected by the controller. Legacy clients
    /// omit it and receive the invitation's complete capability grant.
    #[serde(default)]
    pub requested_capabilities: Option<BTreeSet<DeviceCapability>>,
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
    #[serde(default)]
    pub capabilities: BTreeSet<DeviceCapability>,
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
    #[serde(default)]
    pub capabilities: BTreeSet<DeviceCapability>,
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
    #[serde(default)]
    pub capabilities: BTreeSet<DeviceCapability>,
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

/// Body of `POST /v1/remote/desktop-control/start`. The `batch_mode` flag is
/// only ever a *request* — it is honoured solely when the runner's own local
/// consent prompt was answered "Allow (batch)"; the remote side asking for it
/// is never sufficient on its own (see `api::desktop_control_start`).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DesktopControlStartRequest {
    pub allowlist: Vec<String>,
    #[serde(default)]
    pub batch_mode: bool,
}

/// Body of `POST /v1/remote/desktop-control/action`. Reuses the desktop-control
/// core's own [`little_monkey_lib::desktop_control::ControlAction`] as the
/// payload rather than defining a parallel wire type.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DesktopControlActionRequest {
    pub session_id: String,
    /// Which allowlisted application/window this action is aimed at — the
    /// desktop-control core enforces this against the session allowlist.
    pub target_application_id: String,
    pub action: little_monkey_lib::desktop_control::ControlAction,
}

/// Body of `POST /v1/remote/desktop-control/stop`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DesktopControlStopRequest {
    pub session_id: String,
}

/// Body of `POST /v1/remote/migration/preflight` — metadata only, so a target
/// refuses before a byte of workspace crosses the wire.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MigrationPreflightRequest {
    pub protocol_version: u32,
    pub header: little_monkey_lib::migration::MigrationHeader,
}

/// Body of `POST /v1/remote/migration/accept` — the image itself.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MigrationAcceptRequest {
    pub protocol_version: u32,
    pub image: little_monkey_lib::migration::MigrationImage,
}

/// What the target did, returned to the origin so the move is auditable from
/// both ends without either node reading the other's database.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MigrationReceipt {
    pub protocol_version: u32,
    pub node_id: String,
    pub run_id: String,
    /// The process id the target admitted. Deliberately the target's own, not
    /// the origin's: two nodes minting the same process id would make
    /// `agent_processes` ambiguous on whichever one later received the other's
    /// audit.
    pub process_id: String,
    pub workspace_root: String,
    pub arrival_event_hash: String,
    pub caveats: Vec<String>,
}

/// Largest bounded argument object a device command may carry. Arguments are
/// instructions to hardware, not payloads: nothing legitimate here is large,
/// and the digest stored beside them is what makes "the device ran exactly what
/// was queued" auditable.
pub const MAX_DEVICE_COMMAND_ARG_BYTES: usize = 8 * 1024;
/// Longest a device may hold a lease before the runner may hand the command to
/// another connection. Also the ceiling on the long-poll wait.
pub const DEVICE_LEASE_MS: u64 = 30_000;

/// `queued -> leased -> running -> succeeded | failed | cancelled | expired`.
///
/// The `leased`/`running` split is the whole point: a lease that expires before
/// `running` is safely requeued, because nothing physical has happened yet.
/// After `running` the device has been told to open the camera, so a lost
/// connection can only ever resolve to a terminal state — never to a second
/// execution.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeviceCommandState {
    Queued,
    Leased,
    Running,
    Succeeded,
    Failed,
    Cancelled,
    Expired,
}

impl DeviceCommandState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Leased => "leased",
            Self::Running => "running",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::Expired => "expired",
        }
    }

    pub fn parse(value: &str) -> Result<Self, String> {
        Ok(match value {
            "queued" => Self::Queued,
            "leased" => Self::Leased,
            "running" => Self::Running,
            "succeeded" => Self::Succeeded,
            "failed" => Self::Failed,
            "cancelled" => Self::Cancelled,
            "expired" => Self::Expired,
            other => return Err(format!("Unknown device command state '{other}'")),
        })
    }

    pub fn terminal(self) -> bool {
        matches!(
            self,
            Self::Succeeded | Self::Failed | Self::Cancelled | Self::Expired
        )
    }
}

/// One leased command as the device sees it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeviceCommand {
    pub protocol_version: u32,
    pub command_id: String,
    pub capability: DeviceCapability,
    pub arguments: serde_json::Value,
    pub arguments_sha256: String,
    pub expires_at_ms: u64,
    pub lease_expires_at_ms: u64,
    /// True when an operator asked for cancellation while this command was
    /// queued or leased. The device must not begin the physical action.
    pub cancel_requested: bool,
}

/// What the device reports back once the physical action is over.
///
/// `outcome` is a [`DeviceCommandState`] restricted to the terminal ones by
/// [`DeviceCommandResult::validate`]; `unsupported` and `denied` are not extra
/// states but ordinary `failed` results carrying an honest `error` — an
/// operator reads why in one place rather than in two vocabularies.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeviceCommandResult {
    pub protocol_version: u32,
    pub outcome: DeviceCommandState,
    #[serde(default)]
    pub result: Option<serde_json::Value>,
    /// Base64 of the produced artifact (a still, a recording), stored through
    /// the normal artifact path rather than inlined into the result.
    #[serde(default)]
    pub artifact_base64: Option<String>,
    #[serde(default)]
    pub artifact_media_type: Option<String>,
    #[serde(default)]
    pub error: Option<String>,
}

impl DeviceCommandResult {
    pub fn validate(&self, max_artifact_bytes: u64) -> Result<(), String> {
        if self.protocol_version != REMOTE_PROTOCOL_VERSION {
            return Err(format!(
                "Unsupported remote protocol version {}",
                self.protocol_version
            ));
        }
        if !matches!(
            self.outcome,
            DeviceCommandState::Succeeded
                | DeviceCommandState::Failed
                | DeviceCommandState::Cancelled
        ) {
            return Err("A device may only report succeeded, failed or cancelled".to_string());
        }
        if let Some(result) = &self.result {
            let encoded = serde_json::to_vec(result).map_err(|error| error.to_string())?;
            if encoded.len() > MAX_DEVICE_COMMAND_ARG_BYTES {
                return Err("Device command result exceeds 8 KiB".to_string());
            }
        }
        if let Some(encoded) = &self.artifact_base64 {
            // Base64 inflates by 4/3; comparing the encoded length against the
            // budget refuses an oversized artifact before decoding it.
            if encoded.len() as u64 > max_artifact_bytes.saturating_mul(4).div_euclid(3) + 4 {
                return Err("Device artifact exceeds the granted budget".to_string());
            }
            if self.artifact_media_type.is_none() {
                return Err("A device artifact must declare its media type".to_string());
            }
        }
        if self.error.as_ref().is_some_and(|error| error.len() > 4_096) {
            return Err("Device command error text exceeds 4 KiB".to_string());
        }
        Ok(())
    }
}

/// Body of `POST /v1/remote/device/commands/{id}/start` — the device saying it
/// is about to touch hardware. Empty on purpose: the command id in the path and
/// the signature over it are the whole statement.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeviceCommandStartRequest {}

// --- Voice streaming ------------------------------------------------------
//
// A `voice_stream` command opens a *session*; the audio arrives afterwards on
// the two routes below, one chunk at a time, while the command is still
// `running`. Keeping the audio off the command result is what makes the stream
// bounded: a result is capped at 8 KiB and an artifact at the pairing's budget,
// neither of which can hold minutes of microphone.

/// Largest single chunk a device may post. A second of Opus is a few kilobytes;
/// this is room for a device that batches badly, not for a file upload.
pub const MAX_VOICE_CHUNK_BYTES: usize = 512 * 1024;
/// Ceiling on one whole session's audio, whatever the device claims it will
/// send. Reached, the session fails and the command fails with it.
pub const MAX_VOICE_SESSION_BYTES: u64 = 32 * 1024 * 1024;
/// Longest a stream may stay open. A microphone that is never closed because a
/// phone went into a tunnel is exactly what this bounds.
pub const MAX_VOICE_SESSION_MS: u64 = 10 * 60 * 1_000;
/// Container formats a device may declare. Closed set: the bytes are stored
/// verbatim and handed to whatever plays them later, so an unbounded media type
/// would be an unbounded string in a filename-adjacent position.
pub const VOICE_MEDIA_TYPES: &[&str] = &[
    "audio/webm",
    "audio/webm;codecs=opus",
    "audio/ogg",
    "audio/ogg;codecs=opus",
    "audio/mp4",
    "audio/wav",
];

/// `open -> closed | failed`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VoiceSessionState {
    Open,
    Closed,
    Failed,
}

impl VoiceSessionState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Closed => "closed",
            Self::Failed => "failed",
        }
    }

    pub fn parse(value: &str) -> Result<Self, String> {
        Ok(match value {
            "open" => Self::Open,
            "closed" => Self::Closed,
            "failed" => Self::Failed,
            other => return Err(format!("Unknown voice session state '{other}'")),
        })
    }
}

/// One chunk of a live stream.
///
/// `sequence` is what makes the append exactly-once over a link that drops: the
/// runner accepts only the sequence it is expecting next, treats anything lower
/// as a duplicate it already has, and refuses anything higher rather than
/// writing a hole into the audio.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VoiceChunkRequest {
    pub protocol_version: u32,
    pub sequence: u64,
    pub audio_base64: String,
    /// Declared on the first chunk and ignored afterwards — a session's
    /// container cannot change halfway through it.
    #[serde(default)]
    pub media_type: Option<String>,
    /// The device saying this is the last chunk. Advisory: `close` is what ends
    /// a session, so a stream cut off mid-flight still closes on the runner's
    /// own deadline.
    #[serde(default)]
    pub last: bool,
}

impl VoiceChunkRequest {
    pub fn validate(&self) -> Result<(), String> {
        if self.protocol_version != REMOTE_PROTOCOL_VERSION {
            return Err(format!(
                "Unsupported remote protocol version {}",
                self.protocol_version
            ));
        }
        // Base64 inflates by 4/3; checking the encoded length refuses an
        // oversized chunk before decoding it.
        if self.audio_base64.len() > MAX_VOICE_CHUNK_BYTES * 4 / 3 + 4 {
            return Err(format!(
                "A voice chunk may carry at most {MAX_VOICE_CHUNK_BYTES} bytes"
            ));
        }
        if self.audio_base64.is_empty() {
            return Err("A voice chunk carries no audio".to_string());
        }
        if let Some(media_type) = &self.media_type {
            if !VOICE_MEDIA_TYPES.contains(&media_type.as_str()) {
                return Err(format!("Unsupported voice media type '{media_type}'"));
            }
        }
        Ok(())
    }
}

/// The device ending a stream. An `error` here closes the session `failed`,
/// which is how a microphone that was denied halfway through is reported.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VoiceCloseRequest {
    pub protocol_version: u32,
    #[serde(default)]
    pub error: Option<String>,
}

impl VoiceCloseRequest {
    pub fn validate(&self) -> Result<(), String> {
        if self.protocol_version != REMOTE_PROTOCOL_VERSION {
            return Err(format!(
                "Unsupported remote protocol version {}",
                self.protocol_version
            ));
        }
        if self.error.as_ref().is_some_and(|error| error.len() > 4_096) {
            return Err("Voice session error text exceeds 4 KiB".to_string());
        }
        Ok(())
    }
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

/// Same as `random_token`, except the result is guaranteed to start and end
/// with an ASCII letter or digit — required for any token that gets promoted
/// into an identifier later checked by `run_protocol::validate_protocol_id`
/// (e.g. a `device_id` reused as `ClientIdentity.client_id`/`instance_id`).
/// `random_token`'s URL-safe base64 alphabet includes `-`/`_`, so an ordinary
/// call has roughly a 1-in-32 chance of landing one of those at either
/// boundary; resampling the whole token is simpler and just as sound as
/// trying to fix up only the offending byte, since every byte is uniformly
/// random anyway. Bounded to 8 attempts purely so a truly broken RNG fails
/// loudly instead of looping forever — a real `SystemRandom` failing this
/// check 8 times in a row is not a real-world scenario.
pub fn random_token_id(bytes: usize) -> Result<String, String> {
    for _ in 0..8 {
        let token = random_token(bytes)?;
        let boundary_safe = token
            .as_bytes()
            .first()
            .is_some_and(u8::is_ascii_alphanumeric)
            && token
                .as_bytes()
                .last()
                .is_some_and(u8::is_ascii_alphanumeric);
        if boundary_safe {
            return Ok(token);
        }
    }
    Err("Failed to generate a boundary-safe random token".to_string())
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

    /// Regression test for the real bug `random_token_id` fixes: a
    /// `device_id` built as `format!("device-{}", random_token(18)?)` had
    /// roughly a 1-in-32 chance of failing `run_protocol::validate_protocol_id`'s
    /// "must start and end with an ASCII letter or digit" rule once reused
    /// as `ClientIdentity.client_id`/`instance_id` (see `control_recorder`
    /// in `daemon/remote/api.rs`) — a real, permanently unusable paired
    /// device on a live pairing, not just test flakiness. 500 iterations
    /// makes a reintroduced bug (~6% failure rate per id, two ids checked
    /// per iteration) essentially certain to be caught.
    #[test]
    fn random_token_id_stays_boundary_safe_once_embedded_as_a_client_identity() {
        for _ in 0..500 {
            let device_id = format!("device-{}", random_token_id(18).unwrap());
            assert!(little_monkey_lib::run_protocol::validate_protocol_id(
                "client.client_id",
                &device_id
            )
            .is_ok());
            let instance_id = format!("remote-{device_id}");
            assert!(little_monkey_lib::run_protocol::validate_protocol_id(
                "client.instance_id",
                &instance_id
            )
            .is_ok());
        }
    }

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

    fn surface(
        capabilities: &[DeviceCapability],
        permissions: &[(DeviceCapability, OsPermission)],
    ) -> DeviceSurface {
        DeviceSurface {
            protocol_version: REMOTE_PROTOCOL_VERSION,
            platform: "ios".into(),
            platform_version: "18.2".into(),
            app_version: "1.3.0".into(),
            device_model: "iPhone".into(),
            capabilities: capabilities.iter().copied().collect(),
            permissions: permissions.iter().copied().collect(),
            constraints: DeviceConstraints::default(),
            reported_at_ms: 1_000,
        }
    }

    /// The rule the whole physical-device surface rests on: three sets have to
    /// agree, and any one of them saying no is enough.
    #[test]
    fn effective_authority_is_grant_intersect_advertised_intersect_os_permission() {
        let granted = BTreeSet::from([
            DeviceCapability::ViewRuns,
            DeviceCapability::CameraCapture,
            DeviceCapability::LocationRead,
            DeviceCapability::ScreenCapture,
            DeviceCapability::NotificationPost,
        ]);
        let advertised = surface(
            &[
                DeviceCapability::CameraCapture,
                DeviceCapability::LocationRead,
                DeviceCapability::NotificationPost,
            ],
            &[
                (DeviceCapability::CameraCapture, OsPermission::Granted),
                (DeviceCapability::LocationRead, OsPermission::Denied),
                // NotificationPost advertised but never asked for: undetermined
                // is not permission.
            ],
        );
        let effective = effective_capabilities(&granted, Some(&advertised));
        assert_eq!(
            effective,
            BTreeSet::from([DeviceCapability::ViewRuns, DeviceCapability::CameraCapture]),
            "only a capability granted, advertised and OS-permitted is effective"
        );
        // A capability the OS permits but the operator never granted stays out.
        let ungranted = effective_capabilities(
            &BTreeSet::from([DeviceCapability::ViewRuns]),
            Some(&surface(
                &[DeviceCapability::CameraCapture],
                &[(DeviceCapability::CameraCapture, OsPermission::Granted)],
            )),
        );
        assert_eq!(ungranted, BTreeSet::from([DeviceCapability::ViewRuns]));
    }

    /// A device paired before this feature existed advertises nothing. Its
    /// run-facing grants must keep working, and it must not become able to open
    /// a camera by upgrading its app.
    #[test]
    fn a_device_that_never_advertised_keeps_run_grants_and_gains_no_hardware() {
        let scopes = RemoteScopes {
            actions: BTreeSet::from([RemoteAction::ViewRuns, RemoteAction::Approve]),
            run_ids: BTreeSet::from(["run-one".into()]),
            workspace_ids: BTreeSet::new(),
            max_artifact_bytes: 1_024,
        };
        let legacy = legacy_capabilities(&scopes);
        assert!(
            !legacy.iter().any(|capability| capability.is_physical()),
            "no legacy remote action may imply a physical capability"
        );
        assert_eq!(effective_capabilities(&legacy, None), legacy);
        let with_hardware_grant = {
            let mut set = legacy.clone();
            set.insert(DeviceCapability::CameraCapture);
            set
        };
        assert_eq!(
            effective_capabilities(&with_hardware_grant, None),
            legacy,
            "granted but never advertised is not effective"
        );
    }

    #[test]
    fn streaming_voice_cannot_be_the_only_microphone_grant() {
        let scopes = RemoteScopes {
            actions: BTreeSet::from([RemoteAction::ViewRuns]),
            run_ids: BTreeSet::from(["run-one".into()]),
            workspace_ids: BTreeSet::new(),
            max_artifact_bytes: 1_024,
        };
        let mut capabilities = legacy_capabilities(&scopes);
        capabilities.insert(DeviceCapability::VoiceStream);
        assert!(validate_capabilities(&capabilities, &scopes).is_err());
        capabilities.insert(DeviceCapability::MicrophoneCapture);
        assert!(validate_capabilities(&capabilities, &scopes).is_ok());
    }

    /// The size claim the compact code exists for. A realistic runner URL,
    /// full-length token and fingerprint must stay well inside the target, and
    /// the PEM must not have crept back in.
    #[test]
    fn compact_pairing_code_round_trips_and_stays_scannable() {
        let bootstrap = PairingBootstrap {
            protocol_version: REMOTE_PROTOCOL_VERSION,
            runner_id: format!("runner-{}", "a".repeat(24)),
            runner_url: "https://desktop.example-household.net:8443".into(),
            pairing_id: format!("pair-{}", random_token(18).unwrap()),
            pairing_token: random_token(32).unwrap(),
            certificate_sha256: "b".repeat(64),
            expires_at_ms: 2_000,
        };
        let uri = bootstrap.to_uri().unwrap();
        assert!(
            uri.len() <= QR_BYTE_TARGET,
            "compact pairing code is {} bytes, over the {QR_BYTE_TARGET}-byte scan target",
            uri.len()
        );
        assert!(!uri.contains("CERTIFICATE"));
        assert_eq!(PairingBootstrap::from_uri(&uri).unwrap(), bootstrap);
        assert!(bootstrap.validate(1_000).is_ok());
        assert!(bootstrap.validate(3_000).is_err(), "expiry must be checked");
        assert!(PairingBootstrap::from_uri("https://evil.test/pair").is_err());
        let unpinned = PairingBootstrap {
            certificate_sha256: "not-a-digest".into(),
            ..bootstrap
        };
        assert!(
            unpinned.validate(1_000).is_err(),
            "a code without a valid pin must not pair"
        );
    }

    #[test]
    fn a_device_result_cannot_smuggle_an_oversized_artifact() {
        let result = DeviceCommandResult {
            protocol_version: REMOTE_PROTOCOL_VERSION,
            outcome: DeviceCommandState::Succeeded,
            result: None,
            artifact_base64: Some("A".repeat(4_096)),
            artifact_media_type: Some("image/jpeg".into()),
            error: None,
        };
        assert!(result.validate(4_096).is_ok());
        assert!(result.validate(1_024).is_err());
        let untyped = DeviceCommandResult {
            artifact_media_type: None,
            ..result.clone()
        };
        assert!(untyped.validate(4_096).is_err());
        let running = DeviceCommandResult {
            outcome: DeviceCommandState::Running,
            artifact_base64: None,
            artifact_media_type: None,
            ..result
        };
        assert!(
            running.validate(4_096).is_err(),
            "a device may not report a non-terminal outcome as its result"
        );
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
