use std::path::Path;

use clap::Subcommand;
use little_monkey_lib::native_skills::{NativeSkillManager, SkillSource};
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
}
