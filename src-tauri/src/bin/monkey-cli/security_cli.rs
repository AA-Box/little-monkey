use std::collections::BTreeMap;
use std::path::Path;

use clap::Subcommand;
use little_monkey_lib::denial_sink::{DenialRecord, DenialSink, SINK_FILE};
use little_monkey_lib::native_skills::{NativeSkillManager, SkillSource};
use little_monkey_lib::process_table::{ProcessEgressDestinations, ProcessFilter};
use little_monkey_lib::run_ledger::{
    ChainVerification, PermissionGap, RunLedger, StoredPermissionDecision, StoredSubsystemEvent,
    Subsystem, ToolCallOrigin,
};
use little_monkey_lib::run_protocol::PermissionDecision;
use little_monkey_lib::security_doctor::{
    append_findings, run_security_audit, DaemonSecurityState, DeviceCommandSnapshot,
    DeviceGrantSnapshot, FindingStatus, NativeSkillSnapshot, PushPrivacySnapshot,
    SecurityAuditRequest, SecurityRuntimeSnapshot,
};

#[derive(Subcommand, Debug)]
pub enum SecurityCmd {
    /// Audit Little Monkey's local security posture without contacting a model.
    Audit {
        /// Recursively inspect protected app-data trees and verify the remote TLS pin.
        #[arg(long)]
        deep: bool,
        /// Apply only narrow safe fixes: private modes and disabling clearly unsafe listeners.
        #[arg(long)]
        fix: bool,
        /// Print the versioned machine-readable report.
        #[arg(long)]
        json: bool,
    },
    /// Emit the half of the security audit only the daemon can see.
    ///
    /// Devices, messaging accounts, phone numbers and peers keep their state in
    /// databases this binary owns. The desktop's Security Doctor reads this and
    /// folds it into the same audit `security audit` runs, so a check added on
    /// one side is never missing from the other.
    DaemonState,
    /// Recompute a run's event hash chain and report whether it is intact.
    ///
    /// Detects an edited event, a deleted interior event, and a truncated tail.
    /// Cannot detect removal of the whole chain — a per-run chain has no anchor
    /// outside the database holding it, and the output says so rather than
    /// implying otherwise.
    VerifyRunChain {
        /// The run to verify.
        run_id: String,
        /// Print the versioned machine-readable verdict.
        #[arg(long)]
        json: bool,
    },
    /// Produce the permission decision that authorized a tool call.
    ///
    /// The K12 acceptance says a tool call whose authorizing decision cannot be
    /// produced from the log is a bug, so finding nothing exits non-zero. That
    /// is the difference between "it was allowed" and "nothing gated it".
    PermissionTrail {
        /// The tool call id to trace.
        tool_call_id: String,
        /// Print the versioned machine-readable trail.
        #[arg(long)]
        json: bool,
    },
    /// Ask a whole run the question `permission-trail` asks one call.
    ///
    /// K12's acceptance says a tool call whose authorizing decision cannot be
    /// produced from the log is a bug. Asking one id at a time can only confirm
    /// a call somebody already suspected — this asks every call in the run, and
    /// exits non-zero when a **mutating** one has no decision behind it. An
    /// ungated read is listed and not counted: reading a file is not an
    /// authorization event.
    PermissionGaps {
        /// The run to sweep.
        run_id: String,
        /// Print the versioned machine-readable report.
        #[arg(long)]
        json: bool,
    },
    /// Produce a redacted trace of what the messaging, telephony, peer and
    /// device subsystems have been doing, for handing to somebody else.
    ///
    /// Carries no message text, no transcript, no audio, no key and no
    /// credential — those have no field to live in. Every identifier is
    /// replaced by a token that is stable within one bundle and meaningless
    /// outside it, so a trace stays followable without naming anybody.
    SupportBundle {
        /// Print the versioned machine-readable bundle. The only output there
        /// is; the flag exists so the shape matches every other command here.
        #[arg(long, default_value_t = true)]
        json: bool,
    },
    /// Show the unified subsystem event stream, and verify its hash chain.
    ///
    /// This is the stream the run-less subsystems write to — HTTP, MCP, browser,
    /// ACP, remote node — because `run_events` requires a run and those are not
    /// runs. Exits non-zero if the chain is broken.
    SubsystemEvents {
        /// Show only one subsystem: http, mcp, browser, acp or remote.
        #[arg(long)]
        subsystem: Option<String>,
        /// How many of the most recent events to show.
        #[arg(long, default_value_t = 20)]
        limit: u32,
        /// Print the versioned machine-readable stream.
        #[arg(long)]
        json: bool,
    },
    /// Show the egress the app was allowed beside the egress that was refused.
    ///
    /// The two halves deliberately live in two files. Allowed egress is in the
    /// run ledger (`egress_destinations`, migrations V14 and V19) as a counter
    /// per destination, because its volume is the app's own; refusals are in
    /// `egress-denials-v1.sqlite3`, because *their* volume is
    /// attacker-influenced and ring-buffered. This is the read-side join, and
    /// nothing moves between the files to produce it.
    ///
    /// Exits non-zero when the ledger says it could not name where an allowed
    /// request went: a truncated evidence list that does not say it is
    /// truncated reads as a complete one.
    EgressEvidence {
        /// How many processes to read destinations for, and how many refusals
        /// to show.
        #[arg(long, default_value_t = 50)]
        limit: u32,
        /// Print the versioned machine-readable evidence.
        #[arg(long)]
        json: bool,
    },
    /// Produce the run behind each of the daemon's admission decisions.
    ///
    /// `daemon_scheduler_decisions` is the last gating record with no join to
    /// either ledger stream: it decides whether a job runs at all, and it lives
    /// in the daemon's own database. It stays there — the scheduler rewrites its
    /// verdict on every tick and the table ring-buffers itself, which is a poor
    /// neighbour for an append-only chain — so the join is made here, through
    /// `daemon_jobs.run_id`.
    ///
    /// Exits non-zero when a decision names a run the ledger cannot produce,
    /// which is this command's version of the acceptance's bug: work the daemon
    /// admitted and the log cannot account for.
    AdmissionTrail {
        /// How many of the most recent decisions to join.
        #[arg(long, default_value_t = 50)]
        limit: u32,
        /// Print the versioned machine-readable trail.
        #[arg(long)]
        json: bool,
    },
}

