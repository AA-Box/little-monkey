use std::collections::{BTreeMap, BTreeSet};

use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use base64::Engine;
use little_monkey_lib::run_ledger::StoredRun;
use ring::rand::SecureRandom;
use ring::{hmac, rand as ring_rand};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub const REMOTE_PROTOCOL_VERSION: u32 = 2;
pub const DEFAULT_REQUEST_SKEW_MS: u64 = 5 * 60 * 1_000;
/// A capture may contain a 32 MiB binary artifact encoded as base64 plus
/// bounded JSON metadata. Hyper still enforces this before allocating the
/// complete body; endpoint-specific validation applies the tighter artifact
/// grant after authentication.
pub const MAX_REMOTE_BODY_BYTES: usize = 48 * 1024 * 1024;
pub const MAX_REMOTE_ARTIFACT_BYTES: u64 = 32 * 1024 * 1024;

/// Dedicated Talk frames are versioned independently so their WebSocket wire
/// shape can evolve without changing the signed HTTP remote plane.
///
/// # v2 — a closing audio frame must name its utterance
///
/// v1 accepted a `last: true` audio frame with no `utterance_id`; v2 refuses
/// one. That is a semantic change to what a v1 frame means, so it gets a
/// version rather than being made quietly under the old number: a client that
/// still speaks v1 is refused *once, by version*, with something it can act on
/// — reload — instead of having every utterance rejected by a field-level
/// error it cannot interpret.
///
/// The old behaviour is deliberately not kept as a fallback. Minting the
/// identity here is what caused the duplicate-after-restart it was added to
/// close, so accepting a frame without one would reintroduce it under a
/// compatibility flag.
/// v3 adds [`TalkServerFrameKind::TurnAccepted`], which is the *only* signal a
/// device may delete a recording on.
///
/// It is a version rather than an additive frame, even though a v2 client's
/// `switch` would ignore an unknown type harmlessly. The incompatibility runs
/// the other way: a v3 client keeps every unacknowledged utterance and offers
/// to re-send it, so against a runner that never emits the acknowledgement it
/// would offer to re-send *every* turn — including ones already answered. That
/// is exactly the "tell somebody to repeat what is already running" failure the
/// journal exists to prevent, so the two sides are pinned to each other.
pub const TALK_PROTOCOL_VERSION: u32 = 3;

/// The version whose only difference from [`TALK_PROTOCOL_VERSION`] is the
/// missing utterance id — so a client speaking it can be told precisely what is
/// wrong rather than being handed an opaque refusal.
const TALK_PROTOCOL_VERSION_WITHOUT_UTTERANCE_ID: u32 = 1;

/// The version that names its utterances but has nowhere to hear that one was
/// durably accepted. Refused by version for the reason above.
const TALK_PROTOCOL_VERSION_WITHOUT_ACCEPTANCE: u32 = 2;
pub const MAX_TALK_AUDIO_BYTES: usize = MAX_VOICE_CHUNK_BYTES;
pub const MAX_TALK_AUDIO_BASE64_BYTES: usize = MAX_TALK_AUDIO_BYTES.div_ceil(3) * 4;
pub const MAX_TALK_FRAME_BYTES: usize = MAX_TALK_AUDIO_BASE64_BYTES + 16 * 1024;
pub const MAX_TALK_TEXT_BYTES: usize = 64 * 1024;
pub const MAX_TALK_ERROR_BYTES: usize = 4 * 1024;
pub const MAX_TALK_MEDIA_TYPE_BYTES: usize = 128;
pub const MAX_TALK_SESSION_ID_BYTES: usize = 256;
/// Long enough for a UUID or a device-local counter with a prefix, short enough
/// that it cannot become somewhere to park data on a frame that is otherwise
/// pure audio.
pub const MAX_TALK_UTTERANCE_ID_BYTES: usize = 128;
pub const MAX_TALK_SESSION_GENERATION_BYTES: usize = 128;
pub const MAX_TALK_TICKET_BYTES: usize = 128;
pub const DEFAULT_TALK_TICKET_TTL_MS: u64 = 30_000;
pub const MAX_TALK_TICKET_TTL_MS: u64 = 60_000;
/// The longest span a Talk latency sample may claim. Matches the desktop's own
/// telemetry ceiling, so a phone and a laptop are graded on one scale.
pub const MAX_TALK_LATENCY_MS: u64 = 10 * 60 * 1_000;
const TALK_SESSION_GENERATION_RANDOM_BYTES: usize = 18;
const TALK_TICKET_RANDOM_BYTES: usize = 32;

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
    /// Exchange bounded messages with this installation as a paired *peer*
    /// rather than as a controller (roadmap: peer agents).
    ///
    /// Peer standing is its own grant and implies nothing else in this enum: a
    /// peer that may talk cannot read runs, cannot approve, cannot place work
    /// and cannot reach the desktop. `legacy_capabilities` never produces any
    /// of the three, so an existing pairing does not silently become a peer
    /// when this build ships.
    PeerMessage,
    /// Ask this installation to do something as a paired peer. The request
    /// becomes an ordinary durable turn under this node's own recipe and
    /// permission policy — it is a request, never an instruction.
    ///
    /// Separate from [`Self::PeerMessage`] because saying something and asking
    /// for work are different acts, and an operator who allowed the first has
    /// not agreed to the second.
    PeerTaskRequest,
    /// Attach artifact references to a peer thread. Separate again: handing
    /// content over is not the same as talking, and it is the grant that
    /// decides whether this node will fetch what a peer offers.
    PeerArtifact,
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
/// a permission on. `NotRequired` is distinct again, and is the variant that
/// makes the model honest: several physical capabilities have no persistent OS
/// permission at all (reading the device's own name, playing a sound), and
/// pretending they do left them advertised and permanently ineffective.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OsPermission {
    Granted,
    Denied,
    /// The OS has not been asked yet — the device will prompt when a command
    /// first needs it.
    ///
    /// Retained as the wire spelling older device builds send. Identical to
    /// [`Self::Promptable`] everywhere it is judged: neither is permission.
    Undetermined,
    /// The OS has not been asked yet and *can* be asked, from a user gesture on
    /// the device. Never asked on the runner's behalf.
    Promptable,
    /// This capability needs no persistent OS permission on this platform.
    NotRequired,
    /// This platform/build has no such facility.
    Unsupported,
}

impl OsPermission {
    /// Whether this state is permission to act. `Undetermined`/`Promptable` are
    /// deliberately not: "the OS has not been asked" is never a yes.
    pub fn is_sufficient(self) -> bool {
        matches!(self, Self::Granted | Self::NotRequired)
    }
}

/// Whether the device could act *right now*, given everything the OS permission
/// does not cover.
///
/// A separate axis from the permission because the answers are separate
/// questions with separate fixes. A browser that holds the camera permission
/// still cannot capture while the tab is in the background; a display stream
/// needs the user to share once and stays shared until they stop; autoplay
/// stays blocked until the page has been interacted with. Collapsing any of
/// those into "permission denied" would send the operator to a settings screen
/// that would not help.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeviceReadiness {
    Ready,
    /// The device can do this, but only while its screen is on and the
    /// controller is in front.
    ForegroundRequired,
    /// The platform demands a user gesture before this works at all (autoplay
    /// being the usual one).
    InteractionRequired,
    /// A one-time consent the user must arm, which then stays armed until they
    /// end it — screen sharing.
    ArmedRequired,
    /// Nothing can be done about it from here.
    Unavailable,
}

/// Whether a physical capability has an OS permission to speak of.
///
/// Written down per capability so a new variant cannot enter
/// [`PHYSICAL_DEVICE_CAPABILITIES`] without someone deciding what its
/// permission means — the failure this whole model exists to stop was exactly a
/// capability advertised with an imaginary permission it could never satisfy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionRequirement {
    /// The device must hold a real OS permission.
    OsPermission,
    /// There is no such permission; readiness is the whole story.
    None,
}

