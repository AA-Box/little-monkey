//! Tauri bridge for the skill-learning loop.
//!
//! Every command here is a thin wrapper: the durable decisions (which runs
//! count as evidence, what a candidate may contain, when approval is needed,
//! what a promotion installs) all live in `skill_learning.rs`, so the CLI
//! reaches exactly the same behaviour through the same store rather than a
//! parallel implementation.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::m4_commands::M4CommandState;
use crate::native_skill_commands::{
    optional_primary_workspace, run_blocking, NativeSkillsCommandState,
};
use crate::native_skills::{ExternalSignedSkill, SkillDescriptor, SkillMutationResult, SkillScope};
use crate::run_commands::with_ledger;
use crate::run_protocol::RunEventEnvelope;
use crate::skill_learning::{
    approval_operation_digest, evidence_from_events, reflection_brief, ApprovalGrant,
    CandidateProposal, CorrectedExecution, EffectivenessRecord, EvaluationCaseReport,
    EvaluationMode, EvaluationPlan, EvaluationRecord, LearnedSkillSummary, LearningCandidate,
    LearningMode, LearningSettings, PreTaskFile, PromotionOutcome, RunEvidence, SkillLearningStore,
};
use crate::AppState;

pub struct SkillLearningCommandState {
    pub store: Arc<SkillLearningStore>,
}

impl SkillLearningCommandState {
    pub fn production(app_data_dir: &Path) -> Result<Self, String> {
        Ok(Self {
            store: Arc::new(
                SkillLearningStore::new(app_data_dir).map_err(|error| error.to_string())?,
            ),
        })
    }
}

/// Signed-package skills participate in the same collision namespace as native
/// folders, so deduplication and "does this command already exist" have to see
/// them too — the same list `native_skills_discover` builds.
pub(crate) fn signed_package_skills(
    m4: &M4CommandState,
) -> Result<Vec<ExternalSignedSkill>, String> {
    Ok(m4
        .packages
        .active_skills()
        .map_err(|error| error.to_string())?
        .into_iter()
        .map(|skill| ExternalSignedSkill {
            package_id: skill.package_id,
            name: skill.name,
            description: skill.description,
            command: skill.command,
            version: skill.version.to_string(),
            instructions: skill.instructions,
            sha256: skill.content_sha256,
            permissions: skill
                .permissions
                .into_iter()
                .map(|permission| permission.permission_id)
                .collect(),
        })
        .collect())
}

fn require_main_window(window: &tauri::Window) -> Result<(), String> {
    if window.label() == "main" {
        Ok(())
    } else {
        Err("Skill learning is only available from the main window".to_string())
    }
}

#[tauri::command]
pub async fn skill_learning_mode(
    learning: tauri::State<'_, SkillLearningCommandState>,
) -> Result<LearningMode, String> {
    let store = learning.store.clone();
    run_blocking(move || store.mode()).await
}

#[tauri::command]
pub async fn skill_learning_settings(
    learning: tauri::State<'_, SkillLearningCommandState>,
) -> Result<LearningSettings, String> {
    let store = learning.store.clone();
    run_blocking(move || store.settings()).await
}

#[tauri::command]
pub async fn skill_learning_set_settings(
    window: tauri::Window,
    learning: tauri::State<'_, SkillLearningCommandState>,
    settings: LearningSettings,
) -> Result<LearningSettings, String> {
    require_main_window(&window)?;
    let store = learning.store.clone();
    run_blocking(move || store.set_settings(settings)).await
}

/// The bounded evidence brief the reflection pass reads. Generated here, from
/// the snapshot the backend persisted with the candidate — never assembled in
/// the frontend out of whatever fields happen to be on hand.
#[tauri::command]
pub async fn skill_learning_reflection_brief(
    learning: tauri::State<'_, SkillLearningCommandState>,
    candidate_id: String,
) -> Result<String, String> {
    let store = learning.store.clone();
    run_blocking(move || {
        store
            .candidate(&candidate_id)
            .map(|candidate| reflection_brief(&candidate))
    })
    .await
}

#[tauri::command]
pub async fn skill_learning_set_mode(
    window: tauri::Window,
    learning: tauri::State<'_, SkillLearningCommandState>,
    mode: LearningMode,
) -> Result<LearningMode, String> {
    require_main_window(&window)?;
    let store = learning.store.clone();
    run_blocking(move || store.set_mode(mode)).await
}

