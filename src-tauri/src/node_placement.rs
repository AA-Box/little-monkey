//! Remote node as a scheduled device (roadmap K17) — the wire shapes and the
//! pure decisions.
//!
//! # Why this is a second plane rather than an extension of the first
//!
//! What already exists under `daemon/remote/` is a **control plane for one
//! runner**: the node is the server, the paired device is a controller, and
//! authority runs controller → runner. Every route reflects it — approve,
//! cancel, pause, kill, chat, capture, and a workflow launch that takes only an
//! id the node already holds. **No existing route accepts a `RunSpec`, a policy
//! or a budget**: the run is authored on the node, never shipped to it.
//!
//! K17 needs the other direction — a scheduler holding a list of nodes it can
//! place work on. That is a *second* plane beside the control plane, and the two
//! share only the transport (pinned TLS, signed and replay-proof requests,
//! scoped capabilities, rotation/revocation), which is the part already built and
//! reused here untouched.
//!
//! This module is deliberately Tauri-free and I/O-free: it is the wire contract
//! plus the four decisions (does this node qualify, which qualifying node wins,
//! is a node still alive, and what a vanished node means for the work placed on
//! it). Both sides of the wire and both binaries depend on it, and every decision
//! is unit-testable without a second machine — which matters, because everything
//! past node description genuinely needs two hosts and this repo's CI has one.
//!
//! ## The naming trap
//!
//! `daemon/admission.rs`'s `Placement`/`placement()` mean *RAM-vs-VRAM placement
//! of model layers on the local box*, and `Reservation::Remote` is an accounting
//! exemption for provider HTTP. Neither is machine selection. The type in this
//! module that means machine selection is [`select_node`].

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use crate::run_protocol::{
    PermissionPolicySnapshot, RunBudgets, RunKind, RunSpec, WorkspaceContext,
};
use crate::runtime_adapter::HardwareSnapshot;

/// Wire version for every shape in this module. Bumped independently of
/// `REMOTE_PROTOCOL_VERSION`: the control plane and the placement plane are
/// versioned separately because they can and will move at different speeds.
pub const NODE_PROTOCOL_VERSION: u32 = 1;

/// The residency label a node carries when its operator has not set one.
///
/// A named constant rather than an empty string so a placement rule asking for
/// `"unspecified"` is an explicit choice, and so a node that never had a label
/// can never accidentally satisfy a rule asking for a real jurisdiction.
pub const RESIDENCY_UNSPECIFIED: &str = "unspecified";

/// How often a placing daemon re-probes a node it has placed work on.
pub const HEARTBEAT_INTERVAL_MS: u64 = 30_000;

/// Silence after which a node is no longer a placement candidate. Three missed
/// heartbeats: one missed probe is a hiccup, three is a pattern.
pub const NODE_STALE_AFTER_MS: u64 = 3 * HEARTBEAT_INTERVAL_MS;

/// Silence after which placed work is treated as lost rather than merely
/// unobserved. Ten missed heartbeats — deliberately far past
/// [`NODE_STALE_AFTER_MS`], because dropping a node from *ranking* is cheap and
/// reversible while declaring its running work dead is neither.
pub const NODE_VANISHED_AFTER_MS: u64 = 10 * HEARTBEAT_INTERVAL_MS;

/// How many times a placement is re-placed after its node vanished before it is
/// failed for good.
pub const PLACEMENT_MAX_ATTEMPTS: u32 = 3;

// --- S1: what a node says it is --------------------------------------------

/// One backend on the node and whether anything on that node actually executes
/// on it.
///
/// The `executes` flag is the node's own
/// [`crate::runtime_adapter::execution_support`] answer, not the detector's
/// `available` flag. K16 established that those are different facts, and a
/// placement decision must read the first: a node that *detects* ROCm and cannot
/// run anything on it is not a ROCm node.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NodeAccelerator {
    /// Lowercase token from `AcceleratorKind` (`cpu`, `metal`, `cuda`, …).
    pub kind: String,
    /// Detected and reporting itself usable.
    pub available: bool,
    /// Something on that node runs work on it.
    pub executes: bool,
    /// What executes on it, or why nothing does.
    pub detail: String,
    pub total_memory_bytes: Option<u64>,
    pub available_memory_bytes: Option<u64>,
}

/// A model the node has installed, with the footprint the node measured.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NodeModel {
    pub model_id: String,
    pub display_name: String,
    /// Runtime family token (`llama_cpp`, `mlx`, …) as the node's hub reports it.
    pub runtime: String,
    pub weights_bytes: u64,
    pub estimated_ram_bytes: u64,
    pub estimated_vram_bytes: u64,
}