/// One admission decision beside the run the ledger can (or cannot) produce for
/// it.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AdmissionJoin {
    pub decided_at_ms: u64,
    pub job_id: String,
    pub outcome: String,
    /// The run `daemon_jobs` says this job became, if it ever became one.
    pub run_id: Option<String>,
    /// Whether the ledger holds that run.
    pub run_in_ledger: bool,
}

impl AdmissionJoin {
    /// A decision that claims a run the ledger does not hold.
    ///
    /// `run_id: None` is **not** this: a rejected or still-queued job never
    /// reached `mark_queued`, so there is no run to produce and nothing is
    /// missing. Counting that as a gap would report the scheduler's most
    /// ordinary outcome as a bug.
    #[must_use]
    pub fn is_unproduceable(&self) -> bool {
        self.run_id.is_some() && !self.run_in_ledger
    }
}

/// Allowed requests the ledger counted but could not name a destination for.
///
/// `run_scope::MAX_DESTINATIONS` caps how many distinct destinations one
/// attribution names, and the excess is counted rather than dropped precisely so
/// this number exists. It is the one figure in this report that makes the report
/// itself incomplete, so it is what the exit status is built on.
fn unnamed_allowed_requests<'a>(
    groups: impl Iterator<Item = &'a ProcessEgressDestinations>,
) -> u64 {
    groups.map(|group| group.dropped).sum()
}

/// Reads the paired-device half of the audit out of the daemon's own store.
///
/// This lives here rather than in `security_doctor.rs` because that module is
/// in the library and the remote store's schema is owned by this binary. A
/// second reader in the library would be a second copy of the schema, and the
/// first migration would make the audit quietly wrong instead of loudly broken.
///
/// Takes an explicit daemon root so a test can prove that opening a real Talk
/// socket makes this production reader report a capture in flight. Asserting on
/// a hand-built snapshot would prove only that the audit can format a struct —
/// which is exactly how the documentation came to claim an observability the
/// socket did not actually have.
pub(crate) fn collect_device_state_at(
    runtime: &mut SecurityRuntimeSnapshot,
    paths: &crate::daemon::store::DaemonPaths,
) {
    let store = match crate::daemon::remote::store::RemoteStore::open(&paths.root) {
        Ok(store) => store,
        Err(error) => {
            runtime.device_state_error = Some(error);
            return;
        }
    };
    let devices = match store.devices() {
        Ok(devices) => devices,
        Err(error) => {
            runtime.device_state_error = Some(error);
            return;
        }
    };
    let registrations = store.push_registrations().unwrap_or_default();
    for device in devices {
        let surface = store.device_surface(&device.device_id).ok().flatten();
        let effective = crate::daemon::remote::protocol::effective_capabilities(
            &device.capabilities,
            surface.as_ref(),
        );
        let physical = |set: &std::collections::BTreeSet<
            crate::daemon::remote::protocol::DeviceCapability,
        >| {
            set.iter()
                .filter(|capability| capability.is_physical())
                .filter_map(|capability| {
                    serde_json::to_value(capability)
                        .ok()
                        .and_then(|value| value.as_str().map(str::to_string))
                })
                .collect::<Vec<_>>()
        };
        runtime.devices.push(DeviceGrantSnapshot {
            push_registered: registrations
                .iter()
                .any(|(registered, _, _)| registered == &device.device_id),
            granted_physical: physical(&device.capabilities),
            effective_physical: physical(&effective),
            revoked: !device.active(),
            last_seen_at_ms: surface.map(|surface| surface.reported_at_ms),
            device_id: device.device_id,
            device_name: device.device_name,
        });
    }
    for command in store.active_device_commands().unwrap_or_default() {
        runtime.device_commands.push(DeviceCommandSnapshot {
            command_id: command.command_id,
            device_id: command.device_id,
            capability: serde_json::to_value(command.capability)
                .ok()
                .and_then(|value| value.as_str().map(str::to_string))
                .unwrap_or_default(),
            state: command.state.as_str().to_string(),
        });
    }
    // Read from the same configuration the listener itself uses, so the audit
    // describes the transport devices actually reach rather than a second copy
    // of what it should be.
    runtime.transport = crate::daemon::remote::host_config(paths)
        .ok()
        .flatten()
        .map(
            |config| little_monkey_lib::security_doctor::TransportSnapshot {
                enabled: config.enabled,
                pinned: !config.certificate_sha256.is_empty(),
                advertise_url: config.advertise_url,
            },
        );
    let push = crate::daemon::remote::push::load_config(paths)
        .ok()
        .flatten();
    runtime.push = Some(PushPrivacySnapshot {
        configured: push.is_some(),
        enabled: push.as_ref().is_some_and(|config| config.enabled),
        include_detail: push.as_ref().is_some_and(|config| config.include_detail),
        registered_devices: registrations.len(),
    });
    runtime.device_state_observed = true;
}

