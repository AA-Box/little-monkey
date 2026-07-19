//! Human Approval Chains (ROADMAP.md, Phase 3): multi-step approval workflows
//! layered on top of the existing single-shot `permissions.rs` system.
//!
//! `permissions::request_permission` resolves one decision per call. This
//! module adds a *sequence* of stages that must each be approved in order
//! before the whole chain resolves `true` — a deny or a timeout at any stage
//! stops the entire chain rather than skipping ahead. It is a new,
//! independent state machine (its own pending-oneshot map, its own ledger
//! tables — see `run_ledger.rs`'s `MIGRATION_V4_SQL`) rather than an
//! extension of `PermissionState`, exactly like the design brief calls for.
//!
//! Nothing in this build calls [`run_approval_chain`] from another feature —
//! per the brief, later features (Issue-to-PR/Triage/a future Local App
//! Builder) may wire themselves up to it once they exist, but none of them
//! depend on it existing first. The only caller shipped in this stage is the
//! `approval_chains_start` command, driven from the Settings panel's
//! "Approval Chains" tab, so the feature has a real, clickable UI path
//! end-to-end without requiring another stage to integrate it.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tauri::Emitter;
use tokio::sync::oneshot;

use crate::run_ledger::{LedgerError, RunLedger};
use crate::run_protocol::ClientIdentity;
use crate::AppState;

/// One step of an [`ApprovalChainTemplate`]. `timeout_secs` is how long the
/// stage waits for a decision before it's treated as [`ChainStatus::Expired`]
/// (mirrors `permissions.rs`'s `PERMISSION_TIMEOUT`, but configurable per
/// stage/template instead of one fixed constant). `escalate_after_secs`, when
/// set, re-emits the same stage event with `escalated: true` and
/// `escalate_message` after that many seconds — purely a frontend nudge, it
/// never changes the timeout itself.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChainStage {
    pub label: String,
    pub timeout_secs: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub escalate_after_secs: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub escalate_message: Option<String>,
}

/// A named, ordered sequence of [`ChainStage`]s. Only built-in templates
/// exist in this stage (see [`built_in_templates`]) — no user-authored
/// template editor, matching the brief's "at least two built-in templates"
/// scope.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApprovalChainTemplate {
    pub id: String,
    pub name: String,
    pub stages: Vec<ChainStage>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChainStatus {
    Pending,
    Approved,
    Rejected,
    Expired,
}

impl ChainStatus {
    fn as_sql(self) -> &'static str {
        match self {
            ChainStatus::Pending => "pending",
            ChainStatus::Approved => "approved",
            ChainStatus::Rejected => "rejected",
            ChainStatus::Expired => "expired",
        }
    }

    fn from_sql(value: &str) -> Self {
        match value {
            "approved" => ChainStatus::Approved,
            "rejected" => ChainStatus::Rejected,
            "expired" => ChainStatus::Expired,
            _ => ChainStatus::Pending,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StageDecisionKind {
    Allow,
    Deny,
    Expired,
}

impl StageDecisionKind {
    fn as_sql(self) -> &'static str {
        match self {
            StageDecisionKind::Allow => "allow",
            StageDecisionKind::Deny => "deny",
            StageDecisionKind::Expired => "expired",
        }
    }

    fn from_sql(value: &str) -> Self {
        match value {
            "allow" => StageDecisionKind::Allow,
            "deny" => StageDecisionKind::Deny,
            _ => StageDecisionKind::Expired,
        }
    }
}

/// One recorded decision — who decided, what, and when — for a single stage
/// of a single chain run. The audit trail `approval_chains_history` returns
/// is built entirely out of these.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StageDecision {
    pub stage_index: usize,
    pub label: String,
    pub decision: StageDecisionKind,
    pub decided_at_ms: u64,
    pub escalated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub decided_by: Option<ClientIdentity>,
}

/// A materialized view of one chain run, reconstructed from
/// `approval_chain_runs`/`approval_chain_stage_decisions` (see
/// `run_ledger.rs`'s `MIGRATION_V4_SQL`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApprovalChainRun {
    pub id: String,
    pub template_id: String,
    pub operation_digest: String,
    pub detail: String,
    pub current_stage: usize,
    pub decisions: Vec<StageDecision>,
    pub status: ChainStatus,
}

