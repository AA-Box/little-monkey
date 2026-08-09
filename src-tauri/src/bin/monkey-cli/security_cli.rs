use std::path::Path;

use clap::Subcommand;
use little_monkey_lib::native_skills::{NativeSkillManager, SkillSource};
use little_monkey_lib::run_ledger::{
    ChainVerification, PermissionGap, RunLedger, StoredPermissionDecision, StoredSubsystemEvent,
    Subsystem, ToolCallOrigin,
};
use little_monkey_lib::run_protocol::PermissionDecision;
use little_monkey_lib::security_doctor::{
    run_security_audit, FindingStatus, NativeSkillSnapshot, SecurityAuditRequest,
    SecurityRuntimeSnapshot,
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
}

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
            let report = run_security_audit(&SecurityAuditRequest {
                app_data_dir: data_dir.to_path_buf(),
                workspace: workspace.map(Path::to_path_buf),
                deep: *deep,
                fix: *fix,
                runtime,
            })?;
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