/// Classifies a finished run against the deterministic signal rules and opens
/// a `detected` candidate when one fires. The run's events come from the
/// durable ledger here, not from the caller — a frontend cannot manufacture
/// evidence by describing a run it never had.
#[tauri::command]
pub async fn skill_learning_detect(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    learning: tauri::State<'_, SkillLearningCommandState>,
    run_id: String,
    user_text: String,
    scope: SkillScope,
) -> Result<Option<LearningCandidate>, String> {
    let workspace = optional_primary_workspace(&state)?;
    let events: Vec<RunEventEnvelope> = with_ledger(&app, &state, |ledger| {
        ledger.load_events(&run_id, 0, crate::skill_learning::MAX_SOURCE_EVENTS * 8)
    })?;
    let evidence = evidence_from_events(&run_id, &user_text, &events);
    let store = learning.store.clone();
    run_blocking(move || store.detect(&evidence, scope, workspace.as_deref())).await
}

#[tauri::command]
pub async fn skill_learning_list_candidates(
    learning: tauri::State<'_, SkillLearningCommandState>,
) -> Result<Vec<LearningCandidate>, String> {
    let store = learning.store.clone();
    run_blocking(move || store.list_candidates()).await
}

#[tauri::command]
pub async fn skill_learning_candidate(
    learning: tauri::State<'_, SkillLearningCommandState>,
    candidate_id: String,
) -> Result<LearningCandidate, String> {
    let store = learning.store.clone();
    run_blocking(move || store.candidate(&candidate_id)).await
}

#[tauri::command]
pub async fn skill_learning_begin_reflection(
    learning: tauri::State<'_, SkillLearningCommandState>,
    candidate_id: String,
) -> Result<LearningCandidate, String> {
    let store = learning.store.clone();
    run_blocking(move || store.begin_reflection(&candidate_id)).await
}

/// Stages (or re-stages, for "Edit before install") a candidate's package.
#[tauri::command]
pub async fn skill_learning_stage(
    state: tauri::State<'_, AppState>,
    native: tauri::State<'_, NativeSkillsCommandState>,
    m4: tauri::State<'_, M4CommandState>,
    learning: tauri::State<'_, SkillLearningCommandState>,
    candidate_id: String,
    proposal: CandidateProposal,
    run_id: Option<String>,
) -> Result<LearningCandidate, String> {
    let workspace = optional_primary_workspace(&state)?;
    let packages = signed_package_skills(&m4)?;
    let manager = native.manager.clone();
    let store = learning.store.clone();
    run_blocking(move || {
        store.propose(
            &candidate_id,
            run_id.as_deref(),
            &proposal,
            &manager,
            workspace.as_deref(),
            &packages,
        )
    })
    .await
}

#[tauri::command]
pub async fn skill_learning_plan_evaluation(
    learning: tauri::State<'_, SkillLearningCommandState>,
    candidate_id: String,
) -> Result<EvaluationPlan, String> {
    let store = learning.store.clone();
    run_blocking(move || store.plan_evaluation(&candidate_id)).await
}

#[tauri::command]
pub async fn skill_learning_report_evaluation(
    learning: tauri::State<'_, SkillLearningCommandState>,
    evaluation_id: String,
    mode: EvaluationMode,
    reports: Vec<EvaluationCaseReport>,
) -> Result<EvaluationRecord, String> {
    let store = learning.store.clone();
    run_blocking(move || store.report_evaluation(&evaluation_id, mode, &reports)).await
}

#[tauri::command]
pub async fn skill_learning_mark_unevaluated(
    learning: tauri::State<'_, SkillLearningCommandState>,
    evaluation_id: String,
    reason: String,
) -> Result<EvaluationRecord, String> {
    let store = learning.store.clone();
    run_blocking(move || store.mark_unevaluated(&evaluation_id, &reason)).await
}