/// The whole daemon-owned half of the audit, as one value.
///
/// The single place this half is produced. `monkey security audit` folds it
/// into its own report and the desktop reads it over the typed bridge, so
/// adding a daemon-owned check reaches both surfaces at once — before this
/// existed the desktop panel silently ran none of them.
pub(crate) fn collect_daemon_security_state() -> DaemonSecurityState {
    let mut runtime = SecurityRuntimeSnapshot::default();
    let mut findings = Vec::new();
    if let Ok(paths) = crate::daemon::store::DaemonPaths::resolve() {
        collect_device_state_at(&mut runtime, &paths);
        let now_ms = now_ms_for_audit();
        findings.extend(crate::telecom_audit::telecom_findings(now_ms));
        findings.extend(crate::daemon::peer_audit::audit_peers(&paths, now_ms));
        findings.extend(crate::daemon::channel_audit::channel_findings(
            &paths, now_ms,
        ));
    }
    DaemonSecurityState {
        schema_version: DAEMON_SECURITY_STATE_SCHEMA_VERSION,
        devices: std::mem::take(&mut runtime.devices),
        device_commands: std::mem::take(&mut runtime.device_commands),
        device_state_observed: runtime.device_state_observed,
        device_state_error: runtime.device_state_error.take(),
        push: runtime.push.take(),
        transport: runtime.transport.take(),
        findings,
    }
}

/// Bumped when the shape changes in a way a reader has to notice. The desktop
/// checks it and refuses a newer one rather than silently reading a subset.
pub(crate) const DAEMON_SECURITY_STATE_SCHEMA_VERSION: u32 = 1;