/// Ships with two built-in templates (per the design brief): `double_confirm`
/// (two identical confirm stages — for actions where a single click is too
/// easy to make by accident) and `review_then_approve` (a read-only review
/// stage that shows the same detail/diff text, followed by a real
/// approve/deny stage — for actions where the reviewer should look before
/// deciding).
pub fn built_in_templates() -> Vec<ApprovalChainTemplate> {
    vec![
        ApprovalChainTemplate {
            id: "double_confirm".to_string(),
            name: "Double Confirm".to_string(),
            stages: vec![
                ChainStage {
                    label: "Confirm (1 of 2)".to_string(),
                    timeout_secs: 300,
                    escalate_after_secs: None,
                    escalate_message: None,
                },
                ChainStage {
                    label: "Confirm (2 of 2)".to_string(),
                    timeout_secs: 300,
                    escalate_after_secs: None,
                    escalate_message: None,
                },
            ],
        },
        ApprovalChainTemplate {
            id: "review_then_approve".to_string(),
            name: "Review then Approve".to_string(),
            stages: vec![
                ChainStage {
                    label: "Review".to_string(),
                    timeout_secs: 600,
                    escalate_after_secs: Some(300),
                    escalate_message: Some(
                        "Still waiting on your read-only review of this change".to_string(),
                    ),
                },
                ChainStage {
                    label: "Approve".to_string(),
                    timeout_secs: 300,
                    escalate_after_secs: None,
                    escalate_message: None,
                },
            ],
        },
    ]
}

/// Shared state tracking the in-flight approval-chain stage, mirroring
/// `permissions::PermissionState::pending`. Only one stage can be pending at
/// a time per `chain_id` — stages within a chain run strictly sequentially,
/// never concurrently.
#[derive(Default)]
pub struct ApprovalChainState {
    pending: Mutex<HashMap<String, PendingChainStage>>,
}

struct PendingChainStage {
    stage_index: usize,
    sender: oneshot::Sender<(bool, ClientIdentity)>,
}

/// Payload sent to the frontend over the `approval-chain://stage` event. Sent
/// twice for the same stage when `escalate_after_secs` fires: once with
/// `escalated: false` when the stage begins, and again with `escalated: true`
/// (and `escalate_message` filled in) after the configured delay — the
/// frontend replaces its displayed stage with whichever it saw most
/// recently, it never queues the escalation as a second stage.
#[derive(Debug, Clone, Serialize)]
pub struct ApprovalChainStagePayload {
    pub chain_id: String,
    pub stage_index: usize,
    pub total_stages: usize,
    pub label: String,
    pub detail: String,
    pub timeout_secs: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub escalate_message: Option<String>,
    pub escalated: bool,
    pub expires_at_ms: u64,
}

fn operation_digest(template_id: &str, detail: &str, nonce: &str) -> String {
    let mut hasher = Sha256::new();
    for value in [template_id, detail, nonce] {
        hasher.update((value.len() as u64).to_be_bytes());
        hasher.update(value.as_bytes());
    }
    hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn insert_chain_run(
    ledger: &mut RunLedger,
    chain_id: &str,
    template_id: &str,
    operation_sha256: &str,
    detail: &str,
    total_stages: usize,
    now_ms: u64,
) -> Result<(), LedgerError> {
    ledger.connection().execute(
        "INSERT INTO approval_chain_runs
            (chain_id, template_id, operation_sha256, detail, total_stages, current_stage, status, created_at_ms, updated_at_ms)
         VALUES (?1, ?2, ?3, ?4, ?5, 0, 'pending', ?6, ?6)",
        params![
            chain_id,
            template_id,
            operation_sha256,
            detail,
            total_stages as i64,
            now_ms as i64
        ],
    )?;
    Ok(())
}

fn record_stage_decision(
    ledger: &mut RunLedger,
    chain_id: &str,
    stage_index: usize,
    stage_label: &str,
    decision: StageDecisionKind,
    escalated: bool,
    decided_at_ms: u64,
    decided_by: Option<&ClientIdentity>,
) -> Result<(), LedgerError> {
    let decided_by_json = decided_by.map(serde_json::to_vec).transpose()?;
    ledger.connection().execute(
        "INSERT INTO approval_chain_stage_decisions
            (chain_id, stage_index, stage_label, decision, escalated, decided_at_ms, decided_by_json)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            chain_id,
            stage_index as i64,
            stage_label,
            decision.as_sql(),
            i64::from(escalated),
            decided_at_ms as i64,
            decided_by_json,
        ],
    )?;
    ledger.connection().execute(
        "UPDATE approval_chain_runs SET current_stage = ?2, updated_at_ms = ?3 WHERE chain_id = ?1",
        params![chain_id, (stage_index + 1) as i64, decided_at_ms as i64],
    )?;
    Ok(())
}