/// Everything a node advertises about itself.
///
/// Deliberately *not* a benchmark result. See [`select_node`] for why measured
/// throughput is not an input here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NodeDescriptor {
    pub protocol_version: u32,
    pub runner_id: String,
    /// Operator-set display name. Never used as an identity.
    pub node_name: String,
    /// Operator-set data-residency label — the jurisdiction or zone this
    /// machine's disks are in. The node states it; nothing infers it.
    pub residency: String,
    pub hardware: HardwareSnapshot,
    pub accelerators: Vec<NodeAccelerator>,
    pub resident_models: Vec<NodeModel>,
    /// Whether the node's own queue is currently accepting work (K8
    /// backpressure). A node that says `false` is never chosen.
    pub accepting: bool,
    pub queue_depth: u32,
    pub queue_capacity: u32,
    pub captured_at_ms: u64,
}

impl NodeDescriptor {
    pub fn validate(&self) -> Result<(), String> {
        if self.protocol_version != NODE_PROTOCOL_VERSION {
            return Err(format!(
                "Unsupported node protocol version {}",
                self.protocol_version
            ));
        }
        if self.runner_id.is_empty() || self.runner_id.len() > 256 {
            return Err("Node descriptor has an invalid runner id".to_string());
        }
        validate_residency(&self.residency)?;
        if self.accelerators.len() > 64 || self.resident_models.len() > 4_096 {
            return Err("Node descriptor is implausibly large".to_string());
        }
        Ok(())
    }

    /// Collapses the descriptor into the shape [`select_node`] ranks. `alias` is
    /// the placing side's local name for this node; the node never sees it.
    pub fn candidate(&self, alias: &str, last_seen_at_ms: Option<u64>) -> NodeCandidate {
        NodeCandidate {
            alias: alias.to_string(),
            runner_id: self.runner_id.clone(),
            residency: self.residency.clone(),
            accepting: self.accepting,
            last_seen_at_ms,
            queue_depth: self.queue_depth,
            queue_capacity: self.queue_capacity.max(1),
            available_ram_bytes: self.hardware.available_ram_bytes,
            executing_accelerators: self
                .accelerators
                .iter()
                .filter(|entry| entry.executes && entry.available)
                .map(|entry| entry.kind.clone())
                .collect(),
            resident_models: self
                .resident_models
                .iter()
                .map(|model| model.model_id.clone())
                .collect(),
        }
    }
}

/// The wire token for an accelerator kind, taken from its own serde
/// representation rather than from a second match here — a seventh backend must
/// not need remembering in two places.
#[must_use]
pub fn accelerator_token(kind: crate::runtime_adapter::AcceleratorKind) -> String {
    serde_json::to_value(kind)
        .ok()
        .and_then(|value| value.as_str().map(str::to_string))
        // Unreachable for a unit-variant enum with a snake_case rename; the
        // fallback exists so a serialization change degrades to an
        // unmatchable token rather than to a panic in a route handler.
        .unwrap_or_else(|| format!("{kind:?}").to_ascii_lowercase())
}

/// What this machine's backends look like on the placement wire.
///
/// Reads `available` from the detector and `executes` from
/// [`crate::runtime_adapter::execution_support`], keeping K16's distinction
/// intact all the way across the wire: a node that detects a backend it cannot
/// run anything on advertises exactly that, and [`select_node`] refuses it for a
/// run that requires the backend.
#[must_use]
pub fn describe_accelerators(snapshot: &HardwareSnapshot) -> Vec<NodeAccelerator> {
    snapshot
        .platform
        .accelerators
        .iter()
        .map(|entry| {
            let support = crate::runtime_adapter::execution_support(entry.kind);
            let (executes, detail) = match support {
                crate::runtime_adapter::ExecutionSupport::Executes { via } => (true, via),
                crate::runtime_adapter::ExecutionSupport::DetectionOnly { reason } => {
                    (false, reason)
                }
            };
            NodeAccelerator {
                kind: accelerator_token(entry.kind),
                available: entry.available,
                executes,
                detail,
                total_memory_bytes: entry.total_memory_bytes,
                available_memory_bytes: entry.available_memory_bytes,
            }
        })
        .collect()
}

/// A residency label is an operator-authored token, so it is bounded and
/// restricted rather than free text: it is compared for exact equality by
/// [`PlacementRequirement`], and a label with a stray space or case difference
/// that silently failed to match would read as "the rule was applied" when it was
/// not.
pub fn validate_residency(value: &str) -> Result<(), String> {
    if value.is_empty()
        || value.len() > 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
    {
        return Err(format!(
            "Data-residency label '{value}' must be 1-64 characters of [a-z0-9-]"
        ));
    }
    Ok(())
}

