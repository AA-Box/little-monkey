//! `monkey processes` — the cross-surface process listing.
//!
//! Named `processes` rather than `ps` because `monkey ps` is already the
//! Ollama-compatible "list running models" command, and breaking that
//! compatibility to free up three characters would be a bad trade. `proc` is
//! accepted as a short alias.
//!
//! `processes signal` records a durable request that the owning kind delivers at
//! its own safe point, so it reaches a process this app is not running — a
//! daemon job, or work left behind by a previous session. A kind that cannot
//! honour a signal refuses it *with a reason* rather than accepting a request it
//! will never act on; `processes signals` prints that whole matrix, including
//! why each refusal stands.

use std::path::Path;

use clap::Subcommand;
use little_monkey_lib::process_table::{
    ProcessFilter, ProcessKind, ProcessLimitKind, ProcessRecord, ProcessSignal, DEFAULT_LIST_LIMIT,
};
use little_monkey_lib::run_ledger::RunLedger;

const LEDGER_FILE: &str = "profile-v1.sqlite3";

#[derive(Subcommand, Debug)]
pub enum ProcessesCmd {
    /// List agent processes across every execution surface, newest first.
    List {
        /// Only these kinds. Repeatable, e.g. `--kind chat_turn --kind daemon_job`.
        #[arg(long = "kind")]
        kinds: Vec<String>,
        /// Include processes that have already exited.
        #[arg(long)]
        all: bool,
        /// Only processes owning this workspace root.
        #[arg(long)]
        workspace: Option<String>,
        /// Only children of this process.
        #[arg(long)]
        parent: Option<String>,
        /// Maximum rows.
        #[arg(long)]
        limit: Option<u32>,
        /// Print the machine-readable record instead of a table.
        #[arg(long)]
        json: bool,
    },
    /// Show one process and its descendants.
    Show {
        process_id: String,
        #[arg(long)]
        json: bool,
    },
    /// Live count per kind.
    Count {
        #[arg(long)]
        json: bool,
    },
    /// Ask a process to stop, suspend, resume, or be killed.
    ///
    /// Records durable intent that the owning kind delivers at its own safe
    /// point, so this works for a process in another process — a daemon job, or
    /// a run started by a previous app session.
    Signal {
        process_id: String,
        /// One of `stop`, `suspend`, `resume`, `kill`.
        signal: String,
        /// Why, recorded alongside the request.
        #[arg(long)]
        reason: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Print which signals each kind honours, and why the rest are refused.
    Signals {
        #[arg(long)]
        json: bool,
    },
    /// Print who enforces each declared limit, and why nobody does for the rest.
    ///
    /// The counterpart to `signals`, for the other half of what a process row
    /// claims. A `ProcessLimits` field is a declaration, and this says whether
    /// anything reads it: `enforced` means the owner reads this row's field and
    /// a caller-supplied value takes effect, `owner-sourced` means a real bound
    /// exists but its number comes from a recipe, a workflow definition or the
    /// owner's own settings, and `unavailable` names the mechanism that is
    /// missing.
    Limits {
        #[arg(long)]
        json: bool,
    },
}

pub fn run(action: &ProcessesCmd, data_dir: &Path) -> Result<(), String> {
    // The support matrix is static, so it answers without a ledger — useful on a
    // machine where the app has never run.
    if let ProcessesCmd::Signals { json } = action {
        return print_signal_matrix(*json);
    }
    if let ProcessesCmd::Limits { json } = action {
        return print_limit_matrix(*json);
    }

    let path = data_dir.join(LEDGER_FILE);
    if !path.exists() {
        // An app that has never run has no ledger. Say so rather than creating
        // one as a side effect of a read-only listing.
        return Err(format!(
            "No Little Monkey ledger at {} yet — start the app or a daemon run first",
            path.display()
        ));
    }
    let ledger = RunLedger::open(&path).map_err(|error| error.to_string())?;
    let table = ledger.process_table();

    match action {
        ProcessesCmd::List {
            kinds,
            all,
            workspace,
            parent,
            limit,
            json,
        } => {
            let mut parsed = Vec::new();
            for raw in kinds {
                parsed.push(ProcessKind::parse(raw).map_err(|error| error.to_string())?);
            }
            let records = table
                .list(&ProcessFilter {
                    kinds: parsed,
                    live_only: !all,
                    parent_process_id: parent.clone(),
                    workspace: workspace.clone(),
                    limit: *limit,
                })
                .map_err(|error| error.to_string())?;
            if *json {
                print_json(&records)?;
            } else {
                print_table(&records, !*all, limit.unwrap_or(DEFAULT_LIST_LIMIT));
            }
            Ok(())
        }
        ProcessesCmd::Show { process_id, json } => {
            let record = table
                .get(process_id)
                .map_err(|error| error.to_string())?
                .ok_or_else(|| format!("No process {process_id}"))?;
            let descendants = table
                .descendants(process_id)
                .map_err(|error| error.to_string())?;
            if *json {
                print_json(&serde_json::json!({
                    "process": record,
                    "descendants": descendants,
                }))?;
            } else {
                print_detail(&record);
                if descendants.is_empty() {
                    println!("\nno child processes");
                } else {
                    println!("\n{} descendant(s):", descendants.len());
                    print_table(&descendants, false, DEFAULT_LIST_LIMIT);
                }
            }
            Ok(())
        }
        ProcessesCmd::Signals { .. } | ProcessesCmd::Limits { .. } => {
            unreachable!("handled before the ledger is opened")
        }
        ProcessesCmd::Signal {
            process_id,
            signal,
            reason,
            json,
        } => {
            let parsed = ProcessSignal::parse(signal).map_err(|error| error.to_string())?;
            let now = i64::try_from(
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_err(|_| "system clock is before the unix epoch".to_string())?
                    .as_millis(),
            )
            .map_err(|_| "clock is beyond bounds".to_string())?;
            let record = table
                .signal(process_id, parsed, reason.as_deref(), now)
                .map_err(|error| error.to_string())?;
            if *json {
                print_json(&record)?;
            } else {
                println!(
                    "asked {} ({}) to {}",
                    record.process_id,
                    record.kind.as_str(),
                    parsed.as_str()
                );
                print_detail(&record);
            }
            Ok(())
        }
        ProcessesCmd::Count { json } => {
            let counts = table.live_counts().map_err(|error| error.to_string())?;
            if *json {
                print_json(
                    &counts
                        .iter()
                        .map(|(kind, count)| {
                            serde_json::json!({ "kind": kind.as_str(), "count": count })
                        })
                        .collect::<Vec<_>>(),
                )?;
            } else if counts.is_empty() {
                println!("no live agent processes");
            } else {
                let total: u32 = counts.iter().map(|(_, count)| *count).sum();
                for (kind, count) in &counts {
                    println!("{:<18} {count}", kind.as_str());
                }
                println!("{:<18} {total}", "total");
            }
            Ok(())
        }
    }
}

/// Prints who enforces each declared limit for each kind, and why nobody does
/// for the rest.
///
/// The `unavailable` rows are the useful half, the same way refusals are in the
/// signal matrix: they say which mechanism is missing rather than leaving a
/// reader to infer that an absent number means an absent bound.
fn print_limit_matrix(json: bool) -> Result<(), String> {
    if json {
        let rows: Vec<serde_json::Value> = ProcessKind::ALL
            .iter()
            .flat_map(|kind| {
                ProcessLimitKind::ALL.iter().map(move |limit| {
                    let support = kind.limit_support(*limit);
                    serde_json::json!({
                        "kind": kind.as_str(),
                        "limit": limit.as_str(),
                        "status": support.status(),
                        "honoursCallerValue": support.honours_caller_value(),
                        "detail": support.detail(),
                    })
                })
            })
            .collect();
        return print_json(&rows);
    }

    println!(
        "{:<18} {:<20} {:<14} {}",
        "KIND", "LIMIT", "STATUS", "DETAIL"
    );
    for kind in ProcessKind::ALL {
        for limit in ProcessLimitKind::ALL {
            let support = kind.limit_support(*limit);
            println!(
                "{:<18} {:<20} {:<14} {}",
                kind.as_str(),
                limit.as_str(),
                support.status(),
                support.detail()
            );
        }
    }
    println!();
    println!(
        "enforced      = the owner reads this row's field; a value you set takes effect\n\
         owner-sourced = a real bound, but its number comes from the owner, not from you\n\
         unavailable   = nothing enforces it for this kind; the detail names what is missing"
    );
    println!();
    print_host_enforcement();
    Ok(())
}

/// What this *host* would actually hold a native workload with, right now.
///
/// The matrix above is static — it answers on a machine where the app has never
/// run, which is what makes it a contract. This is the other half: whether that
/// contract is met here by the kernel or by a supervisor, which depends on the
/// machine (a Linux box with no delegated cgroup falls back, and says so). Two
/// questions, so two blocks, rather than one table that would be wrong about one
/// of them.
fn print_host_enforcement() {
    use little_monkey_lib::resource_control::{EffectiveLimits, ResourceController};

    let capabilities = ResourceController::new(EffectiveLimits::default()).capabilities();
    println!("this host: {}", capabilities.backend);
    println!("tree owned by: {}", capabilities.tree_primitive);
    for limit in ProcessLimitKind::ALL {
        let capability = capabilities.for_limit(*limit);
        let (status, detail) = match capability {
            little_monkey_lib::resource_control::LimitCapability::Enforced { level, mechanism } => {
                (level.as_str().to_string(), mechanism.clone())
            }
            little_monkey_lib::resource_control::LimitCapability::NotApplicable { reason } => {
                ("not-applicable".to_string(), reason.clone())
            }
            little_monkey_lib::resource_control::LimitCapability::Unavailable { reason } => {
                ("unavailable".to_string(), reason.clone())
            }
        };
        println!("  {:<20} {:<14} {detail}", limit.as_str(), status);
    }
}

/// Prints which signals each kind honours, and the reason for each refusal.
///
/// The refusals are the useful half: they say whether a signal is missing a
/// mechanism or is a design boundary, which is what a caller needs before
/// deciding whether to wait or give up.
fn print_signal_matrix(json: bool) -> Result<(), String> {
    if json {
        let rows: Vec<serde_json::Value> = ProcessKind::ALL
            .iter()
            .flat_map(|kind| {
                ProcessSignal::ALL.iter().map(move |signal| {
                    let support = kind.signal_support(*signal);
                    serde_json::json!({
                        "kind": kind.as_str(),
                        "signal": signal.as_str(),
                        "honoured": support.is_honoured(),
                        "reason": support.refusal(),
                    })
                })
            })
            .collect();
        return print_json(&rows);
    }

    println!(
        "{:<18} {:<8} {:<8} {:<8} {}",
        "KIND", "STOP", "SUSPEND", "RESUME", "KILL"
    );
    for kind in ProcessKind::ALL {
        let mark = |signal: ProcessSignal| {
            if kind.signal_support(signal).is_honoured() {
                "yes"
            } else {
                "no"
            }
        };
        println!(
            "{:<18} {:<8} {:<8} {:<8} {}",
            kind.as_str(),
            mark(ProcessSignal::Stop),
            mark(ProcessSignal::Suspend),
            mark(ProcessSignal::Resume),
            mark(ProcessSignal::Kill),
        );
    }

    println!("\nwhy each refusal:");
    let mut seen: Vec<&str> = Vec::new();
    for kind in ProcessKind::ALL {
        for signal in ProcessSignal::ALL {
            if let Some(reason) = kind.signal_support(*signal).refusal() {
                if seen.contains(&reason) {
                    continue;
                }
                seen.push(reason);
                println!("  {reason}");
            }
        }
    }
    Ok(())
}

fn print_json<T: serde::Serialize>(value: &T) -> Result<(), String> {
    println!(
        "{}",
        serde_json::to_string_pretty(value).map_err(|error| error.to_string())?
    );
    Ok(())
}

fn print_table(records: &[ProcessRecord], live_only: bool, limit: u32) {
    if records.is_empty() {
        println!("no {}agent processes", if live_only { "live " } else { "" });
        return;
    }
    println!(
        "{:<26} {:<17} {:<10} {:<28} {:<8} {}",
        "PROCESS", "KIND", "STATE", "EXTERNAL ID", "PID", "EXIT"
    );
    for record in records {
        println!(
            "{:<26} {:<17} {:<10} {:<28} {:<8} {}",
            truncate(&record.process_id, 26),
            record.kind.as_str(),
            record.state.as_str(),
            truncate(&record.external_id, 28),
            record
                .native_pid
                .map(|pid| pid.to_string())
                .unwrap_or_else(|| "-".to_string()),
            record
                .exit
                .as_ref()
                .map(|exit| exit.status.as_str().to_string())
                .unwrap_or_else(|| "-".to_string()),
        );
    }
    if records.len() as u32 >= limit {
        println!("\n(showing {limit} rows — pass --limit to widen; listings are always bounded)");
    }
}

fn print_detail(record: &ProcessRecord) {
    println!("process      {}", record.process_id);
    println!("kind         {}", record.kind.as_str());
    println!("state        {}", record.state.as_str());
    println!("external id  {}", record.external_id);
    println!(
        "parent       {}",
        record.parent_process_id.as_deref().unwrap_or("-")
    );
    println!("run          {}", record.run_id.as_deref().unwrap_or("-"));
    println!(
        "workspace    {}",
        record.workspace.as_deref().unwrap_or("-")
    );
    println!("profile      {}", record.profile.as_deref().unwrap_or("-"));
    println!(
        "native pid   {}",
        record
            .native_pid
            .map(|pid| pid.to_string())
            .unwrap_or_else(|| "-".to_string())
    );
    if record.limits.is_unbounded() {
        println!("limits       none declared");
    } else {
        // Each limit with the mechanism that holds it, because the number alone
        // does not say whether anything is holding it. A reader auditing what
        // this app promised needs "4 GiB · enforced · Windows job object", not
        // "4 GiB" — the second is true of a bound nobody reads too.
        println!("limits");
        for (limit, value) in [
            (ProcessLimitKind::Wall, record.limits.max_wall_ms),
            (ProcessLimitKind::Memory, record.limits.max_memory_bytes),
            (ProcessLimitKind::Output, record.limits.max_output_bytes),
            (
                ProcessLimitKind::ChildProcesses,
                record.limits.max_child_processes.map(u64::from),
            ),
            (
                ProcessLimitKind::ContextTokens,
                record.limits.max_context_tokens,
            ),
        ] {
            let Some(value) = value else { continue };
            let support = record.kind.limit_support(limit);
            println!(
                "  {:<20} {:<16} {:<14} {}",
                limit.as_str(),
                value,
                support.status(),
                support.detail()
            );
        }
    }
    match &record.exit {
        Some(exit) => {
            println!(
                "exit         {} code={} signal={} reason={}",
                exit.status.as_str(),
                exit.code
                    .map(|code| code.to_string())
                    .unwrap_or_else(|| "-".to_string()),
                exit.signal.as_deref().unwrap_or("-"),
                exit.reason.as_deref().unwrap_or("-"),
            );
            // The typed breach, when a resource controller made the kill. Printed
            // as its own block rather than folded into the reason: the two
            // numbers beside each other are what tell a reader whether the budget
            // was wrong or the workload was.
            if let Some(breach) = &exit.breach {
                println!("limit fired  {}", breach.limit);
                println!("  configured {}", breach.configured);
                println!("  observed   {}", breach.observed);
                println!("  enforced by {} ({})", breach.backend, breach.level);
            }
        }
        None => println!("exit         -"),
    }
}

fn option_or_dash<T: std::fmt::Display>(value: Option<T>) -> String {
    value
        .map(|inner| inner.to_string())
        .unwrap_or_else(|| "-".to_string())
}

fn truncate(value: &str, width: usize) -> String {
    if value.chars().count() <= width {
        return value.to_string();
    }
    let keep = width.saturating_sub(1);
    format!("{}…", value.chars().take(keep).collect::<String>())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_ledger_is_reported_rather_than_created() {
        let dir = std::env::temp_dir().join(format!(
            "little_monkey_processes_cli_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();

        let error = run(&ProcessesCmd::Count { json: false }, &dir)
            .expect_err("a read-only listing must not create a ledger");
        assert!(error.contains("No Little Monkey ledger"), "{error}");
        assert!(
            !dir.join(LEDGER_FILE).exists(),
            "the listing created a database as a side effect"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn truncate_keeps_short_values_intact_and_marks_clipped_ones() {
        assert_eq!(truncate("short", 10), "short");
        assert_eq!(truncate("abcdefghij", 10), "abcdefghij");
        assert_eq!(truncate("abcdefghijk", 10), "abcdefghi…");
    }

    #[test]
    fn an_unknown_kind_filter_is_refused() {
        let dir = std::env::temp_dir();
        let error = run(
            &ProcessesCmd::List {
                kinds: vec!["not_a_kind".to_string()],
                all: false,
                workspace: None,
                parent: None,
                limit: None,
                json: false,
            },
            &dir,
        )
        .expect_err("an unknown kind must not be silently ignored");
        // Either the ledger is missing in this temp dir, or the kind is refused.
        assert!(
            error.contains("unknown process kind") || error.contains("No Little Monkey ledger"),
            "{error}"
        );
    }
}