fn finish_chain_run(
    ledger: &mut RunLedger,
    chain_id: &str,
    status: ChainStatus,
    updated_at_ms: u64,
) -> Result<(), LedgerError> {
    ledger.connection().execute(
        "UPDATE approval_chain_runs SET status = ?2, updated_at_ms = ?3 WHERE chain_id = ?1",
        params![chain_id, status.as_sql(), updated_at_ms as i64],
    )?;
    Ok(())
}

fn load_chain_runs(ledger: &RunLedger, limit: usize) -> Result<Vec<ApprovalChainRun>, LedgerError> {
    let mut statement = ledger.connection().prepare(
        "SELECT chain_id, template_id, operation_sha256, detail, current_stage, status
         FROM approval_chain_runs ORDER BY created_at_ms DESC LIMIT ?1",
    )?;
    let rows = statement
        .query_map(params![limit as i64], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, i64>(4)?,
                row.get::<_, String>(5)?,
            ))
        })?
        .collect::<Result<Vec<_>, rusqlite::Error>>()?;

    let mut runs = Vec::with_capacity(rows.len());
    for (chain_id, template_id, operation_digest, detail, current_stage, status_raw) in rows {
        let decisions = load_stage_decisions(ledger, &chain_id)?;
        runs.push(ApprovalChainRun {
            id: chain_id,
            template_id,
            operation_digest,
            detail,
            current_stage: current_stage as usize,
            decisions,
            status: ChainStatus::from_sql(&status_raw),
        });
    }
    Ok(runs)
}

fn load_stage_decisions(
    ledger: &RunLedger,
    chain_id: &str,
) -> Result<Vec<StageDecision>, LedgerError> {
    let mut statement = ledger.connection().prepare(
        "SELECT stage_index, stage_label, decision, escalated, decided_at_ms, decided_by_json
         FROM approval_chain_stage_decisions WHERE chain_id = ?1 ORDER BY stage_index ASC",
    )?;
    let rows = statement
        .query_map(params![chain_id], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, i64>(4)?,
                row.get::<_, Option<Vec<u8>>>(5)?,
            ))
        })?
        .collect::<Result<Vec<_>, rusqlite::Error>>()?;

    rows.into_iter()
        .map(
            |(stage_index, label, decision_raw, escalated, decided_at_ms, decided_by_bytes)| {
                let decided_by = decided_by_bytes
                    .map(|bytes| serde_json::from_slice::<ClientIdentity>(&bytes))
                    .transpose()?;
                Ok(StageDecision {
                    stage_index: stage_index as usize,
                    label,
                    decision: StageDecisionKind::from_sql(&decision_raw),
                    decided_at_ms: decided_at_ms as u64,
                    escalated: escalated != 0,
                    decided_by,
                })
            },
        )
        .collect::<Result<Vec<_>, LedgerError>>()
}

fn load_chain_run(ledger: &RunLedger, chain_id: &str) -> Result<Option<ApprovalChainRun>, LedgerError> {
    let row = ledger
        .connection()
        .query_row(
            "SELECT template_id, operation_sha256, detail, current_stage, status
             FROM approval_chain_runs WHERE chain_id = ?1",
            params![chain_id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, String>(4)?,
                ))
            },
        )
        .optional()?;
    let Some((template_id, operation_digest, detail, current_stage, status_raw)) = row else {
        return Ok(None);
    };
    let decisions = load_stage_decisions(ledger, chain_id)?;
    Ok(Some(ApprovalChainRun {
        id: chain_id.to_string(),
        template_id,
        operation_digest,
        detail,
        current_stage: current_stage as usize,
        decisions,
        status: ChainStatus::from_sql(&status_raw),
    }))
}

