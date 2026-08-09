//! Writing to the unified subsystem event stream, from any of the three places
//! that need to (roadmap K12).
//!
//! # Why this exists rather than a helper per subsystem
//!
//! `subsystem_events` (migration V12) is the one stream the run-less
//! subsystems write to. Every writer has to do the same four things — mint an
//! event id, read the clock, resolve what the action belongs to, and append
//! without letting a bookkeeping failure break the action. Four copies of that
//! would drift, and the field most likely to drift is the attribution, which is
//! the field the audit is *for*.
//!
//! # The three contexts are genuinely different, and that is what shapes this
//!
//! They do not differ by taste; they differ by what they can reach.
//!
//! - **Desktop** has a `tauri::AppHandle`, and the ledger lives behind
//!   `AppState`, opened lazily by `run_commands::with_ledger`.
//! - **A process that owns its data directory** — the CLI-hosted API server, the
//!   daemon, ACP — has no `AppState` at all, only a path. It opens the ledger
//!   itself.
//! - **Tests and any context with no ledger** must record nothing, and must say
//!   why rather than looking like a context that recorded and found nothing.
//!
//! That third case is why [`SubsystemAudit::disabled`] takes a reason. A silent
//! no-op would make "this subsystem wrote no events" indistinguishable from
//! "this subsystem was never wired up", which is exactly the ambiguity
//! `run_scope`'s two-arm design exists to prevent.
//!
//! # A failed append never fails the action
//!
//! Every [`record`](SubsystemAudit::record) call logs and returns. Failing an
//! MCP call or an HTTP request that already succeeded, because its audit row
//! could not be written, turns a bookkeeping problem into a user-visible one.
//!
//! This is safe **only because the security-relevant half is written
//! elsewhere and does fail closed**: a gated action's authorization is recorded
//! by `permissions.rs` into `permission_decisions` *before* the action runs, and
//! that path propagates its errors. This stream records what happened, not what
//! was permitted, and the two are deliberately not the same table.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use crate::run_ledger::{
    PermissionAttribution, RunLedger, Subsystem, SubsystemEvent, SubsystemOutcome,
};
use crate::AppState;

/// One thing a subsystem did, as its caller knows it.
///
/// `turn_id` is the run the caller believes it is in, not the attribution:
/// resolving that needs a ledger, and which ledger depends on the context, so
/// [`SubsystemAudit::record`] does it.
pub struct SubsystemAction<'a> {
    pub subsystem: Subsystem,
    /// What was done, in the subsystem's own vocabulary — an MCP `server:tool`,
    /// an HTTP method and route, a browser action.
    pub action: String,
    /// The run the caller was working for, if it knows one.
    pub turn_id: Option<&'a str>,
    /// The `permission_decisions` row that authorized this. `None` means nothing
    /// gated it, which is a finding rather than a blank.
    pub permission_request_id: Option<&'a str>,
    pub outcome: SubsystemOutcome,
    /// Subsystem-specific detail. Covered by the chain, so it cannot be edited
    /// after the fact — keep secrets out of it, the same rule
    /// `redacted_tool_arguments` follows.
    pub detail: Option<serde_json::Value>,
}

/// How an HTTP-style response status reads as an outcome.
///
/// Lives here rather than beside either caller because `server.rs` and the
/// remote node's `RemoteApi` both answer HTTP and must classify it the same way
/// — two copies of this would drift, and the drift would be invisible until
/// somebody counted failures and got refusals.
///
/// `Denied` is kept apart from `Failed` for the reason [`SubsystemOutcome`]
/// gives: a refusal and an error are different findings. Rate limiting is the
/// server failing the caller rather than refusing them on policy, so it is
/// `Failed`.
#[must_use]
pub fn outcome_for_status(status: u16) -> SubsystemOutcome {
    match status {
        200..=299 => SubsystemOutcome::Succeeded,
        401 | 403 => SubsystemOutcome::Denied,
        _ => SubsystemOutcome::Failed,
    }
}

/// The subsystem chain's linkage, with no event contents at all.
/// See [`SubsystemAudit::chain_evidence`].
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChainEvidence {
    pub verification: crate::run_ledger::ChainVerification,
    pub head: Option<crate::run_ledger::ChainLink>,
    pub links_after: Vec<crate::run_ledger::ChainLink>,
}

/// Where a subsystem's events go.
#[derive(Clone)]
pub struct SubsystemAudit {
    target: AuditTarget,
}

