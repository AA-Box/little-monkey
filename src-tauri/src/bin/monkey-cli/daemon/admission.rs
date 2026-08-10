//! Resource-aware admission (K7).
//!
//! The queue used to admit by a fixed integer concurrency alone, so four jobs
//! each needing 12 GB on a 16 GB machine were all started and all thrashed. The
//! numbers to prevent that already existed and were never asked for: a run's
//! frozen `ModelTargetSnapshot` carries `estimated_memory_bytes`, the model
//! hub's installed inventory carries the RAM *and* accelerator footprint of a
//! model id, `LocalOffloadPlanner` already knows where those bytes would
//! actually land, and `HardwareSnapshot` carries what the machine has.
//!
//! Pure functions on purpose — the engine owns *when* to ask and where the
//! footprint is read from, this owns *what the answer is*, and the answer is
//! testable without a daemon, a ledger, or a machine of any particular size.

use std::collections::BTreeMap;
use std::fmt;

use little_monkey_lib::m3_runtime_hub::M3ModelFootprint;
use little_monkey_lib::run_protocol::ModelTargetSnapshot;
use little_monkey_lib::runtime_adapter::{
    AcceleratorCapability, AcceleratorKind, DeviceSplit, HardwareSnapshot, LocalOffloadPlanner,
    MemoryRequirement, OffloadModelProfile, OffloadPlanInput,
};

/// One accelerator device, as a thing the scheduler reserves against.
///
/// `index` is the runtime's own device ordinal — what `--main-gpu` and a position
/// in `--tensor-split` mean, and what [`AcceleratorDevice::index`] carries. It is
/// **not** an index into any vector here, which matters because a machine can
/// enumerate devices 0 and 2 with nothing at 1.
///
/// `None` is the machine that advertised accelerator memory without enumerating
/// any device: an older snapshot, or a detector that only ever reported a sum.
/// Kept as a case rather than defaulted to device 0 for the reason
/// `DeviceSplit::budget_bytes` gives — with devices unenumerated, the aggregate
/// is indistinguishable from one device's own memory, and inventing an ordinal
/// would make a per-device reservation that no runtime flag could ever honour.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct DeviceId {
    pub kind: AcceleratorKind,
    pub index: Option<u32>,
}

impl DeviceId {
    /// The whole accelerator, for a machine that enumerated no devices.
    pub const fn aggregate(kind: AcceleratorKind) -> Self {
        DeviceId { kind, index: None }
    }

    pub const fn device(kind: AcceleratorKind, index: u32) -> Self {
        DeviceId {
            kind,
            index: Some(index),
        }
    }
}

impl fmt::Display for DeviceId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.index {
            Some(index) => write!(formatter, "{} device {index}", self.kind.label()),
            None => write!(formatter, "{}", self.kind.label()),
        }
    }
}

/// Which pool a shortfall is in, so a refusal names the resource that fell
/// short rather than saying "resources".
///
/// # Why the accelerator arm names a device
///
/// It used to be a bare `Vram`, and that was the whole of what K15 still owed: a
/// second job was admitted against an *aggregate* the first may have exhausted on
/// one card. Two 24 GB cards read as 48 GB of "accelerator memory", so two 20 GB
/// models were both admitted and the second died at load time with an
/// out-of-memory error naming nothing about the cause.
///
/// Naming the device in the shortfall is not cosmetic either: it is what lets
/// `scheduler::preemption_victims` free memory on the card that is actually full
/// rather than suspending a job holding bytes on a different one, which would
/// park real work and admit nobody.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resource {
    Ram,
    Accelerator(DeviceId),
}

impl Resource {
    pub fn label(&self) -> String {
        match self {
            Self::Ram => "system memory".to_string(),
            Self::Accelerator(device) => format!("memory on {device}"),
        }
    }

    /// The device this shortfall is on, if it is on one.
    pub fn device(&self) -> Option<&DeviceId> {
        match self {
            Self::Ram => None,
            Self::Accelerator(device) => Some(device),
        }
    }
}

/// Bytes one job holds on one device.
///
/// A list of these replaces the single `vram_bytes` figure a reservation used to
/// carry. Summing them would put the aggregate straight back, which is the bug.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceClaim {
    pub device: DeviceId,
    pub bytes: u64,
}

/// What is held on each device right now, summed across every resident model.
///
/// A map rather than a total, for the same reason [`Resource`] names a device: a
/// total answers "how much accelerator memory is committed", which is not a
/// question any single card can be checked against.
pub type DeviceCommitments = BTreeMap<DeviceId, u64>;

/// Adds one job's device claims into a running commitment map.
pub fn commit_devices(into: &mut DeviceCommitments, claims: &[DeviceClaim]) {
    for claim in claims {
        let slot = into.entry(claim.device.clone()).or_insert(0);
        *slot = slot.saturating_add(claim.bytes);
    }
}

/// What starting one run will make resident, and the identity of the thing it
/// makes resident.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reservation {
    /// Remote inference claims nothing local: the weights live on someone
    /// else's machine, so a cloud job must not be held behind a local memory
    /// bound it does not use. That is the difference between a scheduler and a
    /// queue that counts to four.
    Remote,
    /// A local model whose footprint nothing installed on this machine knows.
    ///
    /// Deliberately a third case rather than a zero-byte [`Self::Local`].
    /// "We never measured this model" and "this model costs nothing" are the
    /// same `u64` and opposite facts: the second may legitimately satisfy a
    /// memory bound, the first must never be allowed to *look* like it did.
    ///
    /// Admission still starts such a job — refusing every unmeasured model
    /// would refuse every spec written before the estimate existed, which is a
    /// stricter policy than the one asked for — but it starts it as an
    /// explicitly unmeasured admission: it contributes nothing to the committed
    /// total, it is never counted as having *fitted*, and it says so in the log
    /// so an operator can see that this tick's bound was taken on faith.
    Unmeasured { model_id: String },
    /// A local model with a measured footprint.
    Local {
        /// Identity of the *resident* thing, not of the job.
        ///
        /// Reservations are keyed by this rather than by job id because memory
        /// is held by the loaded model, not by the process talking to it: N
        /// queued turns against one local model make it resident once and must
        /// pay for it once. The release rule that falls out of that is the
        /// subtle half — the bytes come back when the *last* holder exits, not
        /// the first — and it is enforced in `DaemonEngine::committed`, which
        /// derives the committed total by grouping live jobs on this key
        /// instead of by counting them.
        model_key: String,
        model: OffloadModelProfile,
    },
}