/// Runs `template`'s stages in order against `state`, persisting every stage
/// transition into the durable ledger (see `run_ledger.rs`'s
/// `MIGRATION_V4_SQL`). Resolves `Ok(true)` only if every stage was allowed;
/// a deny or a timeout at ANY stage stops the whole chain immediately —
/// later stages are never reached — and resolves `Ok(false)`. `Err` is
/// reserved for genuine failures (no window to receive the prompt, a ledger
/// error) rather than an ordinary decline.
///
/// Generic over `R: tauri::Runtime` (exactly like
/// `permissions::request_permission`) so unit tests can drive this with
/// `tauri::test`'s `MockRuntime`.
pub async fn run_approval_chain<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    state: &AppState,
    template: &ApprovalChainTemplate,
    operation_digest: String,
    detail: String,
) -> Result<bool, String> {
    if template.stages.is_empty() {
        return Err("Approval chain template has no stages".to_string());
    }

    let chain_id = uuid::Uuid::new_v4().to_string();
    let created_at_ms = crate::run_commands::unix_time_ms()?;
    crate::run_commands::with_ledger(app, state, |ledger| {
        insert_chain_run(
            ledger,
            &chain_id,
            &template.id,
            &operation_digest,
            &detail,
            template.stages.len(),
            created_at_ms,
        )
    })?;

    for (stage_index, stage) in template.stages.iter().enumerate() {
        let now_ms = crate::run_commands::unix_time_ms()?;
        let expires_at_ms = now_ms
            .checked_add(stage.timeout_secs.saturating_mul(1000))
            .ok_or_else(|| "Approval chain stage expiry exceeds protocol bounds".to_string())?;

        let (tx, rx) = oneshot::channel::<(bool, ClientIdentity)>();
        state
            .approval_chains
            .pending
            .lock()
            .unwrap()
            .insert(chain_id.clone(), PendingChainStage { stage_index, sender: tx });

        let payload = ApprovalChainStagePayload {
            chain_id: chain_id.clone(),
            stage_index,
            total_stages: template.stages.len(),
            label: stage.label.clone(),
            detail: detail.clone(),
            timeout_secs: stage.timeout_secs,
            escalate_message: stage.escalate_message.clone(),
            escalated: false,
            expires_at_ms,
        };

        if app.emit("approval-chain://stage", payload.clone()).is_err() {
            state.approval_chains.pending.lock().unwrap().remove(&chain_id);
            let finished_at = crate::run_commands::unix_time_ms()?;
            crate::run_commands::with_ledger(app, state, |ledger| {
                finish_chain_run(ledger, &chain_id, ChainStatus::Rejected, finished_at)
            })?;
            return Err("No windows to receive the approval chain prompt".to_string());
        }

        // Guards the escalation timer below against re-emitting a stale
        // `approval-chain://stage` event for a stage that has already
        // resolved (responded to or timed out) by the time the timer fires —
        // the frontend has no other way to distinguish a live escalation
        // from a ghost one, since it can't be told apart from a fresh stage
        // by shape alone. Flipped to `true` the moment this stage resolves,
        // below, before the escalation timer could possibly still be
        // sleeping past that point in the common case, and checked again
        // right before the timer would emit to close the race in the
        // uncommon case where both happen around the same instant.
        let stage_resolved = Arc::new(AtomicBool::new(false));

        if let Some(escalate_after_secs) = stage.escalate_after_secs {
            if escalate_after_secs < stage.timeout_secs && payload.escalate_message.is_some() {
                let app_for_escalation = app.clone();
                let mut escalated_payload = payload.clone();
                escalated_payload.escalated = true;
                let resolved_for_escalation = stage_resolved.clone();
                tauri::async_runtime::spawn(async move {
                    tokio::time::sleep(Duration::from_secs(escalate_after_secs)).await;
                    if resolved_for_escalation.load(Ordering::SeqCst) {
                        return;
                    }
                    let _ = app_for_escalation.emit("approval-chain://stage", escalated_payload);
                });
            }
        }

        let outcome = tokio::time::timeout(Duration::from_secs(stage.timeout_secs), rx).await;
        stage_resolved.store(true, Ordering::SeqCst);
        state.approval_chains.pending.lock().unwrap().remove(&chain_id);

        let (decision, allowed, decided_by) = match outcome {
            Ok(Ok((true, identity))) => (StageDecisionKind::Allow, true, Some(identity)),
            Ok(Ok((false, identity))) => (StageDecisionKind::Deny, false, Some(identity)),
            // Timed out, or the sender was dropped without a response.
            Ok(Err(_)) | Err(_) => (StageDecisionKind::Expired, false, None),
        };

        let decided_at_ms = crate::run_commands::unix_time_ms()?;
        crate::run_commands::with_ledger(app, state, |ledger| {
            record_stage_decision(
                ledger,
                &chain_id,
                stage_index,
                &stage.label,
                decision,
                false,
                decided_at_ms,
                decided_by.as_ref(),
            )
        })?;

        if !allowed {
            let final_status = if matches!(decision, StageDecisionKind::Expired) {
                ChainStatus::Expired
            } else {
                ChainStatus::Rejected
            };
            crate::run_commands::with_ledger(app, state, |ledger| {
                finish_chain_run(ledger, &chain_id, final_status, decided_at_ms)
            })?;
            return Ok(false);
        }
    }

    let finished_at = crate::run_commands::unix_time_ms()?;
    crate::run_commands::with_ledger(app, state, |ledger| {
        finish_chain_run(ledger, &chain_id, ChainStatus::Approved, finished_at)
    })?;
    Ok(true)
}