pub const PHYSICAL_CAPABILITY_SEMANTICS: &[(DeviceCapability, PermissionRequirement)] = &[
    (DeviceCapability::DeviceInfo, PermissionRequirement::None),
    (
        DeviceCapability::CameraCapture,
        PermissionRequirement::OsPermission,
    ),
    (
        DeviceCapability::MicrophoneCapture,
        PermissionRequirement::OsPermission,
    ),
    (
        DeviceCapability::LocationRead,
        PermissionRequirement::OsPermission,
    ),
    (
        DeviceCapability::NotificationPost,
        PermissionRequirement::OsPermission,
    ),
    // Sharing a screen is a per-session consent, not a stored permission: the
    // browser asks once, the user may stop at any moment, and nothing is
    // remembered afterwards. That is readiness, not permission.
    (DeviceCapability::ScreenCapture, PermissionRequirement::None),
    // No platform has an "may this app make a sound" permission. Autoplay
    // policy is a readiness state.
    (DeviceCapability::AudioPlayback, PermissionRequirement::None),
    (
        DeviceCapability::VoiceStream,
        PermissionRequirement::OsPermission,
    ),
];

/// What a physical capability's permission means, or `None` for a capability
/// that is not physical at all.
pub fn permission_requirement(capability: DeviceCapability) -> Option<PermissionRequirement> {
    PHYSICAL_CAPABILITY_SEMANTICS
        .iter()
        .find(|(value, _)| *value == capability)
        .map(|(_, requirement)| *requirement)
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
    /// Whether the device could act right now, per capability.
    ///
    /// A capability absent from this map reads as [`DeviceReadiness::Unavailable`]
    /// — fail closed. A surface stored by an older build carries no readiness at
    /// all, and that device's physical capabilities stay ineffective until it
    /// reconnects and says what it can do now. Missing security fields are never
    /// read as consent.
    #[serde(default)]
    pub readiness: BTreeMap<DeviceCapability, DeviceReadiness>,
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
        if self.capabilities.len() > 64 || self.permissions.len() > 64 || self.readiness.len() > 64
        {
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

    /// Fail-closed on purpose: an unstated readiness is not readiness.
    pub fn readiness(&self, capability: DeviceCapability) -> DeviceReadiness {
        self.readiness
            .get(&capability)
            .copied()
            .unwrap_or(DeviceReadiness::Unavailable)
    }
}

/// Why one capability is not effective, in the vocabulary the fix is written in.
///
/// A caller that only learns "device unavailable" cannot tell an operator what
/// to do; each of these maps to exactly one action, which is why they are not
/// collapsed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CapabilityBlock {
    NotGranted,
    /// The device has never told the runner what it is.
    NoSurface,
    Unsupported,
    PermissionRequired,
    PermissionDenied,
    ForegroundRequired,
    InteractionRequired,
    ScreenCaptureNotArmed,
    Unavailable,
}

impl CapabilityBlock {
    /// The wire token, so a caller can branch on the reason rather than parse
    /// the sentence.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NotGranted => "not_granted",
            Self::NoSurface => "no_surface",
            Self::Unsupported => "unsupported",
            Self::PermissionRequired => "permission_required",
            Self::PermissionDenied => "permission_denied",
            Self::ForegroundRequired => "foreground_required",
            Self::InteractionRequired => "interaction_required",
            Self::ScreenCaptureNotArmed => "screen_capture_not_armed",
            Self::Unavailable => "unavailable",
        }
    }

    /// What the operator has to do about it, named for the surface they would
    /// do it on.
    pub fn explain(self, capability: DeviceCapability) -> String {
        let name = capability_token(capability).replace('_', " ");
        match self {
            Self::NotGranted => format!(
                "'{name}' is not granted to this device. Grant it from the device's card in \
                 Settings, then retry."
            ),
            Self::NoSurface => format!(
                "This device has never advertised what it can do, so '{name}' cannot be used. \
                 Open the paired-device controller on the device once and retry."
            ),
            Self::Unsupported => {
                format!("This device's build cannot do '{name}' at all.")
            }
            Self::PermissionRequired => format!(
                "'{name}' is granted and supported, but this device has not granted the matching \
                 operating-system permission. Open the paired-device controller, allow it under \
                 Device readiness, then retry."
            ),
            Self::PermissionDenied => format!(
                "This device's operating system denies '{name}'. Allow it in the device's own \
                 system settings, then retry."
            ),
            Self::ForegroundRequired => format!(
                "'{name}' needs the paired-device controller open and in front on the device. \
                 Bring it to the foreground and retry."
            ),
            Self::InteractionRequired => format!(
                "'{name}' needs someone to enable it on the device first. Open the paired-device \
                 controller, tap the control under Device readiness, then retry."
            ),
            Self::ScreenCaptureNotArmed => format!(
                "'{name}' needs screen sharing to be armed on the device. Open the paired-device \
                 controller, allow screen capture, then retry."
            ),
            Self::Unavailable => format!("'{name}' is unavailable on this device right now."),
        }
    }
}

fn capability_token(capability: DeviceCapability) -> String {
    serde_json::to_value(capability)
        .ok()
        .and_then(|value| value.as_str().map(str::to_string))
        .unwrap_or_default()
}

/// The first thing standing between this device and the capability, or `None`
/// when nothing does.
///
/// The single place the four axes are judged; `effective_capabilities` is this
/// function applied to a set, so the operator's card, the tool's error message
/// and the lease check can never disagree about why something is refused.
pub fn capability_block(
    granted: &BTreeSet<DeviceCapability>,
    surface: Option<&DeviceSurface>,
    capability: DeviceCapability,
) -> Option<CapabilityBlock> {
    if !granted.contains(&capability) {
        return Some(CapabilityBlock::NotGranted);
    }
    if !capability.is_physical() {
        return None;
    }
    let Some(surface) = surface else {
        return Some(CapabilityBlock::NoSurface);
    };
    if !surface.capabilities.contains(&capability) {
        return Some(CapabilityBlock::Unsupported);
    }
    match surface.permission(capability) {
        OsPermission::Granted | OsPermission::NotRequired => {}
        OsPermission::Denied => return Some(CapabilityBlock::PermissionDenied),
        OsPermission::Unsupported => return Some(CapabilityBlock::Unsupported),
        OsPermission::Undetermined | OsPermission::Promptable => {
            return Some(CapabilityBlock::PermissionRequired)
        }
    }
    match surface.readiness(capability) {
        DeviceReadiness::Ready => None,
        DeviceReadiness::ForegroundRequired => Some(CapabilityBlock::ForegroundRequired),
        DeviceReadiness::InteractionRequired => Some(CapabilityBlock::InteractionRequired),
        DeviceReadiness::ArmedRequired => Some(CapabilityBlock::ScreenCaptureNotArmed),
        DeviceReadiness::Unavailable => Some(CapabilityBlock::Unavailable),
    }
}

/// `granted ∧ supported ∧ permission ∈ {granted, not_required} ∧ readiness ==
/// ready`, evaluated for the physical capabilities and only for them.
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
        .filter(|capability| capability_block(granted, surface, *capability).is_none())
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

/// Whether a grant is peer standing and nothing else.
///
/// A peer is the one kind of pairing with no control-plane scope: it may not
/// read a run, approve anything or touch the desktop, so requiring it to name
/// an action and a run id — as every controller pairing must — would force an
/// authority onto it that the whole design says it must not have.
pub fn is_peer_only(capabilities: &BTreeSet<DeviceCapability>) -> bool {
    !capabilities.is_empty()
        && capabilities.iter().all(|capability| {
            matches!(
                capability,
                DeviceCapability::PeerMessage
                    | DeviceCapability::PeerTaskRequest
                    | DeviceCapability::PeerArtifact
            )
        })
}

impl RemoteScopes {
    /// The scope rules, given what the pairing is actually for.
    ///
    /// Identical to [`Self::validate`] for every controller pairing. The one
    /// difference is a peer-only grant, which is allowed to carry an entirely
    /// empty control scope — the strongest possible restriction, not a
    /// loosening: an empty scope reaches nothing on the control plane.
    pub fn validate_with_capabilities(
        &self,
        capabilities: &BTreeSet<DeviceCapability>,
    ) -> Result<(), String> {
        // A capability set that is empty, or holds nothing but peer grants, is
        // authority over no run and no workspace — so the run-scope rules have
        // nothing to constrain and only the bounds apply. The empty case is not
        // a degenerate one: "paired, and may not ask for anything" is the state
        // an operator lands in by clearing a peer's grants, and refusing it here
        // made clearing them impossible.
        let peer_shaped = capabilities.is_empty() || is_peer_only(capabilities);
        if peer_shaped
            && self.actions.is_empty()
            && self.run_ids.is_empty()
            && self.workspace_ids.is_empty()
        {
            return self.validate_bounds();
        }
        self.validate()
    }