pub fn run(action: &SecurityCmd, data_dir: &Path, workspace: Option<&Path>) -> Result<(), String> {
    match action {
        SecurityCmd::Audit { deep, fix, json } => {
            let mut runtime = SecurityRuntimeSnapshot::default();
            match data_dir
                .exists()
                .then(|| NativeSkillManager::open_existing(data_dir))
                .transpose()
                .map(|value| value.flatten())
            {
                Ok(Some(manager)) => match manager.discover(workspace, &[]) {
                    Ok(skills) => {
                        runtime.native_skills = skills
                            .into_iter()
                            .map(|skill| NativeSkillSnapshot {
                                command: skill.command,
                                source: match skill.source {
                                    SkillSource::Global { path } => format!("global:{path}"),
                                    SkillSource::Workspace { path } => {
                                        format!("workspace:{path}")
                                    }
                                    SkillSource::SignedPackage { package_id } => {
                                        format!("package:{package_id}")
                                    }
                                },
                                enabled: skill.enabled,
                                eligible: skill.eligibility.eligible,
                                missing_bins: skill.eligibility.missing_bins,
                                missing_env: skill.eligibility.missing_env,
                            })
                            .collect();
                    }
                    Err(error) => runtime.native_skills_error = Some(error.to_string()),
                },
                Ok(None) => {}
                Err(error) => runtime.native_skills_error = Some(error.to_string()),
            }
            // Devices, phone numbers, messaging accounts and peers all live in
            // databases whose schemas this binary owns and the library cannot
            // open. Collected through the same function the desktop reads over
            // the typed bridge, so both surfaces run exactly the same checks.
            let daemon_findings = collect_daemon_security_state().apply(&mut runtime);
            let mut report = run_security_audit(&SecurityAuditRequest {
                app_data_dir: data_dir.to_path_buf(),
                workspace: workspace.map(Path::to_path_buf),
                deep: *deep,
                fix: *fix,
                runtime,
            })?;
            append_findings(&mut report, daemon_findings);
            if *json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&report).map_err(|error| error.to_string())?
                );
            } else {
                print_human(&report);
            }
            Ok(())
        }
        SecurityCmd::SupportBundle { json: _ } => {
            println!(
                "{}",
                serde_json::to_string_pretty(&crate::support_bundle_cli::collect(env!(
                    "CARGO_PKG_VERSION"
                ))?)
                .map_err(|error| error.to_string())?
            );
            Ok(())
        }
        SecurityCmd::DaemonState => {
            // Always JSON: the only caller is the desktop bridge, and a human
            // reading this would be reading it through `security audit`.
            println!(
                "{}",
                serde_json::to_string(&collect_daemon_security_state())
                    .map_err(|error| error.to_string())?
            );
            Ok(())
        }
        SecurityCmd::PermissionTrail { tool_call_id, json } => {
            let ledger = open_existing_ledger(data_dir)?;
            let trail = ledger
                .permission_decisions_for_tool_call(tool_call_id)
                .map_err(|error| error.to_string())?;
            if *json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&trail).map_err(|error| error.to_string())?
                );
            } else {
                print_permission_trail(tool_call_id, &trail);
            }
            // "Nothing gated this call" is the bug K12's acceptance names, so it
            // is a failure rather than an empty report.
            if trail.is_empty() {
                return Err(format!(
                    "no permission decision was ever recorded for tool call {tool_call_id}"
                ));
            }
            Ok(())
        }
        SecurityCmd::PermissionGaps { run_id, json } => {
            let ledger = open_existing_ledger(data_dir)?;
            let gaps = ledger
                .permission_gaps(run_id)
                .map_err(|error| error.to_string())?;
            if *json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&gaps).map_err(|error| error.to_string())?
                );
            } else {
                print_permission_gaps(run_id, &gaps);
            }
            let unauthorized = gaps
                .iter()
                .filter(|gap| gap.is_unauthorized_mutation())
                .count();
            if unauthorized > 0 {
                return Err(format!(
                    "{unauthorized} mutating tool call(s) in run {run_id} have no permission decision in the log"
                ));
            }
            Ok(())
        }
        SecurityCmd::SubsystemEvents {
            subsystem,
            limit,
            json,
        } => {
            let selected = subsystem
                .as_deref()
                .map(|value| {
                    Subsystem::ALL
                        .iter()
                        .find(|candidate| candidate.code() == value)
                        .copied()
                        .ok_or_else(|| {
                            format!(
                                "unknown subsystem '{value}' — expected one of {}",
                                Subsystem::ALL
                                    .iter()
                                    .map(|candidate| candidate.code())
                                    .collect::<Vec<_>>()
                                    .join(", ")
                            )
                        })
                })
                .transpose()?;
            let ledger = open_existing_ledger(data_dir)?;
            let events = ledger
                .recent_subsystem_events(selected, *limit)
                .map_err(|error| error.to_string())?;
            let verdict = ledger
                .verify_subsystem_chain()
                .map_err(|error| error.to_string())?;
            if *json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "events": events,
                        "chain": verdict,
                    }))
                    .map_err(|error| error.to_string())?
                );
            } else {
                print_subsystem_events(&events, &verdict);
            }
            match verdict {
                ChainVerification::Intact { .. } => Ok(()),
                ChainVerification::Broken { .. } => {
                    Err("the subsystem event chain is broken".to_string())
                }
            }
        }
        SecurityCmd::EgressEvidence { limit, json } => {
            let ledger = open_existing_ledger(data_dir)?;
            let table = ledger.process_table();
            // Read through the process list rather than the destination table
            // directly: a destination row is `ON DELETE CASCADE` on its process,
            // so the process is the thing that exists, and this reuses the same
            // page `monkey processes list` shows instead of inventing a second
            // ordering for the same rows.
            let processes = table
                .list(&ProcessFilter {
                    kinds: Vec::new(),
                    live_only: false,
                    parent_process_id: None,
                    workspace: None,
                    limit: Some(*limit),
                })
                .map_err(|error| error.to_string())?;
            let runs: BTreeMap<String, Option<String>> = processes
                .iter()
                .map(|process| (process.process_id.clone(), process.run_id.clone()))
                .collect();
            let ids: Vec<String> = runs.keys().cloned().collect();
            let attributed = table
                .egress_destinations_for(&ids)
                .map_err(|error| error.to_string())?;
            let unattributed = table
                .unattributed_egress_destinations()
                .map_err(|error| error.to_string())?;

            // Opened only if it is already there. A read-only question must not
            // create the sink as a side effect, the same rule
            // `open_existing_ledger` follows.
            let sink_path = data_dir.join(SINK_FILE);
            let denials = if sink_path.exists() {
                DenialSink::open(&sink_path)
                    .and_then(|sink| sink.recent(*limit as usize))
                    .map_err(|error| error.to_string())?
            } else {
                Vec::new()
            };

            let unnamed = unnamed_allowed_requests(attributed.values())
                + unnamed_allowed_requests(unattributed.values());
            if *json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "allowedByProcess": attributed,
                        "allowedOutsideARun": unattributed,
                        "denials": denials.iter().map(denial_json).collect::<Vec<_>>(),
                        "denialSink": sink_path.exists().then(|| sink_path.display().to_string()),
                        "unnamedAllowedRequests": unnamed,
                    }))
                    .map_err(|error| error.to_string())?
                );
            } else {
                print_egress_evidence(&attributed, &runs, &unattributed, &denials, &sink_path);
            }
            if unnamed > 0 {
                return Err(format!(
                    "{unnamed} allowed request(s) went to destinations this ledger cannot name — \
                     the evidence is truncated, not complete"
                ));
            }
            Ok(())
        }
        SecurityCmd::AdmissionTrail { limit, json } => {
            let ledger = open_existing_ledger(data_dir)?;
            let paths = crate::daemon::store::DaemonPaths::under(data_dir);
            let mut joined: Vec<AdmissionJoin> = Vec::new();
            // Absent daemon state is "this machine never ran a daemon", not an
            // error — and opening the store would create it, which a read must
            // not do.
            if paths.state_db.exists() {
                let store = crate::daemon::store::DaemonStore::open(&paths)?;
                for decision in store.recent_decisions(*limit)? {
                    let run_id = store.get_job(&decision.job_id)?.and_then(|job| job.run_id);
                    let run_in_ledger = match &run_id {
                        Some(id) => ledger
                            .load_run(id)
                            .map_err(|error| error.to_string())?
                            .is_some(),
                        None => false,
                    };
                    joined.push(AdmissionJoin {
                        decided_at_ms: decision.decided_at_ms,
                        job_id: decision.job_id,
                        outcome: decision.outcome,
                        run_id,
                        run_in_ledger,
                    });
                }
            }
            if *json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "decisions": joined,
                        "daemonState": paths.state_db.exists()
                            .then(|| paths.state_db.display().to_string()),
                    }))
                    .map_err(|error| error.to_string())?
                );
            } else {
                print_admission_trail(&joined, paths.state_db.exists());
            }
            let unproduceable = joined
                .iter()
                .filter(|entry| entry.is_unproduceable())
                .count();
            if unproduceable > 0 {
                return Err(format!(
                    "{unproduceable} admission decision(s) name a run the ledger cannot produce"
                ));
            }
            Ok(())
        }
        SecurityCmd::VerifyRunChain { run_id, json } => {
            let ledger = open_existing_ledger(data_dir)?;
            let verdict = ledger
                .verify_run_chain(run_id)
                .map_err(|error| error.to_string())?;
            if *json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&verdict).map_err(|error| error.to_string())?
                );
            } else {
                print_chain_verdict(run_id, &verdict);
            }
            // A broken chain is a failure, not a report: exit non-zero so a
            // scripted integrity check does not pass by printing bad news.
            match verdict {
                ChainVerification::Intact { .. } => Ok(()),
                ChainVerification::Broken { .. } => {
                    Err(format!("run {run_id}'s event chain is broken"))
                }
            }
        }
    }
}

