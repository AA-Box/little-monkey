use clap::{Parser, Subcommand, ValueEnum};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{fs, path::{Path, PathBuf}, process::ExitCode};

const RELATIVE_INDEX: &str = ".little-monkey/standards/index.json";

#[derive(Parser)]
#[command(name = "little-monkey-standards", about = "Headless audit and lifecycle commands for Standards Studio")]
struct Cli {
    /// Repository/workspace root. Defaults to the current directory.
    #[arg(long, default_value = ".")]
    workspace: PathBuf,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Print the portable standards document (or only one lifecycle status).
    List {
        #[arg(long)]
        status: Option<LifecycleStatus>,
        #[arg(long)]
        json: bool,
    },
    /// Change lifecycle state without changing policy text/evidence.
    SetStatus {
        standard_id: String,
        status: MutableStatus,
    },
    /// Re-hash evidence and report drift. With --write, stale contradicted
    /// approved standards are persisted exactly like the Studio's safe rule.
    Drift {
        #[arg(long)]
        write: bool,
        #[arg(long)]
        json: bool,
    },
    /// Fail non-zero when approved standards contain unresolved explicit
    /// conflicts or contradicted/stale evidence. Suitable for CI/audit use.
    Audit {
        #[arg(long)]
        json: bool,
    },
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq, ValueEnum)]
#[serde(rename_all = "snake_case")]
enum LifecycleStatus {
    Candidate,
    Approved,
    Rejected,
    Deprecated,
    Conflicting,
    Stale,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum MutableStatus {
    Approved,
    Rejected,
    Deprecated,
}

impl From<MutableStatus> for LifecycleStatus {
    fn from(value: MutableStatus) -> Self {
        match value {
            MutableStatus::Approved => Self::Approved,
            MutableStatus::Rejected => Self::Rejected,
            MutableStatus::Deprecated => Self::Deprecated,
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum DriftState {
    Healthy,
    Weakened,
    Contradicted,
    NotApplicable,
    Unknown,
}

#[derive(Debug, Deserialize, Serialize)]
struct StandardsDocument {
    schema_version: u32,
    workspace_id: String,
    generated_at_ms: u64,
    standards: Vec<Standard>,
}

#[derive(Debug, Deserialize, Serialize)]
struct Standard {
    standard_id: String,
    version: u64,
    title: String,
    body: String,
    status: LifecycleStatus,
    evidence: Vec<Evidence>,
    #[serde(default)]
    conflicts_with: Vec<String>,
    content_sha256: String,
    drift: DriftState,
    approved_at_ms: Option<u64>,
    last_verified_at_ms: Option<u64>,
    #[serde(flatten)]
    extra: serde_json::Map<String, serde_json::Value>,
}

#[derive(Debug, Deserialize, Serialize)]
struct Evidence {
    path: String,
    sha256: String,
    supports: bool,
    #[serde(flatten)]
    extra: serde_json::Map<String, serde_json::Value>,
}

#[derive(Debug, Serialize)]
struct DriftReport {
    standard_id: String,
    previous: DriftState,
    current: DriftState,
    unchanged_supporting: usize,
    supporting_total: usize,
}

#[derive(Debug, Serialize)]
struct AuditReport {
    ok: bool,
    unresolved_conflicts: Vec<String>,
    drift_failures: Vec<String>,
}

fn index_path(workspace: &Path) -> PathBuf {
    workspace.join(RELATIVE_INDEX)
}

fn load(workspace: &Path) -> Result<StandardsDocument, String> {
    let path = index_path(workspace);
    let bytes = fs::read(&path).map_err(|error| format!("failed to read {}: {error}", path.display()))?;
    let document: StandardsDocument = serde_json::from_slice(&bytes)
        .map_err(|error| format!("failed to parse {}: {error}", path.display()))?;
    if document.schema_version != 1 {
        return Err(format!("unsupported standards schema version {}", document.schema_version));
    }
    Ok(document)
}

fn save(workspace: &Path, document: &StandardsDocument) -> Result<(), String> {
    let path = index_path(workspace);
    let parent = path.parent().ok_or_else(|| "invalid standards path".to_string())?;
    fs::create_dir_all(parent).map_err(|error| format!("failed to create {}: {error}", parent.display()))?;
    let mut bytes = serde_json::to_vec_pretty(document).map_err(|error| error.to_string())?;
    bytes.push(b'\n');
    let temporary = path.with_extension("json.tmp");
    fs::write(&temporary, bytes).map_err(|error| format!("failed to write {}: {error}", temporary.display()))?;
    fs::rename(&temporary, &path).map_err(|error| format!("failed to replace {}: {error}", path.display()))
}

fn sha256_file(path: &Path) -> Result<String, String> {
    let bytes = fs::read(path).map_err(|error| format!("{}: {error}", path.display()))?;
    Ok(format!("{:x}", Sha256::digest(bytes)))
}

fn compute_drift(workspace: &Path, standard: &Standard) -> DriftReport {
    let supporting: Vec<&Evidence> = standard.evidence.iter().filter(|evidence| evidence.supports).collect();
    let unchanged = supporting.iter().filter(|evidence| {
        let evidence_path = workspace.join(&evidence.path);
        sha256_file(&evidence_path).map(|digest| digest.eq_ignore_ascii_case(&evidence.sha256)).unwrap_or(false)
    }).count();
    let current = if supporting.is_empty() {
        DriftState::Unknown
    } else if unchanged == supporting.len() {
        DriftState::Healthy
    } else if unchanged == 0 {
        DriftState::Contradicted
    } else {
        DriftState::Weakened
    };
    DriftReport {
        standard_id: standard.standard_id.clone(),
        previous: standard.drift,
        current,
        unchanged_supporting: unchanged,
        supporting_total: supporting.len(),
    }
}

fn now_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis() as u64
}

fn audit(document: &StandardsDocument) -> AuditReport {
    use std::collections::BTreeSet;
    let approved: BTreeSet<&str> = document.standards.iter()
        .filter(|standard| standard.status == LifecycleStatus::Approved)
        .map(|standard| standard.standard_id.as_str())
        .collect();
    let mut conflicts = BTreeSet::new();
    let mut drift = BTreeSet::new();
    for standard in &document.standards {
        if standard.status == LifecycleStatus::Approved {
            for other in &standard.conflicts_with {
                if approved.contains(other.as_str()) {
                    conflicts.insert(format!("{} <-> {}", standard.standard_id, other));
                }
            }
            if matches!(standard.drift, DriftState::Contradicted) {
                drift.insert(format!("{}: contradicted", standard.standard_id));
            }
        }
        if standard.status == LifecycleStatus::Stale {
            drift.insert(format!("{}: stale", standard.standard_id));
        }
    }
    AuditReport {
        ok: conflicts.is_empty() && drift.is_empty(),
        unresolved_conflicts: conflicts.into_iter().collect(),
        drift_failures: drift.into_iter().collect(),
    }
}

fn run(cli: Cli) -> Result<ExitCode, String> {
    let workspace = cli.workspace.canonicalize().map_err(|error| format!("invalid workspace {}: {error}", cli.workspace.display()))?;
    let mut document = load(&workspace)?;
    match cli.command {
        Command::List { status, json } => {
            let standards: Vec<&Standard> = document.standards.iter().filter(|standard| status.map(|wanted| standard.status == wanted).unwrap_or(true)).collect();
            if json {
                println!("{}", serde_json::to_string_pretty(&standards).map_err(|error| error.to_string())?);
            } else {
                for standard in standards {
                    println!("{}@v{}\t{:?}\t{:?}\t{}", standard.standard_id, standard.version, standard.status, standard.drift, standard.title);
                }
            }
            Ok(ExitCode::SUCCESS)
        }
        Command::SetStatus { standard_id, status } => {
            let standard = document.standards.iter_mut().find(|standard| standard.standard_id == standard_id)
                .ok_or_else(|| format!("unknown standard id {standard_id}"))?;
            standard.status = status.into();
            standard.approved_at_ms = if standard.status == LifecycleStatus::Approved { Some(now_ms()) } else { standard.approved_at_ms };
            document.generated_at_ms = now_ms();
            save(&workspace, &document)?;
            println!("{} -> {:?}", standard_id, LifecycleStatus::from(status));
            Ok(ExitCode::SUCCESS)
        }
        Command::Drift { write, json } => {
            let reports: Vec<DriftReport> = document.standards.iter().map(|standard| compute_drift(&workspace, standard)).collect();
            if write {
                let verified_at = now_ms();
                for (standard, report) in document.standards.iter_mut().zip(&reports) {
                    standard.drift = report.current;
                    standard.last_verified_at_ms = Some(verified_at);
                    if standard.status == LifecycleStatus::Approved && report.current == DriftState::Contradicted {
                        standard.status = LifecycleStatus::Stale;
                    }
                }
                document.generated_at_ms = verified_at;
                save(&workspace, &document)?;
            }
            if json {
                println!("{}", serde_json::to_string_pretty(&reports).map_err(|error| error.to_string())?);
            } else {
                for report in reports {
                    println!("{}\t{:?} -> {:?}\t{}/{} supporting evidence unchanged", report.standard_id, report.previous, report.current, report.unchanged_supporting, report.supporting_total);
                }
            }
            Ok(ExitCode::SUCCESS)
        }
        Command::Audit { json } => {
            let report = audit(&document);
            if json {
                println!("{}", serde_json::to_string_pretty(&report).map_err(|error| error.to_string())?);
            } else if report.ok {
                println!("Standards audit passed ({} standards).", document.standards.len());
            } else {
                for conflict in &report.unresolved_conflicts { eprintln!("conflict: {conflict}"); }
                for failure in &report.drift_failures { eprintln!("drift: {failure}"); }
            }
            Ok(if report.ok { ExitCode::SUCCESS } else { ExitCode::from(2) })
        }
    }
}

fn main() -> ExitCode {
    match run(Cli::parse()) {
        Ok(code) => code,
        Err(error) => {
            eprintln!("little-monkey-standards: {error}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn audit_rejects_approved_conflict() {
        let standard = |id: &str, conflicts_with: Vec<String>| Standard {
            standard_id: id.to_string(), version: 1, title: id.to_string(), body: "body".to_string(),
            status: LifecycleStatus::Approved, evidence: vec![], conflicts_with, content_sha256: "a".repeat(64),
            drift: DriftState::Healthy, approved_at_ms: Some(1), last_verified_at_ms: None, extra: Default::default(),
        };
        let document = StandardsDocument {
            schema_version: 1, workspace_id: "test".to_string(), generated_at_ms: 1,
            standards: vec![standard("one", vec!["two".to_string()]), standard("two", vec![])],
        };
        assert!(!audit(&document).ok);
    }
}