    /// Bounds that hold for every pairing, peer or controller.
    fn validate_bounds(&self) -> Result<(), String> {
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
        Ok(())
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.actions.is_empty() {
            return Err("Remote pairing requires at least one action".to_string());
        }
        if self.run_ids.is_empty() && self.workspace_ids.is_empty() {
            return Err(
                "Remote pairing requires an exact run id or declared workspace id".to_string(),
            );
        }
        self.validate_bounds()?;
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
        let capabilities = if self.capabilities.is_empty() {
            legacy_capabilities(&self.scopes)
        } else {
            self.capabilities.clone()
        };
        self.scopes.validate_with_capabilities(&capabilities)?;
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
    /// When this peer last *answered*, not when it was last asked. Written only
    /// by a successful `peers hello`, so a failed probe never reads as contact.
    #[serde(default)]
    pub last_seen_at_ms: Option<u64>,
    /// What the far side says it can accept from peers. A claim it made about
    /// itself, kept separate from [`Self::capabilities`] — which is what it
    /// actually granted this installation — for the same reason the answering
    /// side keeps them apart.
    #[serde(default)]
    pub peer_advertised: BTreeSet<DeviceCapability>,
    /// What this installation asked the far side to grant it. Recorded so the
    /// Peers screen can show an ask that has not been answered yet.
    #[serde(default)]
    pub peer_requested: BTreeSet<DeviceCapability>,
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
    #[serde(default)]
    pub allowed_windows: Vec<String>,
    #[serde(default)]
    pub lifetime_ms: Option<u64>,
    #[serde(default)]
    pub allow_screenshots: Option<bool>,
    #[serde(default)]
    pub allow_keyboard_input: Option<bool>,
    #[serde(default)]
    pub allow_clipboard_read: Option<bool>,
    #[serde(default)]
    pub approval_policy: Option<little_monkey_lib::desktop_control::ApprovalPolicy>,
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
    #[serde(default)]
    pub target_window_id: Option<String>,
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
    /// What the device says the artifact's bytes hash to. Checked against the
    /// bytes actually received, so a truncated upload is refused rather than
    /// stored as authoritative.
    #[serde(default)]
    pub artifact_sha256: Option<String>,
    #[serde(default)]
    pub error: Option<String>,
    /// The execution this report belongs to, matching the one `start`
    /// authorized. Present, it is what tells a replay from a contradiction.
    #[serde(default)]
    pub execution_id: Option<String>,
}

/// The identity of one terminal report — outcome, result and artifact digest.
///
/// Retrying a delivery must be accepted; contradicting one must not. Comparing
/// this digest is how the runner tells them apart without keeping a second copy
/// of the report to compare field by field.
pub fn terminal_digest(
    outcome: DeviceCommandState,
    result: Option<&serde_json::Value>,
    artifact_sha256: Option<&str>,
    error: Option<&str>,
) -> String {
    let canonical = serde_json::json!({
        "outcome": outcome.as_str(),
        "result": result,
        "artifact_sha256": artifact_sha256,
        "error": error,
    });
    sha256_hex(canonical.to_string().as_bytes())
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
        if let Some(digest) = &self.artifact_sha256 {
            validate_sha256(digest)?;
            if self.artifact_base64.is_none() {
                return Err("An artifact digest was declared with no artifact".to_string());
            }
        }
        if let Some(execution_id) = &self.execution_id {
            validate_id(execution_id)?;
        }
        if self.error.as_ref().is_some_and(|error| error.len() > 4_096) {
            return Err("Device command error text exceeds 4 KiB".to_string());
        }
        Ok(())
    }
}

/// Body of `POST /v1/remote/device/commands/{id}/start` — the device saying it
/// is about to touch hardware.
///
/// The `execution_id` is the device's own durable name for *this attempt*,
/// minted and journalled before the request is sent. It is what makes a
/// reconnect distinguishable from a second device: the same id back is the same
/// attempt resuming and is answered `started: false, recoverable: true`; a
/// different id is refused outright, because two executions of one physical
/// command is the failure this whole design exists to prevent. Absent (an older
/// client) it degrades to the previous behaviour, which is safe but cannot tell
/// resumption from intrusion.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeviceCommandStartRequest {
    #[serde(default)]
    pub execution_id: Option<String>,
}

impl DeviceCommandStartRequest {
    pub fn validate(&self) -> Result<(), String> {
        match &self.execution_id {
            None => Ok(()),
            Some(value) if value.len() >= 8 => validate_id(value),
            Some(_) => Err("An execution id must be at least 8 characters".to_string()),
        }
    }
}

/// One nonterminal command the runner still believes this device owns, returned
/// by `GET /v1/remote/device/commands/recover`.
///
/// Deliberately not a lease: a `running` command handed back through the
/// ordinary queue would be a second execution. This says "you started this and
/// never finished it" and leaves what to do about it to the device's own
/// journal — deliver the staged result, or report the outcome unknown.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeviceCommandRecovery {
    pub command_id: String,
    pub capability: DeviceCapability,
    pub arguments_sha256: String,
    pub state: DeviceCommandState,
    /// The execution the runner authorized, when it recorded one.
    #[serde(default)]
    pub execution_id: Option<String>,
    pub started_at_ms: Option<u64>,
    pub expires_at_ms: u64,
    pub cancel_requested: bool,
}

/// The answer to `GET /v1/remote/device/commands/{id}/control` — a running
/// command's control signals, on a request the device makes once rather than a
/// poll it repeats.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeviceCommandControl {
    pub protocol_version: u32,
    pub command_id: String,
    pub state: DeviceCommandState,
    pub cancel_requested: bool,
    /// The pairing itself was revoked or the grant withdrawn: stop, and do not
    /// bother reporting.
    pub revoked: bool,
    /// When this command stops being worth finishing.
    pub deadline_ms: u64,
}

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

// --- Realtime Talk --------------------------------------------------------