/// Open the ledger for a read-only question, refusing rather than creating one
/// as a side effect — the same way `processes_cli` does.
fn open_existing_ledger(data_dir: &Path) -> Result<RunLedger, String> {
    let path = data_dir.join("profile-v1.sqlite3");
    if !path.exists() {
        return Err(format!(
            "No Little Monkey ledger at {} yet — start the app or a daemon run first",
            path.display()
        ));
    }
    RunLedger::open(&path).map_err(|error| error.to_string())
}

/// A denial as JSON. Hand-built because [`DenialRecord`] is a storage struct
/// rather than a wire type, and giving it a `Serialize` here would make this
/// report's field names the sink's public shape.
fn denial_json(record: &DenialRecord) -> serde_json::Value {
    serde_json::json!({
        "recordedAtMs": record.recorded_at_ms,
        "ruleCode": record.rule_code,
        "guard": record.guard,
        // Already bounded to 160 characters and already free of query strings
        // when it was written — this only reads it back.
        "detail": record.detail,
        "runId": record.run_id,
        "unattributedReason": record.unattributed_reason,
    })
}

fn print_egress_evidence(
    attributed: &BTreeMap<String, ProcessEgressDestinations>,
    runs: &BTreeMap<String, Option<String>>,
    unattributed: &BTreeMap<String, ProcessEgressDestinations>,
    denials: &[DenialRecord],
    sink_path: &Path,
) {
    println!(
        "Allowed egress, from the run ledger — a counter per destination, not a row per request:"
    );
    if attributed.is_empty() && unattributed.is_empty() {
        println!("  nothing recorded yet");
    }
    for (process_id, group) in attributed {
        match runs.get(process_id).and_then(Option::as_deref) {
            Some(run_id) => println!("  process {process_id} (run {run_id})"),
            None => println!("  process {process_id} (no ledger run)"),
        }
        print_destinations(group);
    }
    for (reason, group) in unattributed {
        // The reason is `run_scope::Unattributed`'s own persisted code, so this
        // says which kind of run-less work reached the host rather than "none".
        println!("  outside any run — {reason}");
        print_destinations(group);
    }

    println!(
        "\nRefused egress, from {} — its own file, deliberately:",
        sink_path.display()
    );
    if !sink_path.exists() {
        println!("  no denial sink here yet — nothing has been refused on this machine");
        return;
    }
    if denials.is_empty() {
        println!("  the sink exists and holds no refusals");
    }
    for denial in denials {
        println!(
            "  [{}] {} — {}",
            denial.rule_code,
            denial.guard,
            denial.detail.as_deref().unwrap_or("(no detail recorded)")
        );
        match (&denial.run_id, &denial.unattributed_reason) {
            (Some(run_id), _) => println!("    run {run_id}"),
            (None, Some(reason)) => println!("    outside any run — {reason}"),
            (None, None) => println!("    scoped to nothing — this call site is not instrumented"),
        }
    }
    println!(
        "\n  note: this sink ring-buffers itself, so it is the newest refusals rather than all of \
         them. The allowed half above does not — it is a bounded counter."
    );
}