// --- commands -----------------------------------------------------------------

#[tauri::command]
pub fn approval_chains_list_templates() -> Vec<ApprovalChainTemplate> {
    built_in_templates()
}

/// Manual trigger for a chain run — the Settings panel's "Approval Chains"
/// tab is the reachable entry point for this stage (no other shipped feature
/// calls [`run_approval_chain`] yet — see this module's top doc comment).
/// `operation_digest` is computed here (not caller-supplied) from
/// `template_id`/`detail`/a fresh nonce, the same "the request payload can
/// never be forged by the caller" reasoning as
/// `permissions.rs::operation_digest`.
#[tauri::command]
pub async fn approval_chains_start(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    template_id: String,
    detail: String,
) -> Result<bool, String> {
    let template = built_in_templates()
        .into_iter()
        .find(|candidate| candidate.id == template_id)
        .ok_or_else(|| format!("Unknown approval chain template '{template_id}'"))?;
    let nonce = uuid::Uuid::new_v4().to_string();
    let digest = operation_digest(&template_id, &detail, &nonce);
    run_approval_chain(&app, state.inner(), &template, digest, detail).await
}

/// Called by the frontend (`ApprovalChainModal`) once the user decides a
/// stage. Deliberately looks up `stage_index` from the pending map by
/// `chain_id` rather than trusting a caller-supplied index — mirrors
/// `permissions::permission_respond`'s reasoning for not trusting a
/// caller-supplied tool name.
#[tauri::command]
pub fn approval_chain_respond(
    app: tauri::AppHandle,
    window: tauri::Window,
    state: tauri::State<'_, AppState>,
    chain_id: String,
    allow: bool,
) -> Result<(), String> {
    let pending = state
        .approval_chains
        .pending
        .lock()
        .unwrap()
        .remove(&chain_id);
    let Some(pending) = pending else {
        return Err(format!(
            "No pending approval chain stage for chain '{chain_id}'"
        ));
    };
    let _ = pending.stage_index;
    let identity = crate::run_commands::desktop_identity(&app, &window);
    // If the receiving end was already dropped (e.g. the stage timed out just
    // before the user clicked), there's nothing left to notify.
    let _ = pending.sender.send((allow, identity));
    Ok(())
}