/// Audio formats accepted on the dedicated Talk WebSocket. The capture
/// formats match the existing voice upload surface; MPEG is also accepted for
/// speech output produced by a configured TTS backend.
pub const TALK_MEDIA_TYPES: &[&str] = &[
    "audio/webm",
    "audio/webm;codecs=opus",
    "audio/ogg",
    "audio/ogg;codecs=opus",
    "audio/mp4",
    "audio/wav",
    "audio/mpeg",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TalkState {
    Idle,
    Starting,
    Listening,
    Transcribing,
    Thinking,
    Speaking,
    Interrupted,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TalkTicketRequest {
    pub protocol_version: u32,
    pub session_id: String,
}

impl TalkTicketRequest {
    pub fn validate(&self) -> Result<(), String> {
        validate_talk_protocol_version(self.protocol_version)?;
        validate_talk_session_id(&self.session_id)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TalkTicketResponse {
    pub protocol_version: u32,
    pub session_id: String,
    pub session_generation: String,
    pub ticket: String,
    pub expires_at_ms: u64,
    /// Does not contain the bearer ticket. Clients append it as the `ticket`
    /// query parameter immediately before opening the WebSocket.
    pub websocket_path: String,
}

impl TalkTicketResponse {
    pub fn issue(session_id: impl Into<String>, now_ms: u64, ttl_ms: u64) -> Result<Self, String> {
        if ttl_ms == 0 || ttl_ms > MAX_TALK_TICKET_TTL_MS {
            return Err(format!(
                "Talk ticket lifetime must be between 1 and {MAX_TALK_TICKET_TTL_MS} ms"
            ));
        }
        let session_id = session_id.into();
        validate_talk_session_id(&session_id)?;
        let expires_at_ms = now_ms
            .checked_add(ttl_ms)
            .ok_or_else(|| "Talk ticket expiry overflowed".to_string())?;
        let response = Self {
            protocol_version: TALK_PROTOCOL_VERSION,
            websocket_path: format!("/v1/remote/device/talk/{session_id}/stream"),
            session_id,
            session_generation: random_token(TALK_SESSION_GENERATION_RANDOM_BYTES)?,
            ticket: random_token(TALK_TICKET_RANDOM_BYTES)?,
            expires_at_ms,
        };
        response.validate(now_ms)?;
        Ok(response)
    }

    pub fn validate(&self, now_ms: u64) -> Result<(), String> {
        validate_talk_protocol_version(self.protocol_version)?;
        validate_talk_session_id(&self.session_id)?;
        validate_talk_session_generation(&self.session_generation)?;
        validate_talk_token("ticket", &self.ticket, 32, MAX_TALK_TICKET_BYTES)?;
        if self.expires_at_ms <= now_ms {
            return Err("Talk ticket has expired".to_string());
        }
        if self.expires_at_ms.saturating_sub(now_ms) > MAX_TALK_TICKET_TTL_MS {
            return Err("Talk ticket expiry exceeds the maximum lifetime".to_string());
        }
        let expected_path = format!("/v1/remote/device/talk/{}/stream", self.session_id);
        if self.websocket_path != expected_path {
            return Err("Talk WebSocket path does not match its session".to_string());
        }
        Ok(())
    }
}

/// Envelope shared by every device-to-runner Talk frame. The generation is a
/// random token issued with the one-use ticket; sequences restart only when a
/// new generation is issued.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TalkClientFrame {
    pub protocol_version: u32,
    pub session_id: String,
    pub session_generation: String,
    pub frame_sequence: u64,
    #[serde(flatten)]
    pub kind: TalkClientFrameKind,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum TalkClientFrameKind {
    Hello {
        media_type: String,
        sample_rate_hz: u32,
        channels: u8,
    },
    Audio {
        audio_sequence: u64,
        media_type: String,
        audio_base64: String,
        /// Last chunk of this utterance. The device's own local voice activity
        /// detector decides where an utterance ends — the runner never guesses
        /// from silence it cannot hear — so this flag is what hands one
        /// complete recording over to transcription.
        #[serde(default)]
        last: bool,
        /// The device's own durable name for this utterance, required on the
        /// frame that closes one (`last: true`) and ignored on the others.
        ///
        /// # Why the device has to name it
        ///
        /// This is the idempotency key the turn is queued under, and the runner
        /// cannot mint one that survives a restart. The obvious server-side
        /// identity — the session generation plus an utterance counter — is
        /// per *socket*: a generation is minted fresh with every ticket, and
        /// the counter restarts at zero. So a daemon that restarts mid-turn,
        /// and a device that reconnects and retransmits the recording it never
        /// got an answer for, would produce a second key and a second run — and
        /// a Talk turn can send a message or place a call, so that is a
        /// duplicated external effect, not just a duplicated answer.
        ///
        /// Keying on `(session_id, utterance_index)` instead would be worse: the
        /// counter resets, so the first utterance of a reconnected session would
        /// collide with the first of the old one and two different things
        /// somebody said would merge into one.
        ///
        /// Only the device knows that the audio it is sending now is the audio
        /// it sent before, so only the device can name it. Required rather than
        /// optional-with-a-fallback, because a fallback is the hole: a client
        /// that omitted it would silently get the unkeyed behaviour back.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        utterance_id: Option<String>,
    },
    State {
        state: TalkState,
    },
    Interrupt {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
    /// What the device's half of one utterance cost, in milliseconds.
    ///
    /// The runner can time everything from transcription onwards itself, but
    /// the three spans before that happen on the phone and are invisible here.
    /// They ride their own frame, and they are durations — carrying the
    /// transcript or the audio on a telemetry frame is exactly the mistake this
    /// shape exists to make impossible.
    Metrics {
        /// The `audio_sequence` of the first audio frame of the utterance these
        /// spans describe. Required, and required *before* that frame is sent:
        /// the runner answers the moment an utterance closes, so a metrics frame
        /// that arrives after it would otherwise be filed against the next one.
        audio_sequence: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        speech_detection_ms: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        capture_ms: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        upload_ms: Option<u64>,
    },
}

impl TalkClientFrame {
    pub fn validate(&self) -> Result<(), String> {
        validate_talk_envelope(
            self.protocol_version,
            &self.session_id,
            &self.session_generation,
            self.frame_sequence,
        )?;
        match &self.kind {
            TalkClientFrameKind::Hello {
                media_type,
                sample_rate_hz,
                channels,
            } => {
                validate_talk_media_type(media_type)?;
                if !(8_000..=192_000).contains(sample_rate_hz) {
                    return Err("Talk sample rate must be between 8000 and 192000 Hz".to_string());
                }
                if !(1..=2).contains(channels) {
                    return Err("Talk audio must have one or two channels".to_string());
                }
            }
            TalkClientFrameKind::Audio {
                audio_sequence,
                media_type,
                audio_base64,
                last,
                utterance_id,
            } => {
                validate_talk_audio_sequence(*audio_sequence)?;
                validate_talk_media_type(media_type)?;
                validate_talk_audio(audio_base64)?;
                // Only the closing frame queues a turn, so only the closing
                // frame needs the key it is queued under. Refused rather than
                // defaulted: a fallback identity is the hole this field exists
                // to close, and it would be invisible -- the turn would run,
                // and only a restart would show that it ran twice.
                match (last, utterance_id.as_deref()) {
                    (true, None) => {
                        return Err(
                            "A Talk utterance must carry an utterance_id on its final audio frame"
                                .to_string(),
                        )
                    }
                    (true, Some(utterance_id)) => validate_talk_utterance_id(utterance_id)?,
                    // Ignored on a mid-utterance frame rather than refused: a
                    // device that stamps every chunk is not doing anything
                    // wrong, and the closing frame is what is read.
                    (false, _) => {}
                }
            }
            TalkClientFrameKind::State { .. } => {}
            TalkClientFrameKind::Interrupt { reason } => {
                if let Some(reason) = reason {
                    validate_talk_text("interrupt reason", reason, MAX_TALK_ERROR_BYTES)?;
                }
            }
            TalkClientFrameKind::Metrics {
                audio_sequence,
                speech_detection_ms,
                capture_ms,
                upload_ms,
            } => {
                if *audio_sequence == 0 {
                    return Err("Talk metrics must name the utterance they measure".to_string());
                }
                for span in [speech_detection_ms, capture_ms, upload_ms]
                    .into_iter()
                    .flatten()
                {
                    if *span > MAX_TALK_LATENCY_MS {
                        return Err(format!(
                            "Talk latency spans must be at most {MAX_TALK_LATENCY_MS} ms"
                        ));
                    }
                }
            }
        }
        validate_talk_frame_size(self)
    }

    pub fn audio_sequence(&self) -> Option<u64> {
        match &self.kind {
            TalkClientFrameKind::Audio { audio_sequence, .. } => Some(*audio_sequence),
            _ => None,
        }
    }
}

/// Envelope shared by every runner-to-device Talk frame.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TalkServerFrame {
    pub protocol_version: u32,
    pub session_id: String,
    pub session_generation: String,
    pub frame_sequence: u64,
    #[serde(flatten)]
    pub kind: TalkServerFrameKind,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum TalkServerFrameKind {
    Ready,
    State {
        state: TalkState,
    },
    /// One utterance now exists as a durable turn, and the device may delete
    /// the recording it has been holding.
    ///
    /// # Why this is not "we received your audio"
    ///
    /// Three different moments could plausibly acknowledge an utterance, and
    /// only one of them is safe to forget a recording on:
    ///
    ///   audio received  ≠  transcription completed  ≠  durable turn accepted
    ///
    /// A crash after the first two leaves nothing: no row, no job, no run. A
    /// device that had deleted its recording would have lost what somebody
    /// said, with no way to know it had. This frame is emitted only after
    /// [`TalkTurns::submit`](super::talk::TalkTurns::submit) has returned — the
    /// user row is written and the job is queued under the utterance's own
    /// idempotency key — so from here on a re-send is not merely safe, it is
    /// unnecessary: it would collapse onto `run_id`.
    ///
    /// `run_id` is carried so a device that loses the socket before the answer
    /// arrives can recover through the durable conversation rather than by
    /// speaking again.
    TurnAccepted {
        utterance_id: String,
        run_id: String,
    },
    Transcript {
        text: String,
        is_final: bool,
    },
    AssistantDelta {
        text: String,
    },
    OutputAudio {
        audio_sequence: u64,
        media_type: String,
        audio_base64: String,
    },
    Error {
        code: String,
        message: String,
        retryable: bool,
    },
}

impl TalkServerFrame {
    pub fn validate(&self) -> Result<(), String> {
        validate_talk_envelope(
            self.protocol_version,
            &self.session_id,
            &self.session_generation,
            self.frame_sequence,
        )?;
        match &self.kind {
            TalkServerFrameKind::Ready | TalkServerFrameKind::State { .. } => {}
            TalkServerFrameKind::TurnAccepted {
                utterance_id,
                run_id,
            } => {
                validate_talk_utterance_id(utterance_id)?;
                // `validate_id` rather than the stricter Talk token alphabet: a
                // run id is minted by the queue, not by this protocol, and it
                // may carry `.` or `:`. Refusing one here would turn a
                // successfully accepted turn into a dead socket.
                validate_id(run_id)?;
            }
            TalkServerFrameKind::Transcript { text, .. }
            | TalkServerFrameKind::AssistantDelta { text } => {
                validate_talk_text("Talk text", text, MAX_TALK_TEXT_BYTES)?;
            }
            TalkServerFrameKind::OutputAudio {
                audio_sequence,
                media_type,
                audio_base64,
            } => {
                validate_talk_audio_sequence(*audio_sequence)?;
                validate_talk_media_type(media_type)?;
                validate_talk_audio(audio_base64)?;
            }
            TalkServerFrameKind::Error { code, message, .. } => {
                if code.is_empty()
                    || code.len() > 128
                    || !code.bytes().all(|byte| {
                        byte.is_ascii_lowercase()
                            || byte.is_ascii_digit()
                            || matches!(byte, b'_' | b'-' | b'.')
                    })
                {
                    return Err("Talk error code is invalid".to_string());
                }
                validate_talk_text("Talk error message", message, MAX_TALK_ERROR_BYTES)?;
            }
        }
        validate_talk_frame_size(self)
    }
}

/// Stateful replay guard for one direction of one session generation. Use one
/// tracker for client frames and another for server frames.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TalkSequenceTracker {
    last_frame_sequence: u64,
    last_audio_sequence: u64,
}

impl TalkSequenceTracker {
    pub fn accept(
        &mut self,
        frame_sequence: u64,
        audio_sequence: Option<u64>,
    ) -> Result<(), String> {
        if frame_sequence == 0 || frame_sequence <= self.last_frame_sequence {
            return Err(format!(
                "Talk frame sequence must increase beyond {}",
                self.last_frame_sequence
            ));
        }
        if let Some(audio_sequence) = audio_sequence {
            if audio_sequence == 0 || audio_sequence <= self.last_audio_sequence {
                return Err(format!(
                    "Talk audio sequence must increase beyond {}",
                    self.last_audio_sequence
                ));
            }
        }
        self.last_frame_sequence = frame_sequence;
        if let Some(audio_sequence) = audio_sequence {
            self.last_audio_sequence = audio_sequence;
        }
        Ok(())
    }

    pub fn last_frame_sequence(&self) -> u64 {
        self.last_frame_sequence
    }

    pub fn last_audio_sequence(&self) -> u64 {
        self.last_audio_sequence
    }
}

fn validate_talk_protocol_version(protocol_version: u32) -> Result<(), String> {
    if protocol_version == TALK_PROTOCOL_VERSION {
        return Ok(());
    }
    // A page that was already open when this runner was upgraded. Named
    // exactly, because the fix is one the person can perform and an opaque
    // "unsupported version" would not tell them to.
    if matches!(
        protocol_version,
        TALK_PROTOCOL_VERSION_WITHOUT_UTTERANCE_ID | TALK_PROTOCOL_VERSION_WITHOUT_ACCEPTANCE
    ) {
        return Err(
            "This Talk client is from an older version of the app; reload the page to continue"
                .to_string(),
        );
    }
    Err(format!(
        "Unsupported Talk protocol version {protocol_version}"
    ))
}

fn validate_talk_session_id(session_id: &str) -> Result<(), String> {
    if session_id.len() > MAX_TALK_SESSION_ID_BYTES {
        return Err(format!(
            "Talk session id exceeds {MAX_TALK_SESSION_ID_BYTES} bytes"
        ));
    }
    validate_id(session_id)
}

/// The device's own name for one utterance.
///
/// Bounded and character-restricted like every other identifier that crosses
/// this boundary, because it becomes part of a durable key: it reaches
/// `submit_conversation_turn` as the client key and ends up in a job id.
/// Deliberately *not* required to be base64 or to carry entropy — a device that
/// names its utterances `1`, `2`, `3` within a session is behaving correctly,
/// and the value is scoped to the session it arrived on.
fn validate_talk_utterance_id(utterance_id: &str) -> Result<(), String> {
    validate_talk_token("utterance id", utterance_id, 1, MAX_TALK_UTTERANCE_ID_BYTES)
}

fn validate_talk_session_generation(session_generation: &str) -> Result<(), String> {
    validate_talk_token(
        "session generation",
        session_generation,
        16,
        MAX_TALK_SESSION_GENERATION_BYTES,
    )?;
    let decoded = URL_SAFE_NO_PAD
        .decode(session_generation)
        .map_err(|_| "Talk session generation is not URL-safe base64".to_string())?;
    if decoded.len() != TALK_SESSION_GENERATION_RANDOM_BYTES {
        return Err("Talk session generation has the wrong entropy length".to_string());
    }
    Ok(())
}

fn validate_talk_token(
    label: &str,
    value: &str,
    min_bytes: usize,
    max_bytes: usize,
) -> Result<(), String> {
    if value.len() < min_bytes
        || value.len() > max_bytes
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err(format!("Talk {label} is invalid"));
    }
    Ok(())
}

fn validate_talk_envelope(
    protocol_version: u32,
    session_id: &str,
    session_generation: &str,
    frame_sequence: u64,
) -> Result<(), String> {
    validate_talk_protocol_version(protocol_version)?;
    validate_talk_session_id(session_id)?;
    validate_talk_session_generation(session_generation)?;
    if frame_sequence == 0 {
        return Err("Talk frame sequence must be positive".to_string());
    }
    Ok(())
}

fn validate_talk_audio_sequence(audio_sequence: u64) -> Result<(), String> {
    if audio_sequence == 0 {
        return Err("Talk audio sequence must be positive".to_string());
    }
    Ok(())
}

fn validate_talk_media_type(media_type: &str) -> Result<(), String> {
    if media_type.len() > MAX_TALK_MEDIA_TYPE_BYTES || !TALK_MEDIA_TYPES.contains(&media_type) {
        return Err(format!("Unsupported Talk media type '{media_type}'"));
    }
    Ok(())
}

fn validate_talk_audio(audio_base64: &str) -> Result<(), String> {
    if audio_base64.is_empty() {
        return Err("Talk audio payload is empty".to_string());
    }
    if audio_base64.len() > MAX_TALK_AUDIO_BASE64_BYTES {
        return Err(format!(
            "Talk audio payload exceeds {MAX_TALK_AUDIO_BYTES} decoded bytes"
        ));
    }
    let decoded = STANDARD
        .decode(audio_base64)
        .map_err(|_| "Talk audio payload is not valid base64".to_string())?;
    if decoded.len() > MAX_TALK_AUDIO_BYTES {
        return Err(format!(
            "Talk audio payload exceeds {MAX_TALK_AUDIO_BYTES} decoded bytes"
        ));
    }
    Ok(())
}

fn validate_talk_text(label: &str, value: &str, max_bytes: usize) -> Result<(), String> {
    if value.is_empty() || value.len() > max_bytes {
        return Err(format!(
            "{label} must contain between 1 and {max_bytes} bytes"
        ));
    }
    Ok(())
}

fn validate_talk_frame_size(frame: &impl Serialize) -> Result<(), String> {
    let bytes = serde_json::to_vec(frame)
        .map_err(|error| format!("Talk frame cannot be serialized: {error}"))?;
    if bytes.len() > MAX_TALK_FRAME_BYTES {
        return Err(format!("Talk frame exceeds {MAX_TALK_FRAME_BYTES} bytes"));
    }
    Ok(())
}

// --- Peer plane -----------------------------------------------------------

/// Most bytes one peer may hand another in a single artifact upload. Mirrors
/// [`little_monkey_lib::peers::MAX_PEER_ARTIFACT_BYTES`], which is what an
/// envelope may *claim*; this is what the transport will actually take.
pub const MAX_PEER_ARTIFACT_BYTES: usize =
    little_monkey_lib::peers::MAX_PEER_ARTIFACT_BYTES as usize;

/// One peer introducing itself to another.
///
/// Deliberately says nothing a gate reads. `advertised` and `requested` are
/// claims the receiving operator sees on the Peers screen and acts on or
/// ignores; nothing here can change what the sender is allowed to do, which is
/// why a peer may call this with any peer grant at all.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PeerHelloRequest {
    pub protocol_version: u32,
    /// The caller's own instance id, for display and loop diagnostics. Identity
    /// remains the signed device credential; this is never trusted for it.
    pub instance_id: String,
    #[serde(default)]
    pub advertised: BTreeSet<DeviceCapability>,
    #[serde(default)]
    pub requested: BTreeSet<DeviceCapability>,
}

impl PeerHelloRequest {
    pub fn validate(&self) -> Result<(), String> {
        if self.protocol_version != REMOTE_PROTOCOL_VERSION {
            return Err(format!(
                "Unsupported remote protocol version {}",
                self.protocol_version
            ));
        }
        validate_peer_instance_id(&self.instance_id)?;
        for (label, claimed) in [
            ("advertised", &self.advertised),
            ("requested", &self.requested),
        ] {
            if !claimed.is_empty() && !is_peer_only(claimed) {
                return Err(format!("A peer may only {label} peer capabilities"));
            }
        }
        Ok(())
    }
}

/// The answer: who this installation is, what time it thinks it is, and what
/// the caller may actually do here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PeerHelloResponse {
    pub protocol_version: u32,
    pub instance_id: String,
    pub now_ms: u64,
    /// Every peer capability this build understands. Not a promise to grant
    /// any of them.
    pub advertised: BTreeSet<DeviceCapability>,
    /// What the caller is granted here, right now. The one field in this
    /// response that is authoritative — and it is authoritative because the
    /// answering side computed it from its own pairing record.
    pub granted: BTreeSet<DeviceCapability>,
}

impl PeerHelloResponse {
    pub fn validate(&self) -> Result<(), String> {
        if self.protocol_version != REMOTE_PROTOCOL_VERSION {
            return Err(format!(
                "Unsupported remote protocol version {}",
                self.protocol_version
            ));
        }
        validate_peer_instance_id(&self.instance_id)?;
        for (label, claimed) in [("advertise", &self.advertised), ("grant", &self.granted)] {
            if !claimed.is_empty() && !is_peer_only(claimed) {
                return Err(format!("A peer may only {label} peer capabilities"));
            }
        }
        Ok(())
    }
}

/// Content a peer hands over *before* the envelope that references it.
///
/// Push rather than pull, and that is the whole design: a receiver that had to
/// fetch would need an outbound pairing back to every peer that ever wrote to
/// it. The sender already has one, so the sender carries the bytes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PeerArtifactUpload {
    pub protocol_version: u32,
    /// SHA-256 the sender claims. The receiver hashes what it decoded and
    /// refuses a mismatch, so this is a checksum, never an identifier it
    /// trusts.
    pub sha256: String,
    pub content_base64: String,
    #[serde(default)]
    pub filename: Option<String>,
    #[serde(default)]
    pub media_type: Option<String>,
}

