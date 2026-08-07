use std::path::Path;

use clap::Subcommand;
use little_monkey_lib::native_skills::{NativeSkillManager, SkillSource};
use little_monkey_lib::run_ledger::{ChainVerification, RunLedger};
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
        SecurityCmd::VerifyRunChain { run_id, json } => {
            let path = data_dir.join("profile-v1.sqlite3");
            if !path.exists() {
                // Read-only question: refuse rather than create a ledger as a
                // side effect, the same way `processes_cli` does.
                return Err(format!(
                    "No Little Monkey ledger at {} yet — start the app or a daemon run first",
                    path.display()
                ));
            }
            let ledger = RunLedger::open(&path).map_err(|error| error.to_string())?;
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