#[derive(Clone)]
enum AuditTarget {
    /// The ledger behind `AppState`, reached through the app handle.
    Desktop(tauri::AppHandle),
    /// A ledger this process opens itself, kept open after the first append
    /// rather than reopened per event.
    Ledger {
        path: PathBuf,
        opened: Arc<Mutex<Option<RunLedger>>>,
    },
    /// Deliberately not recording, with the reason named.
    Disabled(&'static str),
}

impl SubsystemAudit {
    #[must_use]
    pub fn desktop(app: tauri::AppHandle) -> Self {
        SubsystemAudit {
            target: AuditTarget::Desktop(app),
        }
    }

    /// Record into the ledger under `app_data_dir`, opening it on first use.
    #[must_use]
    pub fn in_data_dir(app_data_dir: &std::path::Path) -> Self {
        SubsystemAudit {
            target: AuditTarget::Ledger {
                path: app_data_dir.join(crate::run_commands::DATABASE_FILE),
                opened: Arc::new(Mutex::new(None)),
            },
        }
    }

    /// Record nothing, and say why. The reason is `'static` on purpose: it
    /// describes the *context*, which is known when the audit is constructed,
    /// not something derived from a request.
    #[must_use]
    pub fn disabled(reason: &'static str) -> Self {
        SubsystemAudit {
            target: AuditTarget::Disabled(reason),
        }
    }

    /// Whether this audit would write anything. Only for tests and diagnostics —
    /// callers must not branch on it, since "recording" is not conditional
    /// behaviour they get to skip.
    #[must_use]
    pub fn is_recording(&self) -> bool {
        !matches!(self.target, AuditTarget::Disabled(_))
    }

    /// One line saying where events go, for a startup log.
    ///
    /// This is what makes [`disabled`](Self::disabled)'s reason load-bearing
    /// rather than a comment wearing a type: an operator running a listener that
    /// is not auditing should be told, at the moment it starts, instead of
    /// discovering an empty stream later and having to guess whether nothing
    /// happened or nothing was recorded.
    #[must_use]
    pub fn describe(&self) -> String {
        match &self.target {
            AuditTarget::Desktop(_) => "recording to the app's run ledger".to_string(),
            AuditTarget::Ledger { path, .. } => format!("recording to {}", path.display()),
            AuditTarget::Disabled(reason) => format!("NOT recording — {reason}"),
        }
    }

    /// Read the subsystem chain's linkage: its verification state, its head,
    /// and the links after `after_sequence` (roadmap K21).
    ///
    /// `Ok(None)` means this context is [`disabled`](Self::disabled) — the same
    /// distinction the rest of this module keeps, so a conformance run reports
    /// "this node exposes no ledger evidence" rather than "the ledger is
    /// empty". Hashes only, never contents: `detail_json` may hold the user's
    /// own text and is covered by the chain, so it is permanent.
    ///
    /// Unlike [`record`](Self::record), a failure here is returned. Nothing is
    /// riding on it — no action has already succeeded that a refusal would
    /// retroactively spoil — and a conformance claim built on a swallowed read
    /// error would be worthless.
    pub fn chain_evidence(
        &self,
        after_sequence: u64,
        limit: u32,
    ) -> Result<Option<ChainEvidence>, String> {
        // ponytail: `verify_subsystem_chain` walks the whole stream. That is
        // the point — a partial verification is not one — and this runs once
        // per attestation read, not per request.
        let read = |ledger: &RunLedger| -> Result<ChainEvidence, String> {
            Ok(ChainEvidence {
                verification: ledger
                    .verify_subsystem_chain()
                    .map_err(|error| error.to_string())?,
                head: ledger
                    .subsystem_chain_head()
                    .map_err(|error| error.to_string())?,
                links_after: ledger
                    .subsystem_chain_links(after_sequence, limit)
                    .map_err(|error| error.to_string())?,
            })
        };
        match &self.target {
            AuditTarget::Disabled(_) => Ok(None),
            AuditTarget::Desktop(app) => {
                use tauri::Manager as _;
                let state = app.state::<AppState>();
                crate::run_commands::with_ledger(app, state.inner(), |ledger| Ok(read(ledger)))?
                    .map(Some)
            }
            AuditTarget::Ledger { path, opened } => {
                let mut slot = opened
                    .lock()
                    .map_err(|_| "Subsystem audit ledger lock was poisoned".to_string())?;
                if slot.is_none() {
                    *slot = Some(RunLedger::open(path).map_err(|error| error.to_string())?);
                }
                read(slot.as_ref().expect("ledger initialized")).map(Some)
            }
        }
    }