/// The node's liveness answer, cheap enough to serve on every heartbeat.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NodeHealth {
    pub protocol_version: u32,
    pub runner_id: String,
    pub now_ms: u64,
    pub accepting: bool,
    pub queue_depth: u32,
    pub queue_capacity: u32,
    /// Placed runs this node currently holds in a non-terminal state.
    pub placed_active: u32,
}

// --- S2: shipping a run to a node ------------------------------------------

/// A frozen `RunSpec` offered to a node, plus the residency rule the placer
/// applied.
///
/// The spec travels whole — `PermissionPolicySnapshot` and `RunBudgets`
/// included — which is the point of the slice. Both are already `Serialize` with
/// `deny_unknown_fields`, so the wire shape is not the work; the receiving,
/// validating and *owning* of a foreign spec is.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlaceRunRequest {
    pub protocol_version: u32,
    pub spec: RunSpec,
    /// The residency the placer required. Echoed so the node can refuse a
    /// placement whose rule it does not in fact satisfy — the node checks the
    /// claim rather than trusting that the placer checked it.
    #[serde(default)]
    pub required_residency: Option<String>,
    /// What the placer believes the node's `runner_id` is. A mismatch is a
    /// refusal: it means the alias now points at a different machine.
    #[serde(default)]
    pub expected_runner_id: Option<String>,
}

impl PlaceRunRequest {
    pub fn validate(&self) -> Result<(), String> {
        if self.protocol_version != NODE_PROTOCOL_VERSION {
            return Err(format!(
                "Unsupported node protocol version {}",
                self.protocol_version
            ));
        }
        self.spec.validate().map_err(|error| error.to_string())?;
        if let Some(residency) = &self.required_residency {
            validate_residency(residency)?;
        }
        Ok(())
    }
}

/// What the node answers once it owns the spec.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlaceRunResponse {
    pub protocol_version: u32,
    /// The submitter's run id, echoed. This is the correlation key on both
    /// sides — the node deliberately does **not** adopt it as its own run id.
    pub submitted_run_id: String,
    /// The run id the node minted in its own ledger.
    ///
    /// Two ids and not one, because the node owns its ledger: adopting a foreign
    /// run id would let a submitter choose (or collide with) a local identity,
    /// and it would make the node's own hash chain depend on a name it did not
    /// generate.
    pub node_run_id: String,
    pub job_id: String,
    pub state: String,
    pub accepted_at_ms: u64,
    /// The node's residency, restated at acceptance, so the acceptance record on
    /// the placing side names the label the work actually landed under.
    pub residency: String,
}

/// One placed run as the node reports it back.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlacedRunStatus {
    pub protocol_version: u32,
    pub submitted_run_id: String,
    pub node_run_id: String,
    pub job_id: String,
    pub state: String,
    pub terminal: bool,
    pub updated_at_ms: u64,
    /// The node's own denial/failure text, when it has one. This is the field
    /// the S3 acceptance is read from: an egress refusal shows up here because
    /// the *node* refused it, not because the submitter predicted it would.
    #[serde(default)]
    pub last_error: Option<String>,
}

/// The execution-relevant half of a placed `RunSpec`, frozen into the node's
/// own queue snapshot.
///
/// This is what makes S3 more than a promise. The node's queue takes recipes,
/// and a recipe carries neither an egress allowlist nor run budgets — so a spec
/// converted to a recipe and back would arrive with the *node's* defaults and
/// the travelled policy would be decoration. Carrying the four frozen fields
/// verbatim through the snapshot means the process that finally runs builds its
/// `RunSpec` from the submitter's policy, not from anything local.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlacedRunSnapshot {
    pub schema_version: u32,
    /// The submitter's run id, kept for correlation and for the audit trail.
    pub submitted_run_id: String,
    pub kind: RunKind,
    pub target: crate::run_protocol::ModelTargetSnapshot,
    pub workspace: Option<WorkspaceContext>,
    pub permission_policy: PermissionPolicySnapshot,
    pub budgets: RunBudgets,
}

impl PlacedRunSnapshot {
    #[must_use]
    pub fn from_spec(spec: &RunSpec) -> Self {
        Self {
            schema_version: NODE_PROTOCOL_VERSION,
            submitted_run_id: spec.run_id.clone(),
            kind: spec.kind.clone(),
            target: spec.target.clone(),
            workspace: spec.workspace.clone(),
            permission_policy: spec.permission_policy.clone(),
            budgets: spec.budgets.clone(),
        }
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.schema_version != NODE_PROTOCOL_VERSION {
            return Err(format!(
                "Unsupported placed-run snapshot version {}",
                self.schema_version
            ));
        }
        self.permission_policy
            .validate()
            .map_err(|error| error.to_string())?;
        self.budgets.validate().map_err(|error| error.to_string())?;
        Ok(())
    }