/// One disposable workspace per evaluation arm, all copied from the same
/// starting state before any arm runs.
///
/// The source is resolved in the backend from the candidate's own recorded
/// workspace — a runtime asks for sandboxes, it does not get to say what they
/// are copies of. A workspace that is too large to copy within the store's
/// bounds returns an error, which the caller records as `unevaluated`: an
/// evaluation that cannot be reproduced is never a pass, and never runs
/// against the user's live files.
///
/// Each copy is rewound to the state the workspace was in before the observed
/// procedure ran, read from that turn's own checkpoint. Learning happens after
/// a run succeeds, so without the rewind every arm would start from a
/// workspace that already contains the answer. A run that changed files and
/// whose checkpoint is gone (pruned, or never linked) therefore cannot be
/// evaluated at all — that is an error here, and an `unevaluated` above.
#[tauri::command]
pub async fn skill_learning_create_sandboxes(
    app: tauri::AppHandle,
    learning: tauri::State<'_, SkillLearningCommandState>,
    evaluation_id: String,
    arms: Vec<String>,
) -> Result<Vec<EvaluationSandbox>, String> {
    let store = learning.store.clone();
    let checkpoints_dir = crate::checkpoints::checkpoints_base_dir(&app)?;
    run_blocking(move || {
        let environment = store.evaluation_environment(&evaluation_id)?;
        let invalid = crate::native_skills::SkillError::Invalid;
        let source = environment.workspace.ok_or_else(|| {
            invalid(
                "this candidate has no recorded workspace, so no reproducible evaluation environment can be built"
                    .to_string(),
            )
        })?;
        let pre_task = match &environment.checkpoint_id {
            Some(checkpoint_id) => {
                let state = crate::checkpoints::pre_turn_state(&checkpoints_dir, checkpoint_id)
                    .map_err(invalid)?;
                // A shell command's side effects are snapshotted by no
                // checkpoint, so the rewind is partial and the copy may still
                // hold what the procedure produced. When the observed run ended
                // verified, the evaluator catches that by verifying the rewound
                // sandbox before any arm runs. When it did not, nothing can
                // tell a reproduced task from a leftover one, and a promotion
                // -grade pass cannot honestly come out of it.
                if state.shell_ran && !environment.self_checking {
                    return Err(invalid(
                        "the observed run used the shell, whose effects no checkpoint captures, and ended with no verification to check the rebuilt starting state against — so a reproducible evaluation environment cannot be confirmed"
                            .to_string(),
                    ));
                }
                state
                    .files
                    .into_iter()
                    .map(|file| PreTaskFile { path: file.path, contents: file.contents })
                    .collect::<Vec<_>>()
            }
            None if environment.requires_pre_task_state => {
                return Err(invalid(
                    "the observed run changed files but its checkpoint is no longer available, so the task it solved cannot be put back for evaluation"
                        .to_string(),
                ))
            }
            None => Vec::new(),
        };
        let created = store.create_eval_sandboxes(&evaluation_id, &source, &arms, &pre_task)?;
        Ok(created
            .into_iter()
            .map(|(arm, path)| EvaluationSandbox {
                arm,
                path: path.to_string_lossy().to_string(),
            })
            .collect::<Vec<_>>())
    })
    .await
}

#[derive(serde::Serialize)]
pub struct EvaluationSandbox {
    pub arm: String,
    pub path: String,
}

#[tauri::command]
pub async fn skill_learning_destroy_sandboxes(
    learning: tauri::State<'_, SkillLearningCommandState>,
    evaluation_id: String,
) -> Result<(), String> {
    let store = learning.store.clone();
    run_blocking(move || store.destroy_eval_sandboxes(&evaluation_id)).await
}

#[tauri::command]
pub async fn skill_learning_evaluations(
    learning: tauri::State<'_, SkillLearningCommandState>,
    candidate_id: String,
) -> Result<Vec<EvaluationRecord>, String> {
    let store = learning.store.clone();
    run_blocking(move || store.evaluations_for(&candidate_id)).await
}

/// The approve-and-install action.
///
/// The approval itself is the app's own permission system — the same prompt,
/// the same durable `permission_decisions` row, the same request id every
/// other gated operation produces. This command never takes an "approved"
/// boolean from the UI: it describes exactly what would be installed, asks,
/// and only on an allow decision hands the store an [`ApprovalGrant`] bound to
/// the digest of the candidate the user was shown. A candidate that is edited
/// or re-staged in between recomputes to a different digest, and the grant
/// stops authorizing it.
///
/// `unattended` marks the auto-promote path instead, which is strictly
/// *narrower* than an approval, never wider: it additionally requires the
/// user's configured mode to allow unattended promotion, the policy to have
/// found nothing needing approval, and a real isolated evaluation to have
/// passed. It can therefore never install something an explicit approval
/// could not.
#[tauri::command]
pub async fn skill_learning_promote(
    app: tauri::AppHandle,
    window: tauri::Window,
    state: tauri::State<'_, AppState>,
    native: tauri::State<'_, NativeSkillsCommandState>,
    learning: tauri::State<'_, SkillLearningCommandState>,
    candidate_id: String,
    unattended: Option<bool>,
) -> Result<PromotionOutcome, String> {
    require_main_window(&window)?;
    let workspace = optional_primary_workspace(&state)?;
    let manager = native.manager.clone();
    let store = learning.store.clone();
    let unattended = unattended.unwrap_or(false);
    if unattended {
        return run_blocking(move || {
            store.promote(&candidate_id, None, true, &manager, workspace.as_deref())
        })
        .await;
    }

    let candidate = {
        let store = learning.store.clone();
        let candidate_id = candidate_id.clone();
        run_blocking(move || store.candidate(&candidate_id)).await?
    };
    let operation_sha256 = approval_operation_digest(&candidate);
    let approval_id = crate::permissions::request_permission(
        &app,
        state.inner(),
        "install_learned_skill",
        promotion_detail(&candidate, &operation_sha256),
        None,
        None,
        None,
        None,
    )
    .await?;
    let grant = ApprovalGrant {
        approval_id,
        operation_sha256,
    };
    run_blocking(move || {
        store.promote(
            &candidate_id,
            Some(&grant),
            false,
            &manager,
            workspace.as_deref(),
        )
    })
    .await
}