    /// Append one event, logging rather than failing. See the module docs for
    /// why a failure here must not fail the action.
    pub fn record(&self, action: SubsystemAction<'_>) {
        if let AuditTarget::Disabled(_) = self.target {
            return;
        }
        let subsystem = action.subsystem;
        if let Err(error) = self.append(action) {
            eprintln!(
                "{} action completed but was not recorded in the subsystem event stream: {error}",
                subsystem.code()
            );
        }
    }

    fn append(&self, action: SubsystemAction<'_>) -> Result<(), String> {
        let occurred_at_ms = crate::run_commands::unix_time_ms()?;
        let detail_json = action
            .detail
            .as_ref()
            .map(serde_json::to_vec)
            .transpose()
            .map_err(|error| format!("Failed to encode subsystem event detail: {error}"))?;
        let process_id =
            crate::run_scope::current_process().map(|process| process.process_id().to_string());

        let build = |run_id: Option<String>, attribution: PermissionAttribution| SubsystemEvent {
            event_id: format!("subsystem-{}", uuid::Uuid::new_v4().simple()),
            subsystem: action.subsystem,
            action: action.action.clone(),
            occurred_at_ms,
            run_id,
            attribution,
            process_id: process_id.clone(),
            permission_request_id: action.permission_request_id.map(str::to_string),
            outcome: action.outcome,
            detail_json,
        };

        match &self.target {
            AuditTarget::Disabled(_) => Ok(()),
            AuditTarget::Desktop(app) => {
                use tauri::Manager as _;
                let state = app.state::<AppState>();
                let (run_id, attribution) =
                    crate::permissions::permission_attribution(app, state.inner(), action.turn_id)?;
                let event = build(run_id, attribution);
                crate::run_commands::with_ledger(app, state.inner(), |ledger| {
                    ledger.append_subsystem_event(&event).map(|_| ())
                })
            }
            AuditTarget::Ledger { path, opened } => {
                let mut slot = opened
                    .lock()
                    .map_err(|_| "Subsystem audit ledger lock was poisoned".to_string())?;
                if slot.is_none() {
                    *slot = Some(RunLedger::open(path).map_err(|error| error.to_string())?);
                }
                let ledger = slot.as_mut().expect("ledger initialized");
                // Resolved against the ledger this process owns, so
                // `ledger-run` still means "the ledger holds it" and not a guess.
                let (run_id, attribution) =
                    crate::permissions::attribution_for(action.turn_id, |run_id| {
                        ledger
                            .load_run(run_id)
                            .map(|run| run.is_some())
                            .map_err(|error| error.to_string())
                    })?;
                let event = build(run_id, attribution);
                ledger
                    .append_subsystem_event(&event)
                    .map(|_| ())
                    .map_err(|error| error.to_string())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn action(outcome: SubsystemOutcome) -> SubsystemAction<'static> {
        SubsystemAction {
            subsystem: Subsystem::Http,
            action: "POST /v1/chat/completions".to_string(),
            turn_id: None,
            permission_request_id: None,
            outcome,
            detail: None,
        }
    }

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(label: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "little-monkey-subsystem-audit-{label}-{}-{:?}",
                std::process::id(),
                std::thread::current().id()
            ));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).unwrap();
            TempDir(path)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// The path a process that owns its data directory takes — the CLI-hosted
    /// API server, the daemon, ACP. No `AppState` is involved.
    #[test]
    fn a_process_that_owns_its_data_dir_appends_to_its_own_ledger() {
        let directory = TempDir::new("owns-dir");
        let audit = SubsystemAudit::in_data_dir(&directory.0);
        assert!(audit.is_recording());

        audit.record(action(SubsystemOutcome::Succeeded));
        audit.record(action(SubsystemOutcome::Failed));

        let ledger = RunLedger::open(directory.0.join(crate::run_commands::DATABASE_FILE)).unwrap();
        let events = ledger.recent_subsystem_events(None, 10).unwrap();
        assert_eq!(events.len(), 2, "both events were appended");
        assert_eq!(events[0].outcome, SubsystemOutcome::Failed, "newest first");
        assert_eq!(events[0].subsystem, Subsystem::Http);
        assert_eq!(
            events[0].attribution,
            PermissionAttribution::Unknown,
            "no turn and no ambient scope is 'nobody told us', not a guess"
        );
        assert!(
            matches!(
                ledger.verify_subsystem_chain().unwrap(),
                crate::run_ledger::ChainVerification::Intact { events_seen: 2, .. }
            ),
            "two appends through one audit must still chain"
        );
    }