    /// The one filesystem root this placement needs, if it needs one.
    ///
    /// A node that cannot produce this path refuses the placement rather than
    /// silently substituting its own working directory — which is the difference
    /// between "the workspace travelled" and "a workspace was invented".
    #[must_use]
    pub fn primary_root(&self) -> Option<&str> {
        let workspace = self.workspace.as_ref()?;
        workspace
            .roots
            .iter()
            .find(|root| root.root_id == workspace.primary_root_id)
            .or_else(|| workspace.roots.first())
            .map(|root| root.canonical_path.as_str())
    }
}

// --- S5: which node, and why -----------------------------------------------

/// A node as placement sees it. Everything here comes from the node's own
/// [`NodeDescriptor`] plus the placing side's record of when it last answered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeCandidate {
    pub alias: String,
    pub runner_id: String,
    pub residency: String,
    pub accepting: bool,
    pub last_seen_at_ms: Option<u64>,
    pub queue_depth: u32,
    pub queue_capacity: u32,
    pub available_ram_bytes: u64,
    /// Backends on this node that actually execute work — see
    /// [`NodeAccelerator::executes`].
    pub executing_accelerators: BTreeSet<String>,
    pub resident_models: BTreeSet<String>,
}

impl NodeCandidate {
    fn free_slots(&self) -> u32 {
        self.queue_capacity.saturating_sub(self.queue_depth)
    }
}

/// What a placement asks of a node.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PlacementRequirement {
    /// The data-residency rule. `None` means the placer stated none, which is
    /// **not** the same as `Some("unspecified")`: the first accepts any node,
    /// the second accepts only nodes whose operator explicitly left the label
    /// unset.
    pub residency: Option<String>,
    /// The model the run's frozen target names, when it names a local one.
    /// A node with it resident is preferred, never required — a node that can
    /// pull it still runs the job.
    pub model_id: Option<String>,
    /// A backend the run must actually execute on. Required, not preferred.
    pub required_accelerator: Option<String>,
    pub min_available_ram_bytes: u64,
}

/// Why no node was chosen. Each arm is a different sentence to an operator and
/// a different fix, which is why this is not an `Option`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlacementRefusal {
    /// Nothing is paired for placement at all.
    NoNodes,
    /// Nodes exist; every one of them was excluded, and this says by what.
    NoCandidate { reasons: Vec<(String, String)> },
}

impl PlacementRefusal {
    #[must_use]
    pub fn message(&self) -> String {
        match self {
            Self::NoNodes => {
                "No node is paired for placement; pair one with --mobile place-runs".to_string()
            }
            Self::NoCandidate { reasons } => {
                let detail = reasons
                    .iter()
                    .map(|(alias, reason)| format!("{alias}: {reason}"))
                    .collect::<Vec<_>>()
                    .join("; ");
                format!("No paired node can take this run ({detail})")
            }
        }
    }
}

/// Why one node was excluded. Returned as text on [`PlacementRefusal`] rather
/// than as an enum because it is only ever shown, never branched on.
fn exclusion(
    candidate: &NodeCandidate,
    requirement: &PlacementRequirement,
    now_ms: u64,
) -> Option<String> {
    match liveness(candidate.last_seen_at_ms, now_ms) {
        NodeLiveness::Alive => {}
        NodeLiveness::Stale { silent_ms } | NodeLiveness::Vanished { silent_ms } => {
            return Some(format!("silent for {silent_ms} ms"));
        }
    }
    if !candidate.accepting {
        return Some("its queue is refusing work".to_string());
    }
    if let Some(required) = &requirement.residency {
        if &candidate.residency != required {
            return Some(format!(
                "data residency is '{}', not '{required}'",
                candidate.residency
            ));
        }
    }
    if let Some(accelerator) = &requirement.required_accelerator {
        if !candidate.executing_accelerators.contains(accelerator) {
            return Some(format!("nothing there executes on {accelerator}"));
        }
    }
    if requirement.min_available_ram_bytes > 0
        && candidate.available_ram_bytes < requirement.min_available_ram_bytes
    {
        return Some(format!(
            "{} bytes of memory free, {} needed",
            candidate.available_ram_bytes, requirement.min_available_ram_bytes
        ));
    }
    if candidate.free_slots() == 0 {
        return Some("its queue is full".to_string());
    }
    None
}

