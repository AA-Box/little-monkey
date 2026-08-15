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
    evidence_from_events, CandidateProposal, EffectivenessRecord, EvaluationCaseReport,
    EvaluationPlan, EvaluationRecord, LearnedSkillSummary, LearningCandidate, LearningMode,
    PromotionOutcome, SkillLearningStore, SkillUsageReport,
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
    reports: Vec<EvaluationCaseReport>,
) -> Result<EvaluationRecord, String> {
    let store = learning.store.clone();
    run_blocking(move || store.report_evaluation(&evaluation_id, &reports)).await
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

#[tauri::command]
pub async fn skill_learning_evaluations(
    learning: tauri::State<'_, SkillLearningCommandState>,
    candidate_id: String,
) -> Result<Vec<EvaluationRecord>, String> {
    let store = learning.store.clone();
    run_blocking(move || store.evaluations_for(&candidate_id)).await
}

/// The approve-and-install action. `approved` reaching this command is a
/// decision made in the desktop UI's approval dialog; the store still
/// re-derives the digest from the staged bytes and refuses anything the policy
/// blocks.
///
/// `unattended` marks the auto-promote path instead, which is strictly
/// *narrower* than an approval, never wider: it additionally requires the
/// user's configured mode to allow unattended promotion, the policy to have
/// found nothing needing approval, and the evaluation to have passed. It can
/// therefore never install something an explicit approval could not.
#[tauri::command]
pub async fn skill_learning_promote(
    window: tauri::Window,
    state: tauri::State<'_, AppState>,
    native: tauri::State<'_, NativeSkillsCommandState>,
    learning: tauri::State<'_, SkillLearningCommandState>,
    candidate_id: String,
    approved: bool,
    unattended: Option<bool>,
) -> Result<PromotionOutcome, String> {
    require_main_window(&window)?;
    let workspace = optional_primary_workspace(&state)?;
    let manager = native.manager.clone();
    let store = learning.store.clone();
    let unattended = unattended.unwrap_or(false);
    run_blocking(move || {
        store.promote(
            &candidate_id,
            approved && !unattended,
            unattended,
            &manager,
            workspace.as_deref(),
            None,
        )
    })
    .await
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

#[tauri::command]
pub async fn skill_learning_record_use(
    learning: tauri::State<'_, SkillLearningCommandState>,
    report: SkillUsageReport,
) -> Result<Option<LearningCandidate>, String> {
    let store = learning.store.clone();
    run_blocking(move || store.record_use(&report)).await
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