impl Reservation {
    /// The resident model this job holds, if it holds one.
    pub fn model_key(&self) -> Option<&str> {
        match self {
            Self::Remote | Self::Unmeasured { .. } => None,
            Self::Local { model_key, .. } => Some(model_key),
        }
    }
}

/// What a queued job will claim, given the frozen target and whatever the model
/// hub knows about the model id that target names.
///
/// The hub's inventory wins over the frozen `estimated_memory_bytes` because it
/// is the only source that separates RAM from accelerator memory; the frozen
/// figure is the fallback for a run whose model the hub never installed (the
/// desktop path freezes one, and it is RAM-only by construction).
pub fn reservation(target: &ModelTargetSnapshot, footprint: &M3ModelFootprint) -> Reservation {
    let (model_id, frozen_estimate) = match target {
        // A provider call is HTTP to somebody else's GPU.
        ModelTargetSnapshot::Provider { .. } => return Reservation::Remote,
        ModelTargetSnapshot::Ollama { is_cloud: true, .. } => return Reservation::Remote,
        ModelTargetSnapshot::Ollama {
            model,
            estimated_memory_bytes,
            ..
        } => (model, *estimated_memory_bytes),
        ModelTargetSnapshot::ManagedLlama {
            model_id,
            estimated_memory_bytes,
            ..
        } => (model_id, *estimated_memory_bytes),
    };
    let model_key = target.target_id().to_string();
    if let M3ModelFootprint::Known {
        weights_bytes,
        memory,
        required_accelerator,
        projector_memory_bytes,
    } = footprint
    {
        if weights_bytes > &0 {
            return Reservation::Local {
                model_key,
                model: OffloadModelProfile {
                    weights_bytes: *weights_bytes,
                    estimated_ram_bytes: memory.ram_bytes,
                    estimated_vram_bytes: memory.vram_bytes,
                    required_accelerator: *required_accelerator,
                    has_vision_projector: projector_memory_bytes.is_some(),
                    projector_memory_bytes: projector_memory_bytes.unwrap_or(0),
                },
            };
        }
    }
    match frozen_estimate.filter(|bytes| *bytes > 0) {
        // A frozen estimate is a single total with no RAM/VRAM split, so it can
        // only feed the RAM leg. `weights_bytes` reuses it as a stand-in for the
        // on-disk size, which is the same approximation the hub already makes.
        Some(bytes) => Reservation::Local {
            model_key,
            model: OffloadModelProfile {
                weights_bytes: bytes,
                estimated_ram_bytes: bytes,
                estimated_vram_bytes: 0,
                required_accelerator: None,
                has_vision_projector: false,
                projector_memory_bytes: 0,
            },
        },
        None => Reservation::Unmeasured {
            model_id: model_id.clone(),
        },
    }
}

/// Whether a reservation can be admitted now, later, or not on this machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Fit {
    /// Fits alongside what is already resident. `claim` is the
    /// placement-adjusted memory this job holds until it exits, and is what the
    /// caller commits and later releases.
    ///
    /// `devices` is that same accelerator memory broken out per card, and it is
    /// the half K15 owed: `claim.vram_bytes` is their sum and is kept only so a
    /// caller with nothing to say about devices — a Metal machine, a CPU-only
    /// placement — reads the same as before. Nothing may reserve against the sum.
    Fits {
        claim: MemoryRequirement,
        devices: Vec<DeviceClaim>,
    },
    /// Admitted without a measurement — see [`Reservation::Unmeasured`].
    Unmeasured,
    /// Does not fit right now. The job stays queued and is retried on a later
    /// tick — this is the case that used to be an admission.
    Hold {
        resource: Resource,
        shortfall_bytes: u64,
    },
    /// Cannot fit even on an idle machine of this size. Rejected without ever
    /// being spawned, rather than started and killed by the memory watchdog.
    Never {
        resource: Resource,
        shortfall_bytes: u64,
    },
}

/// Where the offload planner would actually put this model on an *idle* machine
/// of this size, and what that placement costs in each pool.
struct Placement {
    accelerator: AcceleratorKind,
    claim: MemoryRequirement,
    /// `claim.vram_bytes` distributed over the devices the plan chose. Empty for
    /// a CPU or unified-memory placement, where there is no separate pool to
    /// reserve against.
    devices: Vec<DeviceClaim>,
}

pub const ZERO_MEMORY: MemoryRequirement = MemoryRequirement {
    ram_bytes: 0,
    vram_bytes: 0,
};