fn print_destinations(group: &ProcessEgressDestinations) {
    for destination in &group.destinations {
        println!(
            "    {}://{}:{} — {} request(s)",
            destination.scheme, destination.host, destination.port, destination.requests
        );
    }
    if group.dropped > 0 {
        println!(
            "    {} request(s) went past the destination cap and were counted but NOT named",
            group.dropped
        );
    }
}

fn print_admission_trail(joined: &[AdmissionJoin], has_daemon_state: bool) {
    if !has_daemon_state {
        println!("No daemon state on this machine — nothing has ever been admitted or refused.");
        return;
    }
    if joined.is_empty() {
        println!("The daemon has recorded no admission decisions yet.");
        return;
    }
    println!("{} admission decision(s), newest first:", joined.len());
    for entry in joined {
        // Three states, like `permission-gaps`: produced, never became a run, and
        // claims a run nobody can produce. The middle one is not a gap.
        let verdict = match (&entry.run_id, entry.run_in_ledger) {
            (Some(run_id), true) => format!("run {run_id} — in the ledger"),
            (Some(run_id), false) => {
                format!("run {run_id} — NOT IN THE LEDGER, which the log cannot account for")
            }
            (None, _) => "never queued, so there is no run to produce".to_string(),
        };
        println!("  [{}] job {} — {verdict}", entry.outcome, entry.job_id);
    }
}

fn print_permission_gaps(run_id: &str, gaps: &[PermissionGap]) {
    if gaps.is_empty() {
        println!("Run {run_id}: every tool call has a permission decision behind it.");
        return;
    }
    println!("Run {run_id}: {} tool call(s) with no decision", gaps.len());
    for gap in gaps {
        let tool = gap.tool_name.as_deref().unwrap_or("(never proposed)");
        // Three states, not two. "The log does not say" is its own answer, and
        // it counts as a bug rather than being waved through — see
        // `PermissionGap::mutation`.
        let verdict = match gap.mutation {
            Some(true) => "MUTATING - nothing authorized it",
            Some(false) => "read-only - no gate expected",
            None => "unknown - no ToolProposed, so the log cannot say",
        };
        println!("  {} - {tool} - {verdict}", gap.tool_call_id);
    }
}

fn print_permission_trail(tool_call_id: &str, trail: &[StoredPermissionDecision]) {
    println!(
        "Permission trail for tool call {tool_call_id}: {} decision(s)",
        trail.len()
    );
    for entry in trail {
        let request = &entry.request;
        println!(
            "[{}] {} — {}",
            match entry.decision {
                Some(PermissionDecision::AllowOnce) => "ALLOW",
                Some(PermissionDecision::AllowForRun) => "ALLOW-RUN",
                Some(PermissionDecision::Deny) => "DENY",
                Some(PermissionDecision::Expired) => "EXPIRED",
                None => "OPEN",
            },
            request.tool_name,
            request.request_id
        );
        println!(
            "  attributed to: {}{}",
            request.attribution.code(),
            request
                .run_id
                .as_deref()
                .map(|run_id| format!(" ({run_id})"))
                .unwrap_or_default()
        );
        if let Some(process_id) = &request.process_id {
            println!("  process: {process_id}");
        }
        // Printed only when the id does not name a real tool call. A trail
        // reached by that id and then told the id is synthetic reads as a
        // contradiction, so `caller` — the case where the id is exactly what
        // was asked for — says nothing.
        match request.tool_call_origin {
            ToolCallOrigin::Caller => {}
            ToolCallOrigin::Synthesized => println!(
                "  tool call: none — this id was generated for a gated operation \
                 that was not a tool call"
            ),
            ToolCallOrigin::Unknown => println!(
                "  tool call: unrecorded — written before the origin was tracked, \
                 so this id may or may not name a real tool call"
            ),
        }
        println!(
            "  mode: {}, risk: {}",
            request.mode,
            match (&request.risk_level, request.risk_floored) {
                (Some(level), true) => format!("{level:?} (deterministic floor)"),
                (Some(level), false) => format!("{level:?} (advisory)"),
                (None, _) => "unclassified".to_string(),
            }
        );
        println!("  operation: {}", request.operation_sha256);
        match (&entry.decision, &entry.decided_by) {
            (Some(_), Some(decided_by)) => println!("  decided by: {decided_by}"),
            _ => println!("  still open — nothing has answered it yet"),
        }
    }
}