    /// The ledger is opened once and kept, not reopened per event — an audit on
    /// an HTTP path would otherwise pay a SQLite open per request.
    #[test]
    fn the_ledger_is_opened_once_and_reused() {
        let directory = TempDir::new("reuse");
        let audit = SubsystemAudit::in_data_dir(&directory.0);
        audit.record(action(SubsystemOutcome::Succeeded));

        let AuditTarget::Ledger { opened, .. } = &audit.target else {
            panic!("expected a path-backed audit");
        };
        assert!(
            opened.lock().unwrap().is_some(),
            "the first append must leave the ledger open"
        );

        audit.record(action(SubsystemOutcome::Succeeded));
        let ledger = RunLedger::open(directory.0.join(crate::run_commands::DATABASE_FILE)).unwrap();
        assert_eq!(ledger.recent_subsystem_events(None, 10).unwrap().len(), 2);
    }

    /// One status rule, two HTTP-answering callers (`server.rs` and the remote
    /// node). Two copies would drift, and the drift would be invisible until
    /// somebody counted failures and got refusals.
    #[test]
    fn a_refusal_is_not_a_failure() {
        assert_eq!(outcome_for_status(200), SubsystemOutcome::Succeeded);
        assert_eq!(outcome_for_status(204), SubsystemOutcome::Succeeded);
        assert_eq!(outcome_for_status(299), SubsystemOutcome::Succeeded);
        assert_eq!(outcome_for_status(401), SubsystemOutcome::Denied);
        assert_eq!(outcome_for_status(403), SubsystemOutcome::Denied);
        assert_eq!(
            outcome_for_status(429),
            SubsystemOutcome::Failed,
            "rate limiting is the server failing the caller, not refusing them on policy"
        );
        assert_eq!(outcome_for_status(404), SubsystemOutcome::Failed);
        assert_eq!(outcome_for_status(500), SubsystemOutcome::Failed);
        assert_eq!(
            outcome_for_status(302),
            SubsystemOutcome::Failed,
            "a redirect is not a completed action for these APIs"
        );
    }

    /// A disabled audit writes nothing and says so, rather than looking like a
    /// context that recorded and found nothing.
    #[test]
    fn a_disabled_audit_records_nothing_and_names_its_reason() {
        let audit = SubsystemAudit::disabled("unit test with no ledger");
        assert!(!audit.is_recording());
        audit.record(action(SubsystemOutcome::Succeeded));

        let AuditTarget::Disabled(reason) = &audit.target else {
            panic!("expected a disabled audit");
        };
        assert_eq!(*reason, "unit test with no ledger");
    }

    /// Detail is JSON-encoded into the row and therefore covered by the chain.
    #[test]
    fn detail_is_recorded_and_covered_by_the_chain() {
        let directory = TempDir::new("detail");
        let audit = SubsystemAudit::in_data_dir(&directory.0);
        audit.record(SubsystemAction {
            detail: Some(serde_json::json!({ "status": 200 })),
            ..action(SubsystemOutcome::Succeeded)
        });

        let path = directory.0.join(crate::run_commands::DATABASE_FILE);
        let ledger = RunLedger::open(&path).unwrap();
        assert!(matches!(
            ledger.verify_subsystem_chain().unwrap(),
            crate::run_ledger::ChainVerification::Intact { events_seen: 1, .. }
        ));
        drop(ledger);

        // Editing the detail breaks the chain, which is the point of hashing it
        // rather than storing it beside the hash. Done over a raw connection —
        // the append-only triggers have to be dropped first, which is precisely
        // the writer the chain exists to catch.
        {
            let connection = rusqlite::Connection::open(&path).unwrap();
            connection
                .execute_batch(
                    "DROP TRIGGER subsystem_events_forbid_update;
                     UPDATE subsystem_events SET detail_json = CAST('{\"status\":500}' AS BLOB);",
                )
                .unwrap();
        }
        let ledger = RunLedger::open(&path).unwrap();
        assert!(matches!(
            ledger.verify_subsystem_chain().unwrap(),
            crate::run_ledger::ChainVerification::Broken { .. }
        ));
    }
}