/// **The placement rule.** Which owned node this run goes to, or why none can
/// take it.
///
/// A node qualifies only if it is *alive*, *accepting*, satisfies the
/// *data-residency rule exactly*, actually *executes* on any required backend,
/// has the free memory the run asked for, and has a queue slot. Among the
/// qualifying nodes, the order is: (1) the model is already resident, because
/// not pulling weights is the largest difference placement can make without
/// measuring anything; (2) more free queue slots first; (3) more free memory
/// first; (4) alias, so the order is total and two placements of the same run
/// never disagree.
///
/// # Throughput is deliberately not an input, and this is the collision
///
/// K17's acceptance asks for placement by *measured throughput*. The benchmark
/// surface in this repo is built on the opposite invariant — **no number is
/// displayed that was not measured on the machine displaying it** — with
/// `BenchmarkFreshness::DifferentMachine` existing precisely to refuse another
/// machine's numbers. Importing a node's benchmark would either violate that
/// invariant or require a parallel measurement surface that is careful never to
/// touch the local one.
///
/// So this places by **capability and the node's own admission verdict only**,
/// and says so rather than pretending. A node that admits the job can run it,
/// which is very likely enough. The upgrade has a stated trigger: when two nodes
/// both admit a job and the choice between them measurably matters, import the
/// node's measurement tagged with its own `MachineIdentity` and keep it out of
/// every surface that shows local numbers.
pub fn select_node<'a>(
    candidates: &'a [NodeCandidate],
    requirement: &PlacementRequirement,
    now_ms: u64,
) -> Result<&'a NodeCandidate, PlacementRefusal> {
    if candidates.is_empty() {
        return Err(PlacementRefusal::NoNodes);
    }
    let mut qualified: Vec<&NodeCandidate> = Vec::new();
    let mut reasons: Vec<(String, String)> = Vec::new();
    for candidate in candidates {
        match exclusion(candidate, requirement, now_ms) {
            Some(reason) => reasons.push((candidate.alias.clone(), reason)),
            None => qualified.push(candidate),
        }
    }
    if qualified.is_empty() {
        return Err(PlacementRefusal::NoCandidate { reasons });
    }
    let resident = |candidate: &NodeCandidate| {
        requirement
            .model_id
            .as_ref()
            .is_some_and(|model| candidate.resident_models.contains(model))
    };
    qualified.sort_by(|left, right| {
        resident(right)
            .cmp(&resident(left))
            .then(right.free_slots().cmp(&left.free_slots()))
            .then(right.available_ram_bytes.cmp(&left.available_ram_bytes))
            .then(left.alias.cmp(&right.alias))
    });
    Ok(qualified[0])
}

/// Which of [`select_node`]'s keys put the winner ahead of the runner-up, so a
/// placement record can name the reason instead of asserting one.
#[must_use]
pub fn deciding_key(
    chosen: &NodeCandidate,
    runner_up: Option<&NodeCandidate>,
    requirement: &PlacementRequirement,
) -> &'static str {
    let Some(other) = runner_up else {
        return "sole_candidate";
    };
    let resident = |candidate: &NodeCandidate| {
        requirement
            .model_id
            .as_ref()
            .is_some_and(|model| candidate.resident_models.contains(model))
    };
    if resident(chosen) != resident(other) {
        return "model_resident";
    }
    if chosen.free_slots() != other.free_slots() {
        return "free_queue_slots";
    }
    if chosen.available_ram_bytes != other.available_ram_bytes {
        return "available_ram_bytes";
    }
    "alias"
}

// --- S4: liveness, and what a vanished node means --------------------------

/// How long a node has been silent, bucketed into the three answers that call
/// for different behaviour.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeLiveness {
    Alive,
    /// Not a placement candidate any more; work already there is still assumed
    /// to be running.
    Stale {
        silent_ms: u64,
    },
    /// Long enough that work placed there is treated as lost.
    Vanished {
        silent_ms: u64,
    },
}

/// A node that has never answered is [`NodeLiveness::Vanished`], not `Alive`.
///
/// The direction matters: an unknown node must never be chosen for placement,
/// and "we have never heard from it" is not evidence of health.
#[must_use]
pub fn liveness(last_seen_at_ms: Option<u64>, now_ms: u64) -> NodeLiveness {
    let Some(last_seen) = last_seen_at_ms else {
        return NodeLiveness::Vanished {
            silent_ms: u64::MAX,
        };
    };
    // A node whose clock is ahead of ours reads as freshly seen rather than as
    // silent for a negative time.
    let silent_ms = now_ms.saturating_sub(last_seen);
    if silent_ms >= NODE_VANISHED_AFTER_MS {
        NodeLiveness::Vanished { silent_ms }
    } else if silent_ms >= NODE_STALE_AFTER_MS {
        NodeLiveness::Stale { silent_ms }
    } else {
        NodeLiveness::Alive
    }
}

/// State of one placement as the *placing* side tracks it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlacementState {
    Accepted,
    Running,
    Succeeded,
    Failed,
    Cancelled,
    /// Its node vanished and it has not been re-placed yet.
    Lost,
}