impl PeerArtifactUpload {
    pub fn validate(&self) -> Result<(), String> {
        if self.protocol_version != REMOTE_PROTOCOL_VERSION {
            return Err(format!(
                "Unsupported remote protocol version {}",
                self.protocol_version
            ));
        }
        if self.sha256.len() != 64 || !self.sha256.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err("A peer artifact digest must be 64 hex characters".to_string());
        }
        if self.content_base64.is_empty() {
            return Err("A peer artifact carries no content".to_string());
        }
        // Refused on the encoded length, before anything is decoded.
        if self.content_base64.len() > MAX_PEER_ARTIFACT_BYTES * 4 / 3 + 4 {
            return Err(format!(
                "A peer artifact may carry at most {MAX_PEER_ARTIFACT_BYTES} bytes"
            ));
        }
        if let Some(filename) = &self.filename {
            if filename.trim().is_empty()
                || filename.len() > little_monkey_lib::peers::MAX_ARTIFACT_FILENAME_BYTES
                || filename == "."
                || filename == ".."
                || filename.contains(['/', '\\'])
                || filename.chars().any(char::is_control)
            {
                return Err("A peer artifact filename is invalid".to_string());
            }
        }
        if let Some(media_type) = &self.media_type {
            if media_type.trim().is_empty()
                || media_type.len() > little_monkey_lib::peers::MAX_ARTIFACT_MEDIA_TYPE_BYTES
                || !media_type.is_ascii()
                || media_type.chars().any(char::is_control)
            {
                return Err("A peer artifact media type is invalid".to_string());
            }
        }
        Ok(())
    }
}