/// Everything the approval covers, in the words the user decides on. The
/// digest is part of the text as well as of the grant, so the record of what
/// was approved and the thing that authorizes the install cannot drift apart.
fn promotion_detail(candidate: &LearningCandidate, operation_sha256: &str) -> String {
    let mut lines = vec![
        format!(
            "Install /{} as a {:?}-scope learned skill",
            candidate.proposed_command, candidate.scope
        ),
        format!("Package digest: {}", candidate.candidate_sha256),
        format!(
            "Tools while active: {}",
            if candidate.allowed_tools.is_empty() {
                "unrestricted (the run's own permissions still apply)".to_string()
            } else {
                candidate.allowed_tools.join(", ")
            }
        ),
    ];
    if !candidate.requirements.bins.is_empty() {
        lines.push(format!(
            "Requires executables: {}",
            candidate
                .requirements
                .bins
                .iter()
                .cloned()
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    if !candidate.requirements.env.is_empty() {
        lines.push(format!(
            "Requires environment: {}",
            candidate
                .requirements
                .env
                .iter()
                .cloned()
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    if let Some(parent) = &candidate.parent_skill_sha256 {
        lines.push(format!(
            "Replaces version {} (kept for rollback)",
            &parent[..parent.len().min(12)]
        ));
    }
    if let Some(policy) = &candidate.policy {
        if !policy.approval_reasons.is_empty() {
            lines.push(format!(
                "Needs approval because {}",
                policy.approval_reasons.join("; ")
            ));
        }
    }
    lines.push(match candidate.evaluation_verdict {
        Some(crate::skill_learning::EvaluationVerdict::Passed) => {
            format!(
                "Evaluation: passed ({})",
                candidate.evaluation_ids.join(", ")
            )
        }
        Some(crate::skill_learning::EvaluationVerdict::Failed) => "Evaluation: FAILED".to_string(),
        _ => "Evaluation: not evaluated".to_string(),
    });
    lines.push(format!("Approval digest: {operation_sha256}"));
    lines.join("\n")
}

#[tauri::command]
pub async fn skill_learning_reject(
    learning: tauri::State<'_, SkillLearningCommandState>,
    candidate_id: String,
    reason: String,
) -> Result<LearningCandidate, String> {
    let store = learning.store.clone();
    run_blocking(move || store.reject(&candidate_id, &reason)).await
}

/// Finalizes effectiveness for one run that has reached a terminal state —
/// completed, failed or cancelled alike.
///
/// Everything is read from the run's own durable events: which learned
/// versions it invoked (its `skill_invoked` events), how it ended, what its
/// last verification said, and which tool calls failed. The caller supplies
/// only the identity of the run and the session it belonged to, so no
/// frontend can attribute an outcome to a version that run never used.
#[tauri::command]
pub async fn skill_learning_finalize_run(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    learning: tauri::State<'_, SkillLearningCommandState>,
    run_id: String,
    session_id: Option<String>,
) -> Result<Vec<LearningCandidate>, String> {
    let Some(evidence) = run_evidence(&app, &state, &run_id, "") else {
        return Ok(Vec::new());
    };
    let store = learning.store.clone();
    run_blocking(move || store.record_run(&evidence, session_id.as_deref())).await
}

/// Attributes a correction to the learned version the session's previous turn
/// actually used. The corrected run's own outcome is read from the durable
/// ledger — a correction only becomes an update candidate when the corrected
/// procedure really executed and verified.
#[tauri::command]
pub async fn skill_learning_record_correction(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    learning: tauri::State<'_, SkillLearningCommandState>,
    session_id: String,
    run_id: String,
    user_text: String,
) -> Result<Option<LearningCandidate>, String> {
    let evidence = run_evidence(&app, &state, &run_id, &user_text);
    let corrected = CorrectedExecution {
        user_text,
        succeeded: evidence.as_ref().is_some_and(|entry| {
            entry.completed && !entry.failed && !entry.successful_tools().is_empty()
        }),
        verification_passed: evidence.as_ref().and_then(|entry| {
            entry
                .verifications
                .iter()
                .max_by_key(|verification| verification.sequence)
                .map(|verification| verification.passed)
        }),
        event_ids: evidence
            .as_ref()
            .map(|entry| {
                entry
                    .tool_calls
                    .iter()
                    .map(|call| call.event_id.clone())
                    .collect()
            })
            .unwrap_or_default(),
        evidence,
    };
    let store = learning.store.clone();
    run_blocking(move || store.record_correction(&session_id, &run_id, &corrected)).await
}

/// The bounded projection of one run's durable events. `None` when the run has
/// no events this process can read — an honest absence, never a fabricated
/// snapshot.
fn run_evidence(
    app: &tauri::AppHandle,
    state: &tauri::State<'_, AppState>,
    run_id: &str,
    user_text: &str,
) -> Option<RunEvidence> {
    let events: Vec<RunEventEnvelope> = with_ledger(app, state, |ledger| {
        ledger.load_events(run_id, 0, crate::skill_learning::MAX_SOURCE_EVENTS * 8)
    })
    .ok()?;
    if events.is_empty() {
        return None;
    }
    Some(evidence_from_events(run_id, user_text, &events))
}

#[tauri::command]
pub async fn skill_learning_learned_skills(
    state: tauri::State<'_, AppState>,
    native: tauri::State<'_, NativeSkillsCommandState>,
    m4: tauri::State<'_, M4CommandState>,
    learning: tauri::State<'_, SkillLearningCommandState>,
) -> Result<Vec<LearnedSkillSummary>, String> {
    let workspace = optional_primary_workspace(&state)?;
    let packages = signed_package_skills(&m4)?;
    let manager = native.manager.clone();
    let store = learning.store.clone();
    run_blocking(move || store.learned_skills(&manager, workspace.as_deref(), &packages)).await
}

#[tauri::command]
pub async fn skill_learning_effectiveness(
    learning: tauri::State<'_, SkillLearningCommandState>,
) -> Result<Vec<EffectivenessRecord>, String> {
    let store = learning.store.clone();
    run_blocking(move || store.effectiveness()).await
}

#[tauri::command]
pub async fn skill_learning_deprecate(
    window: tauri::Window,
    state: tauri::State<'_, AppState>,
    native: tauri::State<'_, NativeSkillsCommandState>,
    m4: tauri::State<'_, M4CommandState>,
    learning: tauri::State<'_, SkillLearningCommandState>,
    scope: SkillScope,
    command: String,
    reason: String,
) -> Result<SkillMutationResult, String> {
    require_main_window(&window)?;
    let workspace = optional_primary_workspace(&state)?;
    let packages = signed_package_skills(&m4)?;
    let manager = native.manager.clone();
    let store = learning.store.clone();
    run_blocking(move || {
        store.deprecate(
            &command,
            scope,
            &reason,
            &manager,
            workspace.as_deref(),
            &packages,
        )
    })
    .await
}

/// Discovery with learned provenance attached — the one call the Settings
/// panel needs to show origin, evidence, and version history next to a skill.
#[tauri::command]
pub async fn skill_learning_discover(
    state: tauri::State<'_, AppState>,
    native: tauri::State<'_, NativeSkillsCommandState>,
    m4: tauri::State<'_, M4CommandState>,
    learning: tauri::State<'_, SkillLearningCommandState>,
) -> Result<Vec<SkillDescriptor>, String> {
    let workspace = optional_primary_workspace(&state)?;
    let packages = signed_package_skills(&m4)?;
    let manager = native.manager.clone();
    let store = learning.store.clone();
    run_blocking(move || {
        let mut descriptors = manager.discover(workspace.as_deref(), &packages)?;
        store.decorate(&mut descriptors)?;
        Ok(descriptors)
    })
    .await
}

/// Restart reconciliation, called once during startup. Resolves an
/// interrupted promotion against what is actually installed.
pub fn reconcile_at_startup(
    store: &SkillLearningStore,
    manager: &crate::native_skills::NativeSkillManager,
    workspace: Option<&PathBuf>,
) {
    if let Err(error) = store.reconcile(manager, workspace.map(PathBuf::as_path), &[]) {
        eprintln!("little-monkey: could not reconcile learned skills after restart: {error}");
    }
}
