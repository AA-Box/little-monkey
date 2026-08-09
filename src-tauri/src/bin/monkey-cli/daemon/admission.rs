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

use little_monkey_lib::m3_runtime_hub::M3ModelFootprint;
use little_monkey_lib::run_protocol::ModelTargetSnapshot;
use little_monkey_lib::runtime_adapter::{
    AcceleratorKind, HardwareSnapshot, LocalOffloadPlanner, MemoryRequirement, OffloadModelProfile,
    OffloadPlanInput,
};

/// Which pool a shortfall is in, so a refusal names the resource that fell
/// short rather than saying "resources".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resource {
    Ram,
    Vram,
}

impl Resource {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Ram => "system memory",
            Self::Vram => "accelerator memory",
        }
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
    Fits { claim: MemoryRequirement },
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
pub fn fit(
    reservation: &Reservation,
    committed: &MemoryRequirement,
    snapshot: &HardwareSnapshot,
) -> Fit {
    let model = match reservation {
        Reservation::Remote => return Fit::Fits { claim: ZERO_MEMORY },
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
        if let Some((total, _)) = accelerator_memory(required, snapshot) {
            if model.estimated_vram_bytes > total {
                return Fit::Never {
                    resource: Resource::Vram,
                    shortfall_bytes: model.estimated_vram_bytes.saturating_sub(total),
                };
            }
        }
    }

    let ram_budget = snapshot.available_ram_bytes.saturating_sub(reserve);
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
    if placement.claim.vram_bytes > 0 {
        let (_, free) = accelerator_memory(placement.accelerator, snapshot).unwrap_or((0, 0));
        let wanted_vram = placement
            .claim
            .vram_bytes
            .saturating_add(committed.vram_bytes);
        if wanted_vram > free {
            return Fit::Hold {
                resource: Resource::Vram,
                shortfall_bytes: wanted_vram.saturating_sub(free),
            };
        }
    }

    Fit::Fits {
        claim: placement.claim,
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
    use little_monkey_lib::runtime_adapter::{AcceleratorCapability, PlatformCapabilities};

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
            fit(&Reservation::Remote, &ZERO_MEMORY, &machine(16, 1)),
            Fit::Fits { claim: ZERO_MEMORY }
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
            fit(&unknown, &ZERO_MEMORY, &machine(16, 16)),
            Fit::Unmeasured
        );
    }

    /// The roadmap's own example: four 12 GB jobs on a 16 GB machine.
    #[test]
    fn the_second_of_four_twelve_gig_jobs_is_held_not_admitted() {
        let machine = machine(16, 16);
        let job = measured(12 * GIB, 0, None);
        assert!(matches!(
            fit(&job, &ZERO_MEMORY, &machine),
            Fit::Fits { .. }
        ));

        let committed = MemoryRequirement {
            ram_bytes: 12 * GIB,
            vram_bytes: 0,
        };
        match fit(&job, &committed, &machine) {
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
        match fit(&measured(64 * GIB, 0, None), &ZERO_MEMORY, &machine(16, 16)) {
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
        let claim = match fit(&job, &ZERO_MEMORY, &machine) {
            Fit::Fits { claim } => claim,
            other => panic!("the first job must fit, got {other:?}"),
        };
        assert!(
            claim.vram_bytes > 0,
            "a CUDA placement must charge the card, got {claim:?}"
        );

        let committed = MemoryRequirement {
            ram_bytes: 0,
            vram_bytes: 8 * GIB,
        };
        match fit(&job, &committed, &machine) {
            Fit::Hold {
                resource: Resource::Vram,
                shortfall_bytes,
            } => assert!(shortfall_bytes > 0),
            other => panic!("expected a Hold naming accelerator memory, got {other:?}"),
        }
    }

    #[test]
    fn a_model_requiring_more_vram_than_the_card_has_is_never_and_names_the_card() {
        match fit(
            &measured(4 * GIB, 40 * GIB, Some(AcceleratorKind::Cuda)),
            &ZERO_MEMORY,
            &cuda_machine(16, 16),
        ) {
            Fit::Never {
                resource: Resource::Vram,
                shortfall_bytes,
            } => assert_eq!(shortfall_bytes, 24 * GIB),
            other => panic!("expected Never on accelerator memory, got {other:?}"),
        }
    }

    /// A model with no hard accelerator requirement spills instead of being
    /// refused, and the spilled share lands on the RAM leg.
    #[test]
    fn a_spilling_model_is_judged_on_its_placement_not_its_raw_vram_estimate() {
        let outcome = fit(
            &measured(20 * GIB, 40 * GIB, None),
            &ZERO_MEMORY,
            &cuda_machine(16, 16),
        );
        match outcome {
            Fit::Fits { claim } => {
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
        match fit(&measured(8 * GIB, 8 * GIB, None), &ZERO_MEMORY, &snapshot) {
            Fit::Fits { claim } => {
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