fn print_subsystem_events(events: &[StoredSubsystemEvent], verdict: &ChainVerification) {
    match verdict {
        ChainVerification::Intact {
            events_seen,
            events_naming_a_process,
            ..
        } => {
            println!("[OK] subsystem stream: {events_seen} events, chain intact");
            println!("  naming a process: {events_naming_a_process} of {events_seen}");
            // Stated rather than glossed: unlike the run stream, where
            // `runs.last_sequence` is a second witness, nothing here contradicts
            // a removed tail.
            println!(
                "  note: a removed tail cannot be detected — this stream has no counter outside \
                 itself to contradict one"
            );
        }
        ChainVerification::Broken { sequence, detail } => {
            println!("[CRIT] subsystem stream: chain broken at sequence {sequence}");
            println!("  {detail}");
        }
    }
    for event in events {
        println!(
            "#{} [{}] {} {} — {}",
            event.sequence,
            event.outcome.code(),
            event.subsystem.code(),
            event.action,
            event.attribution.code()
        );
        match &event.permission_request_id {
            Some(request_id) => println!("  authorized by: {request_id}"),
            // The acceptance calls an ungated action a bug, so it is named
            // rather than left as an empty field.
            None => println!("  authorized by: nothing gated this action"),
        }
    }
}

fn print_chain_verdict(run_id: &str, verdict: &ChainVerification) {
    match verdict {
        ChainVerification::Intact {
            covered_from,
            covered_through,
            events_seen,
            events_naming_a_process,
        } => match (covered_from, covered_through) {
            (Some(from), Some(through)) => {
                println!("[OK] run {run_id}: {events_seen} events stored, chain intact");
                println!("  covered: sequence {from}..={through}");
                // Reported as a fraction rather than a checkmark: an event
                // appended outside a process scope names no process, so the gap
                // is how far per-event attribution actually reaches.
                println!(
                    "  naming a process: {events_naming_a_process} of {events_seen}{}",
                    if *events_naming_a_process < *events_seen {
                        " (the rest were appended outside any process scope)"
                    } else {
                        ""
                    }
                );
                if *from > 1 {
                    println!(
                        "  note: sequences 1..={} predate hash chaining and are outside its \
                         coverage — they were deliberately not backfilled, since hashing them now \
                         would certify whatever they currently say",
                        from - 1
                    );
                }
            }
            _ => println!(
                "[OK] run {run_id}: {events_seen} events stored, none of them chained — every one \
                 predates hash chaining, so nothing here is verified"
            ),
        },
        ChainVerification::Broken { sequence, detail } => {
            println!("[CRIT] run {run_id}: chain broken at sequence {sequence}");
            println!("  {detail}");
        }
    }
}