impl PlacementState {
    #[must_use]
    pub const fn token(self) -> &'static str {
        match self {
            Self::Accepted => "accepted",
            Self::Running => "running",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::Lost => "lost",
        }
    }

    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "accepted" => Self::Accepted,
            "running" => Self::Running,
            "succeeded" => Self::Succeeded,
            "failed" => Self::Failed,
            "cancelled" => Self::Cancelled,
            "lost" => Self::Lost,
            _ => return None,
        })
    }

    #[must_use]
    pub const fn terminal(self) -> bool {
        matches!(self, Self::Succeeded | Self::Failed | Self::Cancelled)
    }
}

/// What to do about one placement, given how long its node has been silent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlacementReconcile {
    /// Nothing to do — the node is answering, or the placement is already over.
    Keep,
    /// The node is silent but not yet gone. Recorded so an operator sees it;
    /// nothing is moved.
    Degraded { silent_ms: u64 },
    /// The node is gone and this placement has attempts left: place it again.
    Replace { attempt: u32, reason: String },
    /// The node is gone and this placement is out of attempts.
    Fail { reason: String },
}

/// **The failure semantics a placed run gets when its node goes away.**
///
/// Before K17's S2 there was nothing to lose: `ProcessKind::RemoteRun` is
/// `RestartPolicy::Never` and terminal from birth, and its doc says exactly why —
/// *"a remote run records that a remote controller asked for work, not the work
/// itself."* That stays true for the request row. What changes is that the work
/// itself now exists on another machine, and a vanished node has to be a
/// **process-level failure with a defined restart policy**, not a run that simply
/// stops being mentioned.
///
/// `attempt` is how many times this placement has already been placed (1 for the
/// original), so the third vanish of a `PLACEMENT_MAX_ATTEMPTS = 3` placement
/// fails rather than looping.
#[must_use]
pub fn reconcile_placement(
    state: PlacementState,
    last_seen_at_ms: Option<u64>,
    attempt: u32,
    max_attempts: u32,
    now_ms: u64,
) -> PlacementReconcile {
    if state.terminal() {
        return PlacementReconcile::Keep;
    }
    match liveness(last_seen_at_ms, now_ms) {
        NodeLiveness::Alive => PlacementReconcile::Keep,
        NodeLiveness::Stale { silent_ms } => PlacementReconcile::Degraded { silent_ms },
        NodeLiveness::Vanished { silent_ms } => {
            let silence = if silent_ms == u64::MAX {
                "the node has never answered".to_string()
            } else {
                format!("the node has been silent for {silent_ms} ms")
            };
            if attempt < max_attempts {
                PlacementReconcile::Replace {
                    attempt: attempt + 1,
                    reason: format!("{silence}; re-placing (attempt {})", attempt + 1),
                }
            } else {
                PlacementReconcile::Fail {
                    reason: format!("{silence}; no attempts left after {attempt}"),
                }
            }
        }
    }
}

/// A minimal placeable `RunSpec`, shared with `egress`'s own test for the S3
/// acceptance so the two cannot disagree about what a placed spec looks like.
#[cfg(test)]
pub(crate) mod tests_support {
    use super::*;
    use crate::run_protocol::{
        CapabilityAssessment, CapabilityState, ClientIdentity, ClientKind,
        ModelCapabilitiesSnapshot, ModelTargetSnapshot, PermissionMode, RootAccess, RootGrant,
        ToolPolicyDecision, RUN_PROTOCOL_SCHEMA_VERSION,
    };

    fn unknown_capability() -> CapabilityAssessment {
        CapabilityAssessment {
            state: CapabilityState::Unknown,
            evidence: "fixture".to_string(),
        }
    }