/// Ask [`LocalOffloadPlanner`] where the model lands, then split its two
/// published estimates along that placement.
///
/// Planned against an idle machine on purpose. Planning against the current
/// load would be self-defeating: the planner's job is to make a model fit by
/// spilling layers to CPU, so it would answer "it fits" at every load level and
/// the accelerator leg below could never hold anything. The idle plan instead
/// answers the question admission actually has — how much of this model wants to
/// be on the accelerator *on this hardware* — and a model whose raw VRAM
/// estimate exceeds the device is judged on the spilled placement it will really
/// get rather than on that estimate.
fn placement(model: &OffloadModelProfile, snapshot: &HardwareSnapshot) -> Placement {
    // Charging the whole RAM estimate and nothing to the accelerator is the
    // pre-K7 bound, and a planner that declined to answer must not loosen it.
    let ram_only = Placement {
        accelerator: AcceleratorKind::Cpu,
        claim: MemoryRequirement {
            ram_bytes: model.estimated_ram_bytes,
            vram_bytes: 0,
        },
        devices: Vec::new(),
    };
    let Ok(plan) = LocalOffloadPlanner::plan(&OffloadPlanInput {
        hardware: snapshot.clone(),
        model: model.clone(),
        reserved: ZERO_MEMORY,
        other_resident_count: 0,
        requested_context_tokens: None,
    }) else {
        return ram_only;
    };
    // Metal is unified memory: one physical pool the RAM leg already bounds. A
    // second accelerator leg here would charge the same bytes twice and hold
    // every Mac at half its real capacity.
    if matches!(
        plan.accelerator,
        AcceleratorKind::Cpu | AcceleratorKind::Metal
    ) {
        return Placement {
            accelerator: plan.accelerator,
            ..ram_only
        };
    }
    let total_layers = u64::from(plan.estimated_total_layers.max(1));
    let on_gpu = u64::from(plan.gpu_layers.min(plan.estimated_total_layers));
    let vram_bytes = share(model.estimated_vram_bytes, on_gpu, total_layers);
    Placement {
        accelerator: plan.accelerator,
        claim: MemoryRequirement {
            // The two estimates describe the same model fully on CPU and fully
            // offloaded, so a partial offload lands between them.
            ram_bytes: model.estimated_ram_bytes.saturating_sub(share(
                model.estimated_ram_bytes,
                on_gpu,
                total_layers,
            )),
            vram_bytes,
        },
        devices: device_claims(
            plan.accelerator,
            &plan.device_split,
            vram_bytes,
            accelerator_capability(plan.accelerator, snapshot),
        ),
    }
}

/// Splits one placement's accelerator bytes across the devices the plan chose.
///
/// The plan already decided *where* — `DeviceSplit` is the answer K15's first
/// half produced — so this only has to distribute the bytes the same way, and it
/// must distribute them by the same rule the planner used or the reservation
/// would describe a placement the runtime is not going to make.
///
/// # The rounding, which has to go up
///
/// `Across`'s weights are floats summing to 1.0, so the per-device shares of an
/// integer byte count will not divide evenly. Each share is rounded *up*, which
/// means the reserved total may exceed the claim by a byte or two per device.
/// That is the correct direction: a reservation that rounded down would under-book
/// the busiest card by exactly the amount that makes the next admission look like
/// it fits.
fn device_claims(
    accelerator: AcceleratorKind,
    split: &DeviceSplit,
    vram_bytes: u64,
    capability: Option<&AcceleratorCapability>,
) -> Vec<DeviceClaim> {
    if vram_bytes == 0 {
        return Vec::new();
    }
    // A machine that enumerated no devices reserves against the accelerator as a
    // whole, which is exactly what it did before this existed. Inventing device 0
    // here would claim a card nothing said was there.
    let enumerated = capability.is_some_and(|entry| !entry.devices.is_empty());
    if !enumerated {
        return vec![DeviceClaim {
            device: DeviceId::aggregate(accelerator),
            bytes: vram_bytes,
        }];
    }
    match split {
        DeviceSplit::SingleDevice { index } => vec![DeviceClaim {
            device: DeviceId::device(accelerator, *index),
            bytes: vram_bytes,
        }],
        DeviceSplit::Across { weights, .. } => weights
            .iter()
            .enumerate()
            .filter(|(_, weight)| **weight > 0.0)
            .map(|(ordinal, weight)| {
                let share = (vram_bytes as f64 * f64::from(*weight)).ceil();
                DeviceClaim {
                    // The position in `weights` *is* the device ordinal —
                    // `--tensor-split` is positional, which is what makes this
                    // index meaningful rather than an index into a local vector.
                    device: DeviceId::device(accelerator, ordinal as u32),
                    bytes: if share.is_finite() && share >= 0.0 {
                        share as u64
                    } else {
                        0
                    },
                }
            })
            .filter(|claim| claim.bytes > 0)
            .collect(),
    }
}

/// The capability entry for `kind`, if this machine advertises it as available.
fn accelerator_capability(
    kind: AcceleratorKind,
    snapshot: &HardwareSnapshot,
) -> Option<&AcceleratorCapability> {
    snapshot
        .platform
        .accelerators
        .iter()
        .find(|entry| entry.kind == kind && entry.available)
}

/// Free bytes on one specific device, or on the accelerator as a whole when no
/// device was enumerated.
///
/// `None` means this machine does not advertise that device at all, which a
/// caller must not read as zero: it is the difference between "the card is full"
/// and "there is no such card".
fn device_free_bytes(device: &DeviceId, snapshot: &HardwareSnapshot) -> Option<u64> {
    let capability = accelerator_capability(device.kind, snapshot)?;
    match device.index {
        Some(index) => capability
            .devices
            .iter()
            .find(|entry| entry.index == index)
            .map(|entry| {
                entry
                    .available_memory_bytes
                    .or(entry.total_memory_bytes)
                    .unwrap_or(0)
            }),
        None => Some(
            capability
                .available_memory_bytes
                .or(capability.total_memory_bytes)
                .unwrap_or(0),
        ),
    }
}