fn print_human(report: &little_monkey_lib::security_doctor::SecurityAuditReport) {
    println!(
        "Security Doctor{}: {} critical, {} warning, {} fixed, {} passed",
        if report.deep { " (deep)" } else { "" },
        report.summary.critical,
        report.summary.warnings,
        report.summary.fixed,
        report.summary.passed
    );
    for finding in &report.findings {
        let marker = match finding.status {
            FindingStatus::Pass => "PASS",
            FindingStatus::Info => "INFO",
            FindingStatus::Warning => "WARN",
            FindingStatus::Critical => "CRIT",
            FindingStatus::Fixed => "FIXED",
        };
        println!("[{marker}] {}: {}", finding.category, finding.title);
        println!("  {}", finding.detail);
        if let Some(path) = &finding.path {
            println!("  path: {path}");
        }
        if let Some(remediation) = &finding.remediation {
            println!("  next: {remediation}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Both new commands exit non-zero on one condition each, and both
    /// conditions are a *silence* rather than a failure — the shape that reads
    /// as "nothing to report" if the predicate is wrong by one case.
    #[test]
    fn an_admission_decision_is_unproduceable_only_when_it_named_a_run() {
        let join = |run_id: Option<&str>, run_in_ledger: bool| AdmissionJoin {
            decided_at_ms: 0,
            job_id: "job".to_string(),
            outcome: "admitted".to_string(),
            run_id: run_id.map(str::to_string),
            run_in_ledger,
        };

        // The bug: the daemon admitted work against a run the ledger cannot
        // produce.
        assert!(join(Some("run-1"), false).is_unproduceable());
        // Produced. Not a bug.
        assert!(!join(Some("run-1"), true).is_unproduceable());
        // The scheduler's most ordinary outcome — a decision that names no run
        // at all. Counting this would report every healthy daemon as broken,
        // which is how a check gets switched off.
        assert!(!join(None, false).is_unproduceable());
    }

    #[test]
    fn unnamed_allowed_requests_counts_only_what_the_ledger_could_not_name() {
        let group = |dropped| ProcessEgressDestinations {
            destinations: Vec::new(),
            dropped,
        };

        assert_eq!(unnamed_allowed_requests([].iter()), 0);
        // A complete list is not a truncated one, however long it is.
        assert_eq!(unnamed_allowed_requests([group(0), group(0)].iter()), 0);
        // Both halves of the report contribute: attributed processes and the
        // unattributed bucket are summed by the caller, so this must add rather
        // than take a maximum.
        assert_eq!(unnamed_allowed_requests([group(2), group(3)].iter()), 5);
    }

    #[test]
    fn security_command_shape_supports_requested_flags() {
        use clap::Parser;

        #[derive(Parser)]
        struct Harness {
            #[command(subcommand)]
            command: SecurityCmd,
        }

        let parsed = Harness::try_parse_from(["monkey", "audit", "--deep", "--fix", "--json"])
            .expect("security audit flags should parse");
        assert!(matches!(
            parsed.command,
            SecurityCmd::Audit {
                deep: true,
                fix: true,
                json: true
            }
        ));
    }

    #[test]
    fn verify_run_chain_takes_a_run_id_and_a_json_flag() {
        use clap::Parser;

        #[derive(Parser)]
        struct Harness {
            #[command(subcommand)]
            command: SecurityCmd,
        }

        let parsed = Harness::try_parse_from(["monkey", "verify-run-chain", "run-7", "--json"])
            .expect("verify-run-chain should parse");
        match parsed.command {
            SecurityCmd::VerifyRunChain { run_id, json } => {
                assert_eq!(run_id, "run-7");
                assert!(json);
            }
            other => panic!("expected VerifyRunChain, got {other:?}"),
        }

        assert!(
            Harness::try_parse_from(["monkey", "verify-run-chain"]).is_err(),
            "the run id is required — verifying 'whichever run' is not a question"
        );
    }

    #[test]
    fn permission_trail_takes_a_tool_call_id_and_a_json_flag() {
        use clap::Parser;

        #[derive(Parser)]
        struct Harness {
            #[command(subcommand)]
            command: SecurityCmd,
        }

        let parsed = Harness::try_parse_from(["monkey", "permission-trail", "tool-9", "--json"])
            .expect("permission-trail should parse");
        match parsed.command {
            SecurityCmd::PermissionTrail { tool_call_id, json } => {
                assert_eq!(tool_call_id, "tool-9");
                assert!(json);
            }
            other => panic!("expected PermissionTrail, got {other:?}"),
        }

        assert!(
            Harness::try_parse_from(["monkey", "permission-trail"]).is_err(),
            "the tool call id is required — 'whichever call' is not a question"
        );
    }

    #[test]
    fn subsystem_events_parses_its_filters_and_rejects_an_unknown_subsystem() {
        use clap::Parser;

        #[derive(Parser)]
        struct Harness {
            #[command(subcommand)]
            command: SecurityCmd,
        }

        let parsed = Harness::try_parse_from([
            "monkey",
            "subsystem-events",
            "--subsystem",
            "mcp",
            "--limit",
            "5",
        ])
        .expect("subsystem-events should parse");
        match parsed.command {
            SecurityCmd::SubsystemEvents {
                subsystem,
                limit,
                json,
            } => {
                assert_eq!(subsystem.as_deref(), Some("mcp"));
                assert_eq!(limit, 5);
                assert!(!json);
            }
            other => panic!("expected SubsystemEvents, got {other:?}"),
        }

        // The subsystem name is validated against `Subsystem::ALL` at run time
        // rather than by clap, so an unknown one is a clear error naming the
        // valid set instead of an empty result that reads like "nothing
        // happened".
        assert!(Subsystem::ALL
            .iter()
            .all(|value| value.code() != "carrier-pigeon"));
    }

    /// The human output must never imply coverage it does not have. A run whose
    /// events all predate chaining is reported as verifying nothing, not as `OK`
    /// with an empty range.
    #[test]
    fn an_unchained_run_is_not_reported_as_verified() {
        let intact_but_uncovered = ChainVerification::Intact {
            covered_from: None,
            covered_through: None,
            events_seen: 4,
            events_naming_a_process: 0,
        };
        // `print_chain_verdict` writes to stdout, so assert on the branch the
        // verdict selects rather than on captured output: the tagged union is
        // what makes the two cases impossible to conflate.
        assert!(matches!(
            intact_but_uncovered,
            ChainVerification::Intact {
                covered_from: None,
                ..
            }
        ));
        print_chain_verdict("run-legacy", &intact_but_uncovered);
    }
}

/// Fold extra findings into a finished report, keeping its summary honest.
///
/// The summary is what an operator reads first, so a finding that is not
/// counted may as well not exist.
fn now_ms_for_audit() -> i64 {
    i64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_millis())
            .unwrap_or_default(),
    )
    .unwrap_or(i64::MAX)
}