    pub(crate) fn placement_spec(run_id: &str) -> RunSpec {
        RunSpec {
            schema_version: RUN_PROTOCOL_SCHEMA_VERSION,
            run_id: run_id.to_string(),
            idempotency_key: format!("idem-{run_id}"),
            created_at_ms: 1_000,
            kind: RunKind::Background,
            submitted_by: ClientIdentity {
                client_id: "placer".into(),
                instance_id: "placer".into(),
                kind: ClientKind::Daemon,
                version: "1".into(),
            },
            task: "placed fixture".into(),
            instructions: None,
            input_artifact_ids: Vec::new(),
            target: ModelTargetSnapshot::Provider {
                target_id: "target".into(),
                label: "target".into(),
                provider_id: "anthropic".into(),
                endpoint: "https://api.example.com".into(),
                model: "model".into(),
                credential_ref_id: "credential:anthropic".into(),
                capabilities: ModelCapabilitiesSnapshot {
                    tool_calling: unknown_capability(),
                    vision: unknown_capability(),
                    embeddings: unknown_capability(),
                    structured_output: unknown_capability(),
                    image_generation: unknown_capability(),
                    audio: unknown_capability(),
                    runtime_lifecycle: unknown_capability(),
                    fim: unknown_capability(),
                    code_completion: unknown_capability(),
                    inline_edit: unknown_capability(),
                    fim_metadata: None,
                },
            },
            workspace: Some(WorkspaceContext {
                workspace_id: "workspace-placed".into(),
                primary_root_id: "root".into(),
                roots: vec![RootGrant {
                    root_id: "root".into(),
                    canonical_path: "/tmp".into(),
                    access: RootAccess::ReadWrite,
                    allow_symlinks_within_root: false,
                }],
                repository_policy: None,
            }),
            permission_policy: PermissionPolicySnapshot {
                mode: PermissionMode::Auto,
                unattended: true,
                approval_timeout_ms: 60_000,
                default_tool_decision: ToolPolicyDecision::Prompt,
                tool_rules: Vec::new(),
                allow_network: true,
                allow_external_mutations: false,
                egress_allowlist: None,
                channel_send: None,
            },
            budgets: RunBudgets {
                wall_time_ms: 60_000,
                max_iterations: 4,
                max_model_calls: 8,
                max_tool_calls: 8,
                max_input_tokens: 10_000,
                max_output_tokens: 10_000,
                max_cost_micros: None,
                max_artifact_bytes: 1_024,
                max_event_count: 1_000,
            },
            autonomous_task: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate(alias: &str) -> NodeCandidate {
        NodeCandidate {
            alias: alias.to_string(),
            runner_id: format!("runner-{alias}"),
            residency: "eu-west".to_string(),
            accepting: true,
            last_seen_at_ms: Some(1_000),
            queue_depth: 0,
            queue_capacity: 8,
            available_ram_bytes: 16 * 1024 * 1024 * 1024,
            executing_accelerators: BTreeSet::from(["cpu".to_string(), "metal".to_string()]),
            resident_models: BTreeSet::new(),
        }
    }

    fn requirement() -> PlacementRequirement {
        PlacementRequirement {
            residency: Some("eu-west".to_string()),
            ..Default::default()
        }
    }

    #[test]
    fn a_residency_rule_is_an_exact_match_and_unspecified_is_not_a_wildcard() {
        let mut elsewhere = candidate("us");
        elsewhere.residency = "us-east".to_string();
        let mut unlabelled = candidate("plain");
        unlabelled.residency = RESIDENCY_UNSPECIFIED.to_string();
        let nodes = vec![elsewhere, unlabelled];
        let refusal = select_node(&nodes, &requirement(), 1_000).unwrap_err();
        assert!(
            matches!(&refusal, PlacementRefusal::NoCandidate { reasons } if reasons.len() == 2),
            "both nodes must be excluded and both must say why: {refusal:?}"
        );
        assert!(refusal.message().contains("data residency"));

        // A placer that stated no rule takes either.
        let any = PlacementRequirement::default();
        assert!(select_node(&nodes, &any, 1_000).is_ok());
    }

    #[test]
    fn a_node_that_only_detects_a_backend_is_not_a_node_for_it() {
        let mut node = candidate("detects-rocm");
        node.executing_accelerators = BTreeSet::from(["cpu".to_string()]);
        let requirement = PlacementRequirement {
            required_accelerator: Some("rocm".to_string()),
            ..Default::default()
        };
        let refusal = select_node(std::slice::from_ref(&node), &requirement, 1_000).unwrap_err();
        assert!(refusal.message().contains("executes on rocm"));
    }

    #[test]
    fn a_silent_node_is_not_a_placement_candidate_and_an_unknown_one_never_was() {
        let mut silent = candidate("silent");
        silent.last_seen_at_ms = Some(0);
        let refusal = select_node(
            std::slice::from_ref(&silent),
            &PlacementRequirement::default(),
            NODE_STALE_AFTER_MS,
        )
        .unwrap_err();
        assert!(refusal.message().contains("silent for"));

        let mut never = candidate("never");
        never.last_seen_at_ms = None;
        assert!(select_node(
            std::slice::from_ref(&never),
            &PlacementRequirement::default(),
            1_000
        )
        .is_err());
    }

    #[test]
    fn a_resident_model_wins_over_a_larger_emptier_machine() {
        let mut big = candidate("big");
        big.available_ram_bytes = 512 * 1024 * 1024 * 1024;
        let mut warm = candidate("warm");
        warm.resident_models = BTreeSet::from(["qwen3-8b".to_string()]);
        let requirement = PlacementRequirement {
            residency: Some("eu-west".to_string()),
            model_id: Some("qwen3-8b".to_string()),
            ..Default::default()
        };
        let nodes = vec![big, warm];
        let chosen = select_node(&nodes, &requirement, 1_000).unwrap();
        assert_eq!(chosen.alias, "warm");
        assert_eq!(
            deciding_key(chosen, Some(&nodes[0]), &requirement),
            "model_resident"
        );
    }

    #[test]
    fn a_full_or_refusing_queue_is_excluded_and_the_emptier_queue_wins() {
        let mut full = candidate("full");
        full.queue_depth = 8;
        let mut refusing = candidate("refusing");
        refusing.accepting = false;
        let nodes = vec![full, refusing];
        assert!(select_node(&nodes, &requirement(), 1_000).is_err());

        let mut busy = candidate("busy");
        busy.queue_depth = 7;
        let idle = candidate("idle");
        let nodes = vec![busy, idle];
        let chosen = select_node(&nodes, &requirement(), 1_000).unwrap();
        assert_eq!(chosen.alias, "idle");
        assert_eq!(
            deciding_key(chosen, Some(&nodes[0]), &requirement()),
            "free_queue_slots"
        );
    }

    #[test]
    fn the_order_is_total_so_two_placements_never_disagree() {
        let one = candidate("a");
        let two = candidate("b");
        assert_eq!(
            select_node(&[one.clone(), two.clone()], &requirement(), 1_000)
                .unwrap()
                .alias,
            "a"
        );
        assert_eq!(
            select_node(&[two, one], &requirement(), 1_000)
                .unwrap()
                .alias,
            "a"
        );
    }

    #[test]
    fn no_nodes_at_all_is_a_different_answer_from_no_node_qualifying() {
        assert_eq!(
            select_node(&[], &requirement(), 1_000).unwrap_err(),
            PlacementRefusal::NoNodes
        );
    }

    /// The S4 semantics, in the three steps they actually happen in: a node goes
    /// quiet, then goes away, and the placement is re-placed until its attempts
    /// run out.
    #[test]
    fn a_vanished_node_re_places_until_its_attempts_run_out() {
        let placed_at = 0;
        assert_eq!(
            reconcile_placement(
                PlacementState::Running,
                Some(placed_at),
                1,
                PLACEMENT_MAX_ATTEMPTS,
                HEARTBEAT_INTERVAL_MS
            ),
            PlacementReconcile::Keep
        );
        assert!(matches!(
            reconcile_placement(
                PlacementState::Running,
                Some(placed_at),
                1,
                PLACEMENT_MAX_ATTEMPTS,
                NODE_STALE_AFTER_MS
            ),
            PlacementReconcile::Degraded { .. }
        ));
        assert!(matches!(
            reconcile_placement(
                PlacementState::Running,
                Some(placed_at),
                1,
                PLACEMENT_MAX_ATTEMPTS,
                NODE_VANISHED_AFTER_MS
            ),
            PlacementReconcile::Replace { attempt: 2, .. }
        ));
        assert!(matches!(
            reconcile_placement(
                PlacementState::Running,
                Some(placed_at),
                PLACEMENT_MAX_ATTEMPTS,
                PLACEMENT_MAX_ATTEMPTS,
                NODE_VANISHED_AFTER_MS
            ),
            PlacementReconcile::Fail { .. }
        ));
    }

    /// A finished placement is never resurrected by its node going away
    /// afterwards — the work is done and the result is already recorded.
    #[test]
    fn a_terminal_placement_is_left_alone_however_dead_its_node_is() {
        for state in [
            PlacementState::Succeeded,
            PlacementState::Failed,
            PlacementState::Cancelled,
        ] {
            assert_eq!(
                reconcile_placement(state, None, 1, PLACEMENT_MAX_ATTEMPTS, u64::MAX / 2),
                PlacementReconcile::Keep
            );
        }
    }

    #[test]
    fn residency_labels_are_bounded_tokens_rather_than_free_text() {
        assert!(validate_residency("eu-west").is_ok());
        assert!(validate_residency(RESIDENCY_UNSPECIFIED).is_ok());
        assert!(validate_residency("EU West").is_err());
        assert!(validate_residency("").is_err());
        assert!(validate_residency(&"x".repeat(65)).is_err());
    }

    #[test]
    fn placement_states_round_trip_through_their_tokens() {
        for state in [
            PlacementState::Accepted,
            PlacementState::Running,
            PlacementState::Succeeded,
            PlacementState::Failed,
            PlacementState::Cancelled,
            PlacementState::Lost,
        ] {
            assert_eq!(PlacementState::parse(state.token()), Some(state));
        }
        assert_eq!(PlacementState::parse("nonsense"), None);
    }
}