/// Total bytes on one specific device — the capacity a [`Fit::Never`] compares
/// against, as opposed to what happens to be free.
fn device_total_bytes(device: &DeviceId, snapshot: &HardwareSnapshot) -> Option<u64> {
    let capability = accelerator_capability(device.kind, snapshot)?;
    match device.index {
        Some(index) => capability
            .devices
            .iter()
            .find(|entry| entry.index == index)
            .map(|entry| entry.total_memory_bytes.unwrap_or(0)),
        None => Some(capability.total_memory_bytes.unwrap_or(0)),
    }
}

fn share(bytes: u64, numerator: u64, denominator: u64) -> u64 {
    if denominator == 0 {
        return 0;
    }
    u64::try_from(u128::from(bytes) * u128::from(numerator) / u128::from(denominator))
        .unwrap_or(u64::MAX)
}

/// Total and currently-free bytes on one accelerator, or `None` when this
/// machine does not advertise it.
fn accelerator_memory(kind: AcceleratorKind, snapshot: &HardwareSnapshot) -> Option<(u64, u64)> {
    snapshot
        .platform
        .accelerators
        .iter()
        .find(|entry| entry.kind == kind && entry.available)
        .map(|entry| {
            let total = entry.total_memory_bytes.unwrap_or(0);
            (total, entry.available_memory_bytes.unwrap_or(total))
        })
}

/// `committed` is the placement-adjusted memory already held by everything
/// admitted, deduplicated by resident model.
///
/// RAM headroom comes from `HardwareProfile::recommended_ram_reserve_bytes`,
/// which already exists as this codebase's answer to "how much must stay free" —
/// a second, differently-tuned constant here would be one more number to
/// disagree with the planner.
/// `profile_ceiling_bytes` is the system memory the *active profile* (K23) may
/// hold at once — its K4 quota, or its K8 share of a machine it splits with
/// another profile, whichever is tighter. `None` is every installation that has
/// only ever had one identity and set no quota, and takes the same path as
/// before this parameter existed.
///
/// A ceiling smaller than a single job's claim is a [`Fit::Never`], not a hold:
/// holding forever for memory that will never be released is exactly the
/// starvation K8 spent a bound ruling out.
pub fn fit(
    reservation: &Reservation,
    committed: &MemoryRequirement,
    committed_devices: &DeviceCommitments,
    snapshot: &HardwareSnapshot,
    profile_ceiling_bytes: Option<u64>,
) -> Fit {
    let model = match reservation {
        Reservation::Remote => {
            return Fit::Fits {
                claim: ZERO_MEMORY,
                devices: Vec::new(),
            }
        }
        Reservation::Unmeasured { .. } => return Fit::Unmeasured,
        Reservation::Local { model, .. } => model,
    };
    let reserve = snapshot
        .profile()
        .map(|profile| profile.recommended_ram_reserve_bytes)
        .unwrap_or(0);
    let placement = placement(model, snapshot);

    // `Never` means "not even on an idle machine of this size", so both legs
    // compare against capacity rather than against what happens to be free.
    let ram_ceiling = snapshot.total_ram_bytes.saturating_sub(reserve);
    if placement.claim.ram_bytes > ram_ceiling {
        return Fit::Never {
            resource: Resource::Ram,
            shortfall_bytes: placement.claim.ram_bytes.saturating_sub(ram_ceiling),
        };
    }
    // Only a model that *requires* an accelerator can be refused outright for
    // accelerator memory. Everything else spills to CPU and is judged by the
    // RAM leg above, which is why an oversized VRAM estimate on its own is not a
    // refusal — it is a slower placement.
    if let Some(required) = model.required_accelerator {
        // Against the *largest single device*, never the sum. This was the shape
        // of the bug at the detection layer that K15's first half fixed, and it
        // was still here: two 24 GB cards accepted a 40 GB model that neither
        // could hold, then failed at load time naming nothing about the cause.
        if let Some(capability) = accelerator_capability(required, snapshot) {
            let (ceiling, device) = match capability.largest_device_memory() {
                Some(largest) => {
                    let index = capability
                        .devices
                        .iter()
                        .filter(|entry| {
                            entry.available_memory_bytes.or(entry.total_memory_bytes)
                                == Some(largest)
                        })
                        .map(|entry| entry.index)
                        .min();
                    (
                        largest,
                        match index {
                            Some(index) => DeviceId::device(required, index),
                            None => DeviceId::aggregate(required),
                        },
                    )
                }
                // No device enumerated: the aggregate is indistinguishable from
                // one device's own memory, so it is the honest ceiling rather
                // than a guess — the same call `DeviceSplit::budget_bytes` makes.
                None => (
                    capability
                        .total_memory_bytes
                        .or(capability.available_memory_bytes)
                        .unwrap_or(0),
                    DeviceId::aggregate(required),
                ),
            };
            if model.estimated_vram_bytes > ceiling {
                return Fit::Never {
                    resource: Resource::Accelerator(device),
                    shortfall_bytes: model.estimated_vram_bytes.saturating_sub(ceiling),
                };
            }
        }
    }

    if let Some(ceiling) = profile_ceiling_bytes {
        if placement.claim.ram_bytes > ceiling {
            return Fit::Never {
                resource: Resource::Ram,
                shortfall_bytes: placement.claim.ram_bytes.saturating_sub(ceiling),
            };
        }
    }

    let ram_budget = snapshot.available_ram_bytes.saturating_sub(reserve);
    let ram_budget = match profile_ceiling_bytes {
        Some(ceiling) => ram_budget.min(ceiling),
        None => ram_budget,
    };
    let wanted_ram = placement
        .claim
        .ram_bytes
        .saturating_add(committed.ram_bytes);
    if wanted_ram > ram_budget {
        return Fit::Hold {
            resource: Resource::Ram,
            shortfall_bytes: wanted_ram.saturating_sub(ram_budget),
        };
    }
    // Per device, not against a total. This is the whole of what K15 still owed:
    // summing free memory across cards admits a second job against capacity the
    // first already exhausted on one of them.
    for claim in &placement.devices {
        // A device the snapshot does not advertise is not a full device — it is
        // one nobody measured — so it is skipped rather than treated as zero,
        // which would hold every job forever on a machine whose probe failed.
        let Some(free) = device_free_bytes(&claim.device, snapshot) else {
            continue;
        };
        // Capacity first: a claim that no idle card of this size could hold is a
        // refusal, not a hold. Holding for memory that will never be released is
        // the starvation K8 spent a bound ruling out.
        if let Some(total) = device_total_bytes(&claim.device, snapshot) {
            if total > 0 && claim.bytes > total {
                return Fit::Never {
                    resource: Resource::Accelerator(claim.device.clone()),
                    shortfall_bytes: claim.bytes.saturating_sub(total),
                };
            }
        }
        let already = committed_devices.get(&claim.device).copied().unwrap_or(0);
        let wanted = claim.bytes.saturating_add(already);
        if wanted > free {
            return Fit::Hold {
                resource: Resource::Accelerator(claim.device.clone()),
                shortfall_bytes: wanted.saturating_sub(free),
            };
        }
    }

    Fit::Fits {
        claim: placement.claim,
        devices: placement.devices,
    }
}