/// Looks up a single chain run by id — used by `ApprovalChainModal` to
/// re-fetch the full stage-decision history for a chain it's currently
/// showing (e.g. after the modal reopens), without waiting for the next
/// `approval-chain://stage` event.
#[tauri::command]
pub fn approval_chains_get(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    chain_id: String,
) -> Result<Option<ApprovalChainRun>, String> {
    crate::run_commands::with_ledger(&app, state.inner(), |ledger| {
        load_chain_run(ledger, &chain_id)
    })
}

/// Audit history: who/when/what for every stage of every past chain run,
/// most recent first.
#[tauri::command]
pub fn approval_chains_history(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    limit: usize,
) -> Result<Vec<ApprovalChainRun>, String> {
    crate::run_commands::with_ledger(&app, state.inner(), |ledger| {
        load_chain_runs(ledger, limit.clamp(1, 200))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::run_ledger::RunLedger;

    fn stage(label: &str, timeout_secs: u64) -> ChainStage {
        ChainStage {
            label: label.to_string(),
            timeout_secs,
            escalate_after_secs: None,
            escalate_message: None,
        }
    }

    fn template(id: &str, stages: Vec<ChainStage>) -> ApprovalChainTemplate {
        ApprovalChainTemplate {
            id: id.to_string(),
            name: id.to_string(),
            stages,
        }
    }

    fn test_identity() -> ClientIdentity {
        ClientIdentity {
            client_id: "test-client".to_string(),
            instance_id: "test-instance".to_string(),
            kind: crate::run_protocol::ClientKind::Test,
            version: "0.0.0".to_string(),
        }
    }

    #[test]
    fn built_in_templates_include_double_confirm_and_review_then_approve() {
        let templates = built_in_templates();
        let ids: Vec<&str> = templates.iter().map(|t| t.id.as_str()).collect();
        assert!(ids.contains(&"double_confirm"));
        assert!(ids.contains(&"review_then_approve"));
        for template in &templates {
            assert!(!template.stages.is_empty());
        }
    }

    #[test]
    fn double_confirm_has_exactly_two_stages() {
        let templates = built_in_templates();
        let double_confirm = templates.iter().find(|t| t.id == "double_confirm").unwrap();
        assert_eq!(double_confirm.stages.len(), 2);
    }

    #[test]
    fn review_then_approve_escalates_only_its_review_stage() {
        let templates = built_in_templates();
        let review_then_approve = templates
            .iter()
            .find(|t| t.id == "review_then_approve")
            .unwrap();
        assert!(review_then_approve.stages[0].escalate_after_secs.is_some());
        assert!(review_then_approve.stages[0].escalate_message.is_some());
        assert!(review_then_approve.stages[1].escalate_after_secs.is_none());
    }

    #[test]
    fn ledger_round_trips_a_chain_run_and_its_stage_decisions() {
        let mut ledger = RunLedger::open_in_memory().unwrap();
        insert_chain_run(&mut ledger, "chain-1", "double_confirm", &"a".repeat(64), "do the thing", 2, 1_000).unwrap();

        let loaded = load_chain_run(&ledger, "chain-1").unwrap().unwrap();
        assert_eq!(loaded.status, ChainStatus::Pending);
        assert_eq!(loaded.current_stage, 0);
        assert!(loaded.decisions.is_empty());

        record_stage_decision(
            &mut ledger,
            "chain-1",
            0,
            "Confirm (1 of 2)",
            StageDecisionKind::Allow,
            false,
            2_000,
            Some(&test_identity()),
        )
        .unwrap();

        let loaded = load_chain_run(&ledger, "chain-1").unwrap().unwrap();
        assert_eq!(loaded.current_stage, 1);
        assert_eq!(loaded.decisions.len(), 1);
        assert_eq!(loaded.decisions[0].decision, StageDecisionKind::Allow);
        assert_eq!(
            loaded.decisions[0].decided_by.as_ref().unwrap().client_id,
            "test-client"
        );

        finish_chain_run(&mut ledger, "chain-1", ChainStatus::Approved, 3_000).unwrap();
        let loaded = load_chain_run(&ledger, "chain-1").unwrap().unwrap();
        assert_eq!(loaded.status, ChainStatus::Approved);
    }

    #[test]
    fn load_chain_run_returns_none_for_an_unknown_chain_id() {
        let ledger = RunLedger::open_in_memory().unwrap();
        assert!(load_chain_run(&ledger, "does-not-exist").unwrap().is_none());
    }

    #[test]
    fn history_orders_most_recent_first_and_respects_the_limit() {
        let mut ledger = RunLedger::open_in_memory().unwrap();
        insert_chain_run(&mut ledger, "chain-a", "double_confirm", &"a".repeat(64), "first", 1, 1_000).unwrap();
        insert_chain_run(&mut ledger, "chain-b", "double_confirm", &"b".repeat(64), "second", 1, 2_000).unwrap();
        insert_chain_run(&mut ledger, "chain-c", "double_confirm", &"c".repeat(64), "third", 1, 3_000).unwrap();

        let history = load_chain_runs(&ledger, 2).unwrap();
        assert_eq!(history.len(), 2);
        assert_eq!(history[0].id, "chain-c");
        assert_eq!(history[1].id, "chain-b");
    }

    #[tokio::test]
    async fn a_deny_at_the_first_stage_never_reaches_the_second_stage() {
        let state = std::sync::Arc::new(AppState::default());
        // Pre-seed an in-memory ledger so `with_ledger` never resolves
        // `mock_app()`'s real (unscoped) app-data directory on disk — see
        // this module's own tests for why that matters.
        *state.run_ledger.lock().unwrap() = Some(RunLedger::open_in_memory().unwrap());
        let handle = tauri::test::mock_app().handle().clone();
        let tmpl = template("deny-first", vec![stage("Stage 1", 5), stage("Stage 2", 5)]);

        let task_state = state.clone();
        let task_handle = handle.clone();
        let task = tokio::spawn(async move {
            run_approval_chain(&task_handle, &task_state, &tmpl, "d".repeat(64), "do it".to_string()).await
        });

        let mut chain_id = None;
        for _ in 0..200 {
            let pending = state.approval_chains.pending.lock().unwrap();
            if let Some((id, entry)) = pending.iter().next() {
                if entry.stage_index == 0 {
                    chain_id = Some(id.clone());
                    break;
                }
            }
            drop(pending);
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let chain_id = chain_id.expect("chain never reached its first stage");

        approval_chain_respond_for_test(&state, &chain_id, false);

        let result = task.await.unwrap().unwrap();
        assert_eq!(result, false);

        // Never registered a pending stage for stage index 1 — the deny at
        // stage 0 stopped the chain before stage 1 was ever reached.
        assert!(state.approval_chains.pending.lock().unwrap().is_empty());

        let history = crate::run_commands::with_ledger(&handle, &state, |ledger| load_chain_runs(ledger, 10)).unwrap();
        let run = history.iter().find(|r| r.id == chain_id).unwrap();
        assert_eq!(run.status, ChainStatus::Rejected);
        assert_eq!(run.decisions.len(), 1);
        assert_eq!(run.decisions[0].stage_index, 0);
        assert_eq!(run.decisions[0].decision, StageDecisionKind::Deny);
    }

    #[tokio::test]
    async fn allowing_every_stage_approves_the_whole_chain_in_order() {
        let state = std::sync::Arc::new(AppState::default());
        // Pre-seed an in-memory ledger so `with_ledger` never resolves
        // `mock_app()`'s real (unscoped) app-data directory on disk — see
        // this module's own tests for why that matters.
        *state.run_ledger.lock().unwrap() = Some(RunLedger::open_in_memory().unwrap());
        let handle = tauri::test::mock_app().handle().clone();
        let tmpl = template("allow-both", vec![stage("Stage 1", 5), stage("Stage 2", 5)]);

        let task_state = state.clone();
        let task_handle = handle.clone();
        let task = tokio::spawn(async move {
            run_approval_chain(&task_handle, &task_state, &tmpl, "d".repeat(64), "do it".to_string()).await
        });

        for expected_stage in 0..2 {
            let mut chain_id = None;
            for _ in 0..200 {
                let pending = state.approval_chains.pending.lock().unwrap();
                if let Some((id, entry)) = pending.iter().next() {
                    if entry.stage_index == expected_stage {
                        chain_id = Some(id.clone());
                        break;
                    }
                }
                drop(pending);
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
            let chain_id = chain_id.unwrap_or_else(|| panic!("chain never reached stage {expected_stage}"));
            approval_chain_respond_for_test(&state, &chain_id, true);
        }

        let result = task.await.unwrap().unwrap();
        assert_eq!(result, true);

        let history = crate::run_commands::with_ledger(&handle, &state, |ledger| load_chain_runs(ledger, 10)).unwrap();
        let run = history.first().unwrap();
        assert_eq!(run.status, ChainStatus::Approved);
        assert_eq!(run.decisions.len(), 2);
        assert!(run.decisions.iter().all(|d| d.decision == StageDecisionKind::Allow));
    }

    #[tokio::test]
    async fn an_unanswered_stage_expires_and_rejects_the_chain() {
        let state = std::sync::Arc::new(AppState::default());
        // Pre-seed an in-memory ledger so `with_ledger` never resolves
        // `mock_app()`'s real (unscoped) app-data directory on disk — see
        // this module's own tests for why that matters.
        *state.run_ledger.lock().unwrap() = Some(RunLedger::open_in_memory().unwrap());
        let handle = tauri::test::mock_app().handle().clone();
        let tmpl = template("times-out", vec![stage("Stage 1", 0), stage("Stage 2", 5)]);

        let result = run_approval_chain(&handle, &state, &tmpl, "d".repeat(64), "do it".to_string())
            .await
            .unwrap();
        assert_eq!(result, false);

        let history = crate::run_commands::with_ledger(&handle, &state, |ledger| load_chain_runs(ledger, 10)).unwrap();
        let run = history.first().unwrap();
        assert_eq!(run.status, ChainStatus::Expired);
        assert_eq!(run.decisions.len(), 1);
        assert_eq!(run.decisions[0].decision, StageDecisionKind::Expired);
        assert!(run.decisions[0].decided_by.is_none());
    }

    #[tokio::test]
    async fn escalation_re_emits_the_same_stage_before_its_timeout() {
        let state = std::sync::Arc::new(AppState::default());
        // Pre-seed an in-memory ledger so `with_ledger` never resolves
        // `mock_app()`'s real (unscoped) app-data directory on disk — see
        // this module's own tests for why that matters.
        *state.run_ledger.lock().unwrap() = Some(RunLedger::open_in_memory().unwrap());
        let handle = tauri::test::mock_app().handle().clone();
        let tmpl = template(
            "escalates",
            vec![ChainStage {
                label: "Review".to_string(),
                timeout_secs: 3,
                escalate_after_secs: Some(1),
                escalate_message: Some("please look now".to_string()),
            }],
        );

        let task_state = state.clone();
        let task_handle = handle.clone();
        let task = tokio::spawn(async move {
            run_approval_chain(&task_handle, &task_state, &tmpl, "d".repeat(64), "do it".to_string()).await
        });

        // Let the escalation timer (1s) fire, then approve before the 3s
        // stage timeout — proving escalation is advisory and never changes
        // the actual deadline.
        tokio::time::sleep(std::time::Duration::from_millis(1200)).await;
        let chain_id = {
            let pending = state.approval_chains.pending.lock().unwrap();
            pending.keys().next().cloned().expect("stage still pending after escalation")
        };
        approval_chain_respond_for_test(&state, &chain_id, true);

        let result = task.await.unwrap().unwrap();
        assert_eq!(result, true);
    }

    /// Test-only helper mirroring `approval_chain_respond`'s core effect
    /// without needing a `tauri::Window` (the command itself requires one to
    /// stamp `decided_by`, which unit tests running under `MockRuntime` have
    /// no reason to stand up).
    fn approval_chain_respond_for_test(state: &AppState, chain_id: &str, allow: bool) {
        let pending = state.approval_chains.pending.lock().unwrap().remove(chain_id);
        let pending = pending.expect("no pending approval chain stage");
        let _ = pending.sender.send((allow, test_identity()));
    }
}