/// What the receiver stored. The id is the content digest, so both sides name
/// the same bytes without either trusting the other's naming.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PeerArtifactStored {
    pub artifact_id: String,
    pub sha256: String,
    pub size_bytes: u64,
}

/// An instance id as it may appear on the peer plane: the same alphabet the
/// envelope accepts, so a value that passes here passes there.
fn validate_peer_instance_id(value: &str) -> Result<(), String> {
    if value.is_empty() || value.len() > little_monkey_lib::peers::MAX_ID_LEN {
        return Err("A peer instance id must be 1-128 characters".to_string());
    }
    if !value
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | ':'))
    {
        return Err("A peer instance id contains an unsupported character".to_string());
    }
    Ok(())
}

/// Every peer capability this build understands, for a hello response.
pub fn all_peer_capabilities() -> BTreeSet<DeviceCapability> {
    BTreeSet::from([
        DeviceCapability::PeerMessage,
        DeviceCapability::PeerTaskRequest,
        DeviceCapability::PeerArtifact,
    ])
}

/// Just the peer grants out of a capability set.
pub fn peer_capabilities_of(
    capabilities: &BTreeSet<DeviceCapability>,
) -> BTreeSet<DeviceCapability> {
    capabilities
        .iter()
        .copied()
        .filter(|capability| {
            matches!(
                capability,
                DeviceCapability::PeerMessage
                    | DeviceCapability::PeerTaskRequest
                    | DeviceCapability::PeerArtifact
            )
        })
        .collect()
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
        readiness: &[(DeviceCapability, DeviceReadiness)],
    ) -> DeviceSurface {
        DeviceSurface {
            protocol_version: REMOTE_PROTOCOL_VERSION,
            platform: "ios".into(),
            platform_version: "18.2".into(),
            app_version: "1.3.0".into(),
            device_model: "iPhone".into(),
            capabilities: capabilities.iter().copied().collect(),
            permissions: permissions.iter().copied().collect(),
            readiness: readiness.iter().copied().collect(),
            constraints: DeviceConstraints::default(),
            reported_at_ms: 1_000,
        }
    }

    /// The rule the whole physical-device surface rests on: four axes have to
    /// agree, and any one of them saying no is enough.
    #[test]
    fn effective_authority_is_grant_support_permission_and_readiness() {
        let granted = BTreeSet::from([
            DeviceCapability::ViewRuns,
            DeviceCapability::CameraCapture,
            DeviceCapability::LocationRead,
            DeviceCapability::ScreenCapture,
            DeviceCapability::NotificationPost,
            DeviceCapability::AudioPlayback,
        ]);
        let advertised = surface(
            &[
                DeviceCapability::CameraCapture,
                DeviceCapability::LocationRead,
                DeviceCapability::NotificationPost,
                DeviceCapability::ScreenCapture,
                DeviceCapability::AudioPlayback,
            ],
            &[
                (DeviceCapability::CameraCapture, OsPermission::Granted),
                (DeviceCapability::LocationRead, OsPermission::Denied),
                // NotificationPost advertised but never asked for: promptable
                // is not permission.
                (DeviceCapability::NotificationPost, OsPermission::Promptable),
                // Screen capture and audio playback need no OS permission at
                // all; readiness is what decides them.
                (DeviceCapability::ScreenCapture, OsPermission::NotRequired),
                (DeviceCapability::AudioPlayback, OsPermission::NotRequired),
            ],
            &[
                (DeviceCapability::CameraCapture, DeviceReadiness::Ready),
                (DeviceCapability::LocationRead, DeviceReadiness::Ready),
                (DeviceCapability::NotificationPost, DeviceReadiness::Ready),
                // Nobody has shared a screen, so it is armable, not ready.
                (
                    DeviceCapability::ScreenCapture,
                    DeviceReadiness::ArmedRequired,
                ),
                (DeviceCapability::AudioPlayback, DeviceReadiness::Ready),
            ],
        );
        let effective = effective_capabilities(&granted, Some(&advertised));
        assert_eq!(
            effective,
            BTreeSet::from([
                DeviceCapability::ViewRuns,
                DeviceCapability::CameraCapture,
                DeviceCapability::AudioPlayback,
            ]),
            "only a capability granted, advertised, permitted and ready is effective"
        );
        // Each refusal keeps its own reason rather than collapsing to one.
        assert_eq!(
            capability_block(&granted, Some(&advertised), DeviceCapability::LocationRead),
            Some(CapabilityBlock::PermissionDenied)
        );
        assert_eq!(
            capability_block(
                &granted,
                Some(&advertised),
                DeviceCapability::NotificationPost
            ),
            Some(CapabilityBlock::PermissionRequired)
        );
        assert_eq!(
            capability_block(&granted, Some(&advertised), DeviceCapability::ScreenCapture),
            Some(CapabilityBlock::ScreenCaptureNotArmed)
        );
        // A capability the OS permits but the operator never granted stays out.
        let ungranted = effective_capabilities(
            &BTreeSet::from([DeviceCapability::ViewRuns]),
            Some(&surface(
                &[DeviceCapability::CameraCapture],
                &[(DeviceCapability::CameraCapture, OsPermission::Granted)],
                &[(DeviceCapability::CameraCapture, DeviceReadiness::Ready)],
            )),
        );
        assert_eq!(ungranted, BTreeSet::from([DeviceCapability::ViewRuns]));
    }

    /// The whole matrix, for every physical capability, so a future one cannot
    /// enter the enum with undefined permission/readiness semantics.
    #[test]
    fn every_physical_capability_answers_the_complete_permission_matrix() {
        for capability in PHYSICAL_DEVICE_CAPABILITIES {
            let capability = *capability;
            let requirement = permission_requirement(capability).unwrap_or_else(|| {
                panic!(
                    "'{capability:?}' is physical but has no entry in \
                     PHYSICAL_CAPABILITY_SEMANTICS — decide what its OS permission means"
                )
            });
            let granted = BTreeSet::from([capability]);
            let sufficient = match requirement {
                PermissionRequirement::OsPermission => OsPermission::Granted,
                PermissionRequirement::None => OsPermission::NotRequired,
            };
            let ready = surface(
                &[capability],
                &[(capability, sufficient)],
                &[(capability, DeviceReadiness::Ready)],
            );

            // grant = no
            assert_eq!(
                capability_block(&BTreeSet::new(), Some(&ready), capability),
                Some(CapabilityBlock::NotGranted)
            );
            // support = no
            assert_eq!(
                capability_block(
                    &granted,
                    Some(&surface(
                        &[],
                        &[(capability, sufficient)],
                        &[(capability, DeviceReadiness::Ready)]
                    )),
                    capability
                ),
                Some(CapabilityBlock::Unsupported)
            );
            // no surface at all
            assert_eq!(
                capability_block(&granted, None, capability),
                Some(CapabilityBlock::NoSurface)
            );
            // permission = denied
            assert_eq!(
                capability_block(
                    &granted,
                    Some(&surface(
                        &[capability],
                        &[(capability, OsPermission::Denied)],
                        &[(capability, DeviceReadiness::Ready)]
                    )),
                    capability
                ),
                Some(CapabilityBlock::PermissionDenied)
            );
            // permission = promptable, and the legacy spelling of it
            for pending in [OsPermission::Promptable, OsPermission::Undetermined] {
                assert_eq!(
                    capability_block(
                        &granted,
                        Some(&surface(
                            &[capability],
                            &[(capability, pending)],
                            &[(capability, DeviceReadiness::Ready)]
                        )),
                        capability
                    ),
                    Some(CapabilityBlock::PermissionRequired),
                    "'{capability:?}' with a {pending:?} permission must not be effective"
                );
            }
            // readiness != ready, in each of its shapes
            for (readiness, expected) in [
                (
                    DeviceReadiness::ForegroundRequired,
                    CapabilityBlock::ForegroundRequired,
                ),
                (
                    DeviceReadiness::InteractionRequired,
                    CapabilityBlock::InteractionRequired,
                ),
                (
                    DeviceReadiness::ArmedRequired,
                    CapabilityBlock::ScreenCaptureNotArmed,
                ),
                (DeviceReadiness::Unavailable, CapabilityBlock::Unavailable),
            ] {
                assert_eq!(
                    capability_block(
                        &granted,
                        Some(&surface(
                            &[capability],
                            &[(capability, sufficient)],
                            &[(capability, readiness)]
                        )),
                        capability
                    ),
                    Some(expected)
                );
            }
            // A surface that says nothing about readiness fails closed — this
            // is what an upgrade from a build without the field looks like.
            assert_eq!(
                capability_block(
                    &granted,
                    Some(&surface(&[capability], &[(capability, sufficient)], &[])),
                    capability
                ),
                Some(CapabilityBlock::Unavailable),
                "'{capability:?}' with no stated readiness must fail closed"
            );
            // everything satisfied
            assert_eq!(capability_block(&granted, Some(&ready), capability), None);
            assert!(effective_capabilities(&granted, Some(&ready)).contains(&capability));
        }
    }

    /// `device_info` is the capability the old model could never make
    /// effective: it has no OS permission to grant, so a runner demanding
    /// `granted` refused it forever.
    #[test]
    fn device_info_becomes_effective_without_an_imaginary_permission() {
        let granted = BTreeSet::from([DeviceCapability::DeviceInfo]);
        let advertised = surface(
            &[DeviceCapability::DeviceInfo],
            &[(DeviceCapability::DeviceInfo, OsPermission::NotRequired)],
            &[(DeviceCapability::DeviceInfo, DeviceReadiness::Ready)],
        );
        assert_eq!(
            effective_capabilities(&granted, Some(&advertised)),
            granted,
            "a capability needing no permission must be effective once it is ready"
        );
        assert_eq!(
            permission_requirement(DeviceCapability::DeviceInfo),
            Some(PermissionRequirement::None)
        );
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

    /// A page that was open across the upgrade is told what to do about it.
    ///
    /// v1 accepted a closing audio frame with no utterance id and v2 refuses
    /// one, which is a change to what a v1 frame *means* rather than an
    /// addition to it. Without the version bump such a client would keep
    /// speaking v1 and have every utterance refused by a field-level error it
    /// cannot interpret; with it, the session is refused once, by version, with
    /// the one instruction that fixes it.
    #[test]
    fn a_client_from_before_the_utterance_id_is_told_to_reload() {
        let mut frame = client_talk_frame(
            1,
            TalkClientFrameKind::Hello {
                media_type: "audio/webm;codecs=opus".into(),
                sample_rate_hz: 48_000,
                channels: 1,
            },
        );
        frame.protocol_version = TALK_PROTOCOL_VERSION_WITHOUT_UTTERANCE_ID;
        let error = frame.validate().expect_err("a v1 client must be refused");
        assert!(error.contains("reload the page"), "{error}");

        // A version this build has never spoken gets the generic refusal —
        // telling somebody to reload would not help them.
        frame.protocol_version = 99;
        let error = frame.validate().expect_err("an unknown version is refused");
        assert!(
            error.contains("Unsupported Talk protocol version 99"),
            "{error}"
        );
        assert!(!error.contains("reload"), "{error}");
    }

    fn talk_generation() -> String {
        random_token(TALK_SESSION_GENERATION_RANDOM_BYTES).unwrap()
    }

    fn client_talk_frame(frame_sequence: u64, kind: TalkClientFrameKind) -> TalkClientFrame {
        TalkClientFrame {
            protocol_version: TALK_PROTOCOL_VERSION,
            session_id: "session-one".into(),
            session_generation: talk_generation(),
            frame_sequence,
            kind,
        }
    }

    fn server_talk_frame(frame_sequence: u64, kind: TalkServerFrameKind) -> TalkServerFrame {
        TalkServerFrame {
            protocol_version: TALK_PROTOCOL_VERSION,
            session_id: "session-one".into(),
            session_generation: talk_generation(),
            frame_sequence,
            kind,
        }
    }

    #[test]
    fn talk_tickets_bind_random_generations_to_a_bounded_session_path() {
        let first =
            TalkTicketResponse::issue("session-one", 1_000, DEFAULT_TALK_TICKET_TTL_MS).unwrap();
        let second =
            TalkTicketResponse::issue("session-one", 1_000, DEFAULT_TALK_TICKET_TTL_MS).unwrap();
        assert_ne!(first.session_generation, second.session_generation);
        assert_ne!(first.ticket, second.ticket);
        assert_eq!(
            first.websocket_path,
            "/v1/remote/device/talk/session-one/stream"
        );
        assert!(!first.websocket_path.contains(&first.ticket));
        assert!(first.validate(1_000).is_ok());
        assert!(first.validate(first.expires_at_ms).is_err());
        assert!(TalkTicketResponse::issue("bad/session", 1_000, 1_000).is_err());
    }

    #[test]
    fn every_talk_frame_kind_round_trips_and_validates() {
        let audio = STANDARD.encode(b"bounded audio");
        let client_frames = vec![
            client_talk_frame(
                1,
                TalkClientFrameKind::Hello {
                    media_type: "audio/webm;codecs=opus".into(),
                    sample_rate_hz: 48_000,
                    channels: 1,
                },
            ),
            client_talk_frame(
                2,
                TalkClientFrameKind::Audio {
                    audio_sequence: 1,
                    media_type: "audio/webm;codecs=opus".into(),
                    audio_base64: audio.clone(),
                    last: false,
                    utterance_id: None,
                },
            ),
            client_talk_frame(
                3,
                TalkClientFrameKind::State {
                    state: TalkState::Listening,
                },
            ),
            client_talk_frame(
                4,
                TalkClientFrameKind::Interrupt {
                    reason: Some("barge_in".into()),
                },
            ),
        ];
        for frame in client_frames {
            frame.validate().unwrap();
            let json = serde_json::to_string(&frame).unwrap();
            assert_eq!(
                serde_json::from_str::<TalkClientFrame>(&json).unwrap(),
                frame
            );
        }

        let server_frames = vec![
            server_talk_frame(1, TalkServerFrameKind::Ready),
            server_talk_frame(
                2,
                TalkServerFrameKind::State {
                    state: TalkState::Thinking,
                },
            ),
            server_talk_frame(
                3,
                TalkServerFrameKind::Transcript {
                    text: "hello".into(),
                    is_final: true,
                },
            ),
            server_talk_frame(4, TalkServerFrameKind::AssistantDelta { text: "hi".into() }),
            server_talk_frame(
                5,
                TalkServerFrameKind::OutputAudio {
                    audio_sequence: 1,
                    media_type: "audio/mpeg".into(),
                    audio_base64: audio,
                },
            ),
            server_talk_frame(
                6,
                TalkServerFrameKind::Error {
                    code: "provider_unavailable".into(),
                    message: "Try again".into(),
                    retryable: true,
                },
            ),
        ];
        for frame in server_frames {
            frame.validate().unwrap();
            let json = serde_json::to_string(&frame).unwrap();
            assert_eq!(
                serde_json::from_str::<TalkServerFrame>(&json).unwrap(),
                frame
            );
        }
    }

    #[test]
    fn talk_frames_reject_unversioned_unbounded_or_malformed_payloads() {
        let mut frame = client_talk_frame(
            1,
            TalkClientFrameKind::Audio {
                audio_sequence: 1,
                media_type: "audio/webm".into(),
                audio_base64: STANDARD.encode(b"audio"),
                last: false,
                utterance_id: None,
            },
        );
        frame.protocol_version += 1;
        assert!(frame.validate().is_err());
        frame.protocol_version = TALK_PROTOCOL_VERSION;
        frame.session_generation = "predictable".into();
        assert!(frame.validate().is_err());
        frame.session_generation = talk_generation();
        frame.kind = TalkClientFrameKind::Audio {
            audio_sequence: 1,
            media_type: "video/webm".into(),
            audio_base64: STANDARD.encode(b"audio"),
            last: false,
            utterance_id: None,
        };
        assert!(frame.validate().is_err());
        frame.kind = TalkClientFrameKind::Audio {
            audio_sequence: 1,
            media_type: "audio/webm".into(),
            audio_base64: STANDARD.encode(vec![0; MAX_TALK_AUDIO_BYTES + 1]),
            last: false,
            utterance_id: None,
        };
        assert!(frame.validate().is_err());

        let oversized_text = server_talk_frame(
            1,
            TalkServerFrameKind::AssistantDelta {
                text: "x".repeat(MAX_TALK_TEXT_BYTES + 1),
            },
        );
        assert!(oversized_text.validate().is_err());
    }

    #[test]
    fn talk_sequence_tracker_rejects_frame_and_audio_replay() {
        let mut tracker = TalkSequenceTracker::default();
        tracker.accept(1, None).unwrap();
        tracker.accept(2, Some(1)).unwrap();
        assert!(tracker.accept(2, None).is_err());
        assert!(tracker.accept(3, Some(1)).is_err());
        assert_eq!(tracker.last_frame_sequence(), 2);
        assert_eq!(tracker.last_audio_sequence(), 1);
        tracker.accept(4, Some(2)).unwrap();
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
            artifact_sha256: None,
            error: None,
            execution_id: None,
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

    /// A retry and a contradiction have to be distinguishable without keeping
    /// the first report around to compare field by field.
    #[test]
    fn a_terminal_digest_separates_a_retry_from_a_different_answer() {
        let value = serde_json::json!({ "width": 1_280 });
        let first = terminal_digest(
            DeviceCommandState::Succeeded,
            Some(&value),
            Some(&"a".repeat(64)),
            None,
        );
        assert_eq!(
            first,
            terminal_digest(
                DeviceCommandState::Succeeded,
                Some(&value),
                Some(&"a".repeat(64)),
                None
            ),
            "the same report must digest the same, or every retry becomes a conflict"
        );
        for different in [
            terminal_digest(
                DeviceCommandState::Failed,
                Some(&value),
                Some(&"a".repeat(64)),
                None,
            ),
            terminal_digest(
                DeviceCommandState::Succeeded,
                Some(&serde_json::json!({ "width": 640 })),
                Some(&"a".repeat(64)),
                None,
            ),
            terminal_digest(
                DeviceCommandState::Succeeded,
                Some(&value),
                Some(&"b".repeat(64)),
                None,
            ),
        ] {
            assert_ne!(first, different);
        }
    }

    #[test]
    fn a_start_request_refuses_an_unusable_execution_id() {
        assert!(DeviceCommandStartRequest { execution_id: None }
            .validate()
            .is_ok());
        assert!(DeviceCommandStartRequest {
            execution_id: Some("exec-0123456789".into()),
        }
        .validate()
        .is_ok());
        assert!(DeviceCommandStartRequest {
            execution_id: Some("short".into()),
        }
        .validate()
        .is_err());
        assert!(DeviceCommandStartRequest {
            execution_id: Some("exec with spaces".into()),
        }
        .validate()
        .is_err());
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