/// Operator-facing shortfall, used in the rejection reason so a refusal names a
/// number rather than saying "resources".
pub fn describe_bytes(bytes: u64) -> String {
    const GIB: f64 = (1024 * 1024 * 1024) as f64;
    const MIB: f64 = (1024 * 1024) as f64;
    let bytes_f = bytes as f64;
    if bytes_f >= GIB {
        format!("{:.1} GiB", bytes_f / GIB)
    } else {
        format!("{:.0} MiB", bytes_f / MIB)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use little_monkey_lib::runtime_adapter::{
        AcceleratorCapability, AcceleratorDevice, PlatformCapabilities,
    };

    /// [`fit`] against a machine with nothing already committed on any device.
    ///
    /// Almost every case here is about one job on an idle machine, and threading
    /// an empty map through each of them would bury the cases that genuinely
    /// exercise per-device commitment — which pass the map explicitly.
    fn fit_idle(
        reservation: &Reservation,
        committed: &MemoryRequirement,
        snapshot: &HardwareSnapshot,
        profile_ceiling_bytes: Option<u64>,
    ) -> Fit {
        fit(
            reservation,
            committed,
            &DeviceCommitments::new(),
            snapshot,
            profile_ceiling_bytes,
        )
    }

    const GIB: u64 = 1024 * 1024 * 1024;

    fn machine(total_gib: u64, available_gib: u64) -> HardwareSnapshot {
        HardwareSnapshot {
            captured_at_ms: 1,
            total_ram_bytes: total_gib * GIB,
            available_ram_bytes: available_gib * GIB,
            logical_cpu_count: 8,
            platform: PlatformCapabilities::from_host("macos", "aarch64", Vec::new()),
        }
    }

    /// A Linux box with a discrete CUDA card, which is the only shape where the
    /// accelerator leg is a genuinely separate pool from system RAM.
    fn cuda_machine(vram_gib: u64, free_vram_gib: u64) -> HardwareSnapshot {
        HardwareSnapshot {
            captured_at_ms: 1,
            total_ram_bytes: 64 * GIB,
            available_ram_bytes: 64 * GIB,
            logical_cpu_count: 16,
            platform: PlatformCapabilities::from_host(
                "linux",
                "x86_64",
                vec![AcceleratorCapability {
                    kind: AcceleratorKind::Cuda,
                    available: true,
                    device_names: vec!["fixture".into()],
                    total_memory_bytes: Some(vram_gib * GIB),
                    available_memory_bytes: Some(free_vram_gib * GIB),
                    devices: Vec::new(),
                }],
            ),
        }
    }

    fn local(estimate: Option<u64>) -> ModelTargetSnapshot {
        ModelTargetSnapshot::ManagedLlama {
            target_id: "t".into(),
            label: "l".into(),
            model_id: "m".into(),
            model_path: "/tmp/m.gguf".into(),
            capabilities: crate::task::cli_capabilities(),
            estimated_memory_bytes: estimate,
        }
    }

    fn measured(ram: u64, vram: u64, accelerator: Option<AcceleratorKind>) -> Reservation {
        Reservation::Local {
            model_key: "key".into(),
            model: OffloadModelProfile {
                weights_bytes: ram,
                estimated_ram_bytes: ram,
                estimated_vram_bytes: vram,
                required_accelerator: accelerator,
                has_vision_projector: false,
                projector_memory_bytes: 0,
            },
        }
    }

    #[test]
    fn remote_targets_reserve_no_local_memory() {
        let provider = ModelTargetSnapshot::Provider {
            target_id: "t".into(),
            label: "l".into(),
            provider_id: "anthropic".into(),
            endpoint: "https://api.anthropic.com".into(),
            model: "claude".into(),
            credential_ref_id: "ref".into(),
            capabilities: crate::task::cli_capabilities(),
        };
        assert_eq!(
            reservation(&provider, &M3ModelFootprint::Unknown),
            Reservation::Remote
        );

        let cloud = ModelTargetSnapshot::Ollama {
            target_id: "t".into(),
            label: "l".into(),
            base_url: "https://ollama.com".into(),
            model: "big".into(),
            is_cloud: true,
            capabilities: crate::task::cli_capabilities(),
            estimated_memory_bytes: Some(400 * GIB),
        };
        assert_eq!(
            reservation(&cloud, &M3ModelFootprint::Unknown),
            Reservation::Remote
        );
        assert_eq!(
            fit_idle(&Reservation::Remote, &ZERO_MEMORY, &machine(16, 1), None),
            Fit::Fits {
                claim: ZERO_MEMORY,
                devices: Vec::new()
            }
        );
    }

    #[test]
    fn a_frozen_estimate_feeds_the_ram_leg_and_the_hub_footprint_wins_over_it() {
        match reservation(&local(Some(12 * GIB)), &M3ModelFootprint::Unknown) {
            Reservation::Local { model, .. } => {
                assert_eq!(model.estimated_ram_bytes, 12 * GIB);
                assert_eq!(model.estimated_vram_bytes, 0);
            }
            other => panic!("expected a measured reservation, got {other:?}"),
        }

        let hub = M3ModelFootprint::Known {
            weights_bytes: 7 * GIB,
            memory: MemoryRequirement {
                ram_bytes: 9 * GIB,
                vram_bytes: 8 * GIB,
            },
            required_accelerator: None,
            projector_memory_bytes: None,
        };
        match reservation(&local(Some(12 * GIB)), &hub) {
            Reservation::Local { model, .. } => {
                assert_eq!(model.estimated_ram_bytes, 9 * GIB);
                assert_eq!(model.estimated_vram_bytes, 8 * GIB);
            }
            other => panic!("expected the hub footprint, got {other:?}"),
        }
    }

    /// The whole point of the third case: an unmeasured model must not arrive at
    /// the fit computation looking like a model that costs nothing.
    #[test]
    fn an_unknown_footprint_is_unmeasured_rather_than_zero() {
        let unknown = reservation(&local(None), &M3ModelFootprint::Unknown);
        assert_eq!(
            unknown,
            Reservation::Unmeasured {
                model_id: "m".into()
            }
        );
        assert_eq!(unknown.model_key(), None, "it commits nothing");
        // Not `Fits`: it is admitted, but never counted as having fitted.
        assert_eq!(
            fit_idle(&unknown, &ZERO_MEMORY, &machine(16, 16), None),
            Fit::Unmeasured
        );
    }

    /// The roadmap's own example: four 12 GB jobs on a 16 GB machine.
    #[test]
    fn the_second_of_four_twelve_gig_jobs_is_held_not_admitted() {
        let machine = machine(16, 16);
        let job = measured(12 * GIB, 0, None);
        assert!(matches!(
            fit_idle(&job, &ZERO_MEMORY, &machine, None),
            Fit::Fits { .. }
        ));

        let committed = MemoryRequirement {
            ram_bytes: 12 * GIB,
            vram_bytes: 0,
        };
        match fit_idle(&job, &committed, &machine, None) {
            Fit::Hold {
                resource: Resource::Ram,
                ..
            } => {}
            other => panic!("second 12 GiB job must wait on RAM, got {other:?}"),
        }
    }

    #[test]
    fn a_job_too_big_for_the_machine_is_never_rather_than_held() {
        // Held would mean waiting forever for memory that cannot appear.
        match fit_idle(
            &measured(64 * GIB, 0, None),
            &ZERO_MEMORY,
            &machine(16, 16),
            None,
        ) {
            Fit::Never {
                resource: Resource::Ram,
                shortfall_bytes,
            } => assert!(shortfall_bytes >= 48 * GIB),
            other => panic!("expected Never on RAM, got {other:?}"),
        }
    }

    /// The RAM-only bound missed this entirely: 64 GiB of system RAM is plenty,
    /// and the card is the resource that ran out.
    #[test]
    fn accelerator_memory_holds_a_job_the_ram_leg_would_admit() {
        let machine = cuda_machine(16, 16);
        let job = measured(4 * GIB, 12 * GIB, Some(AcceleratorKind::Cuda));
        let claim = match fit_idle(&job, &ZERO_MEMORY, &machine, None) {
            Fit::Fits { claim, .. } => claim,
            other => panic!("the first job must fit, got {other:?}"),
        };
        assert!(
            claim.vram_bytes > 0,
            "a CUDA placement must charge the card, got {claim:?}"
        );

        // The commitment is per *device* now. `MemoryRequirement::vram_bytes` is
        // no longer consulted for the accelerator leg at all, which is the whole
        // of the fix: a pooled total is not a question any single card can be
        // checked against.
        let mut committed_devices = DeviceCommitments::new();
        committed_devices.insert(DeviceId::aggregate(AcceleratorKind::Cuda), 8 * GIB);
        match fit(&job, &ZERO_MEMORY, &committed_devices, &machine, None) {
            Fit::Hold {
                resource: Resource::Accelerator(device),
                shortfall_bytes,
            } => {
                assert_eq!(device, DeviceId::aggregate(AcceleratorKind::Cuda));
                assert!(shortfall_bytes > 0);
            }
            other => panic!("expected a Hold naming accelerator memory, got {other:?}"),
        }
    }

    #[test]
    fn a_model_requiring_more_vram_than_the_card_has_is_never_and_names_the_card() {
        match fit_idle(
            &measured(4 * GIB, 40 * GIB, Some(AcceleratorKind::Cuda)),
            &ZERO_MEMORY,
            &cuda_machine(16, 16),
            None,
        ) {
            Fit::Never {
                resource: Resource::Accelerator(device),
                shortfall_bytes,
            } => {
                // No device enumerated on this fixture, so the aggregate *is* the
                // honest ceiling — the same call `DeviceSplit::budget_bytes`
                // makes, and the reason this is not a refusal to answer.
                assert_eq!(device, DeviceId::aggregate(AcceleratorKind::Cuda));
                assert_eq!(shortfall_bytes, 24 * GIB);
            }
            other => panic!("expected Never on accelerator memory, got {other:?}"),
        }
    }

    /// Two discrete cards, enumerated, with their own memory. This is the shape
    /// the aggregate hid: 2 x 24 GiB reads as 48 GiB of "accelerator memory",
    /// which is the number that makes a model no single card can hold look like
    /// it fits.
    fn two_card_machine(per_card_gib: u64, free_gib: [u64; 2]) -> HardwareSnapshot {
        HardwareSnapshot {
            captured_at_ms: 1,
            total_ram_bytes: 256 * GIB,
            available_ram_bytes: 256 * GIB,
            logical_cpu_count: 32,
            platform: PlatformCapabilities::from_host(
                "linux",
                "x86_64",
                vec![AcceleratorCapability {
                    kind: AcceleratorKind::Cuda,
                    available: true,
                    device_names: vec!["card-0".into(), "card-1".into()],
                    // The display-only aggregate. Nothing below may reserve
                    // against it, which is what these tests are for.
                    total_memory_bytes: Some(2 * per_card_gib * GIB),
                    available_memory_bytes: Some((free_gib[0] + free_gib[1]) * GIB),
                    devices: vec![
                        AcceleratorDevice {
                            index: 0,
                            name: "card-0".into(),
                            total_memory_bytes: Some(per_card_gib * GIB),
                            available_memory_bytes: Some(free_gib[0] * GIB),
                        },
                        AcceleratorDevice {
                            index: 1,
                            name: "card-1".into(),
                            total_memory_bytes: Some(per_card_gib * GIB),
                            available_memory_bytes: Some(free_gib[1] * GIB),
                        },
                    ],
                }],
            ),
        }
    }

    /// The bug K15 still owed, stated as a test: a second job must not be
    /// admitted against memory the first exhausted **on one card**.
    #[test]
    fn a_second_job_is_held_by_the_card_the_first_one_filled_not_by_the_pool() {
        let machine = two_card_machine(24, [24, 24]);
        let job = measured(8 * GIB, 20 * GIB, Some(AcceleratorKind::Cuda));

        let devices = match fit_idle(&job, &ZERO_MEMORY, &machine, None) {
            Fit::Fits { devices, .. } => devices,
            other => panic!("the first job must fit on an idle machine, got {other:?}"),
        };
        assert_eq!(
            devices.len(),
            1,
            "a model that fits one card must not be spread across two, got {devices:?}"
        );
        let filled = devices[0].device.clone();
        assert!(devices[0].bytes > 0);

        // Now book that card and ask again. Under the old aggregate the pool
        // still shows ~28 GiB free across both cards and this was an admission —
        // followed by an out-of-memory at load time naming nothing.
        let mut committed = DeviceCommitments::new();
        commit_devices(&mut committed, &devices);
        match fit(&job, &ZERO_MEMORY, &committed, &machine, None) {
            Fit::Hold {
                resource: Resource::Accelerator(device),
                shortfall_bytes,
            } => {
                assert_eq!(
                    device, filled,
                    "the refusal must name the card that is full"
                );
                assert!(shortfall_bytes > 0);
            }
            other => panic!("expected a Hold naming the filled card, got {other:?}"),
        }
    }

    /// The refusal names a *device*, not "accelerator memory", and that string is
    /// what an operator reads out of a hold reason.
    #[test]
    fn a_device_shortfall_names_the_card_in_its_label() {
        let ram = Resource::Ram;
        assert_eq!(ram.label(), "system memory");
        assert_eq!(ram.device(), None);

        let card = Resource::Accelerator(DeviceId::device(AcceleratorKind::Cuda, 1));
        assert_eq!(card.label(), "memory on CUDA device 1");
        assert_eq!(
            card.device(),
            Some(&DeviceId::device(AcceleratorKind::Cuda, 1))
        );

        // A machine that enumerated no device says so rather than claiming
        // device 0, which no `--main-gpu` could have honoured.
        let whole = Resource::Accelerator(DeviceId::aggregate(AcceleratorKind::Rocm));
        assert_eq!(whole.label(), "memory on ROCm");
        assert_eq!(whole.device().and_then(|device| device.index), None);
    }

    /// A model too large for any *single* card is `Never`, even when the cards
    /// add up to more than it needs. This is the exact number the pre-K15
    /// detection bug produced, now asserted at the admission layer.
    #[test]
    fn a_model_no_single_card_can_hold_is_never_even_when_the_cards_sum_to_enough() {
        let machine = two_card_machine(24, [24, 24]);
        match fit_idle(
            &measured(8 * GIB, 40 * GIB, Some(AcceleratorKind::Cuda)),
            &ZERO_MEMORY,
            &machine,
            None,
        ) {
            Fit::Never {
                resource: Resource::Accelerator(device),
                shortfall_bytes,
            } => {
                assert_eq!(device, DeviceId::device(AcceleratorKind::Cuda, 0));
                assert_eq!(
                    shortfall_bytes,
                    16 * GIB,
                    "measured against the largest single card, never the 48 GiB sum"
                );
            }
            other => panic!("expected Never naming a card, got {other:?}"),
        }
    }

    /// A machine that advertised accelerator memory without enumerating devices
    /// keeps working exactly as it did. Refusing there would silently drop a GPU
    /// the planner used yesterday.
    #[test]
    fn an_unenumerated_accelerator_reserves_against_itself_rather_than_a_made_up_device() {
        let machine = cuda_machine(16, 16);
        let job = measured(4 * GIB, 12 * GIB, Some(AcceleratorKind::Cuda));
        match fit_idle(&job, &ZERO_MEMORY, &machine, None) {
            Fit::Fits { devices, .. } => {
                assert_eq!(devices.len(), 1);
                assert_eq!(
                    devices[0].device,
                    DeviceId::aggregate(AcceleratorKind::Cuda),
                    "no device was enumerated, so none may be invented"
                );
            }
            other => panic!("the job must fit, got {other:?}"),
        }
    }

    /// A CPU or Metal placement reserves against no device at all: Metal is one
    /// physical pool the RAM leg already bounds, and charging it twice would hold
    /// every Apple Silicon machine at half its real capacity.
    #[test]
    fn a_unified_memory_placement_books_no_device() {
        match fit_idle(
            &measured(4 * GIB, 4 * GIB, None),
            &ZERO_MEMORY,
            &machine(32, 32),
            None,
        ) {
            Fit::Fits { claim, devices } => {
                assert!(devices.is_empty(), "got {devices:?}");
                assert_eq!(claim.vram_bytes, 0);
            }
            other => panic!("the job must fit, got {other:?}"),
        }
    }

    /// Commitments add per device rather than into one number.
    #[test]
    fn device_commitments_accumulate_per_card() {
        let mut committed = DeviceCommitments::new();
        let card0 = DeviceId::device(AcceleratorKind::Cuda, 0);
        let card1 = DeviceId::device(AcceleratorKind::Cuda, 1);
        commit_devices(
            &mut committed,
            &[
                DeviceClaim {
                    device: card0.clone(),
                    bytes: 3 * GIB,
                },
                DeviceClaim {
                    device: card1.clone(),
                    bytes: GIB,
                },
            ],
        );
        commit_devices(
            &mut committed,
            &[DeviceClaim {
                device: card0.clone(),
                bytes: 2 * GIB,
            }],
        );
        assert_eq!(committed.get(&card0), Some(&(5 * GIB)));
        assert_eq!(committed.get(&card1), Some(&GIB));
    }

    /// A profile's ceiling (K23) binds before the machine's free memory does,
    /// and a claim no ceiling could ever satisfy is `Never` rather than a hold
    /// that would wait forever.
    #[test]
    fn a_profile_ceiling_bounds_admission_below_what_the_machine_would_allow() {
        let job = measured(6 * GIB, 0, None);
        let idle = machine(64, 64);

        // No ceiling: the machine's own headroom decides, as before K23.
        assert!(matches!(
            fit_idle(&job, &ZERO_MEMORY, &idle, None),
            Fit::Fits { .. }
        ));

        // An 8 GiB ceiling admits the first 6 GiB job and holds the second,
        // on a machine with 58 GiB still free.
        assert!(matches!(
            fit_idle(&job, &ZERO_MEMORY, &idle, Some(8 * GIB)),
            Fit::Fits { .. }
        ));
        match fit_idle(
            &job,
            &MemoryRequirement {
                ram_bytes: 6 * GIB,
                vram_bytes: 0,
            },
            &idle,
            Some(8 * GIB),
        ) {
            Fit::Hold {
                resource: Resource::Ram,
                shortfall_bytes,
            } => assert_eq!(shortfall_bytes, 4 * GIB),
            other => panic!("expected a hold against the profile ceiling, got {other:?}"),
        }

        // A ceiling smaller than one job's claim can never be satisfied by
        // waiting, so it is a refusal at enqueue rather than a permanent hold.
        match fit_idle(&job, &ZERO_MEMORY, &idle, Some(2 * GIB)) {
            Fit::Never {
                resource: Resource::Ram,
                shortfall_bytes,
            } => assert_eq!(shortfall_bytes, 4 * GIB),
            other => panic!("expected Never under an unsatisfiable ceiling, got {other:?}"),
        }

        // A remote run claims nothing local, so no profile ceiling applies.
        assert!(matches!(
            fit_idle(&Reservation::Remote, &ZERO_MEMORY, &idle, Some(1)),
            Fit::Fits { .. }
        ));
    }

    /// A model with no hard accelerator requirement spills instead of being
    /// refused, and the spilled share lands on the RAM leg.
    #[test]
    fn a_spilling_model_is_judged_on_its_placement_not_its_raw_vram_estimate() {
        let outcome = fit_idle(
            &measured(20 * GIB, 40 * GIB, None),
            &ZERO_MEMORY,
            &cuda_machine(16, 16),
            None,
        );
        match outcome {
            Fit::Fits { claim, .. } => {
                assert!(
                    claim.vram_bytes <= 16 * GIB,
                    "cannot claim more than the card has, got {claim:?}"
                );
                assert!(
                    claim.ram_bytes > 0,
                    "the spilled layers have to be charged somewhere, got {claim:?}"
                );
            }
            other => panic!("a spilling model still runs, got {other:?}"),
        }
    }

    /// Metal is one physical pool. Charging it as both RAM and VRAM would hold
    /// every Apple Silicon machine at half its real capacity.
    #[test]
    fn unified_memory_is_charged_once() {
        let mut snapshot = machine(32, 32);
        snapshot.platform = PlatformCapabilities::from_host(
            "macos",
            "aarch64",
            vec![AcceleratorCapability {
                kind: AcceleratorKind::Metal,
                available: true,
                device_names: vec!["fixture".into()],
                total_memory_bytes: Some(32 * GIB),
                available_memory_bytes: Some(32 * GIB),
                devices: Vec::new(),
            }],
        );
        match fit_idle(
            &measured(8 * GIB, 8 * GIB, None),
            &ZERO_MEMORY,
            &snapshot,
            None,
        ) {
            Fit::Fits { claim, .. } => {
                assert_eq!(claim.ram_bytes, 8 * GIB);
                assert_eq!(claim.vram_bytes, 0, "unified memory has no second pool");
            }
            other => panic!("expected a unified-memory fit, got {other:?}"),
        }
    }

    #[test]
    fn shortfall_reads_as_a_size() {
        assert_eq!(describe_bytes(3 * GIB + GIB / 2), "3.5 GiB");
        assert_eq!(describe_bytes(512 * 1024 * 1024), "512 MiB");
    }
}
