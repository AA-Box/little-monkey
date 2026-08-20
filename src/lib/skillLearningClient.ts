/**
 * Typed IPC surface for the skill-learning loop (`skill_learning.rs`).
 *
 * Nothing here holds authoritative state. Candidates, evaluations, provenance
 * and the learning mode all live in the durable backend store — this module
 * only ferries them, so a reload, a crash, or the CLI touching the same store
 * cannot disagree with what the UI shows.
 */
import { invoke } from "@tauri-apps/api/core";

import type { NativeSkillDescriptor, NativeSkillMutationResult, NativeSkillScope } from "./nativeSkillsClient";

export type LearningMode = "off" | "suggest_only" | "auto_stage" | "auto_promote_safe";

export type CandidateStatus =
  | "detected"
  | "reflecting"
  | "staged"
  | "evaluating"
  | "awaiting_approval"
  | "promoted"
  | "rejected"
  | "superseded"
  | "rolled_back";

export type LearningSourceKind =
  | "explicit_user_instruction"
  | "manual_run_capture"
  | "user_correction"
  | "verification_repair"
  | "successful_novel_procedure"
  | "repeated_failure_resolution";

export type DedupOutcome = "new_skill" | "update_existing" | "possible_duplicate" | "conflict";

export type EvaluationVerdict = "passed" | "failed" | "unevaluated";

/** How an evaluation was carried out. A `preflight` record only captured the
 * tools a model asked for and executed none of them, so it can never carry a
 * promotion-grade pass — see `EvaluationMode` in `skill_learning.rs`. */
export type EvaluationMode = "preflight" | "real_isolated";

export type RunOutcome = "success" | "failure" | "cancelled";

export interface CandidateResourceFile {
  path: string;
  content: string;
}

export interface CandidateRequirements {
  bins: string[];
  env: string[];
}

export interface CandidateProposal {
  scope: NativeSkillScope;
  title: string;
  description: string;
  proposed_command: string;
  proposed_skill_content: string;
  proposed_resource_files: CandidateResourceFile[];
  allowed_tools: string[];
  requirements: CandidateRequirements;
}

export interface PromotionPolicy {
  auto_promote_allowed: boolean;
  requires_approval: boolean;
  blocking: string[];
  approval_reasons: string[];
}

export interface LearningCandidate {
  candidate_id: string;
  scope: NativeSkillScope;
  status: CandidateStatus;
  title: string;
  description: string;
  source_run_ids: string[];
  source_event_ids: string[];
  source_kind: LearningSourceKind;
  signal_summary: string;
  proposed_command: string;
  proposed_skill_content: string;
  proposed_resource_files: CandidateResourceFile[];
  allowed_tools: string[];
  requirements: CandidateRequirements;
  parent_skill_sha256: string | null;
  candidate_sha256: string;
  created_at_unix_ms: number;
  updated_at_unix_ms: number;
  evaluation_summary: string | null;
  evaluation_ids: string[];
  evaluation_verdict: EvaluationVerdict | null;
  approval_digest: string | null;
  installed_sha256: string | null;
  dedup: DedupOutcome | null;
  dedup_detail: string | null;
  policy: PromotionPolicy | null;
  rejection_reason: string | null;
  staging_path: string | null;
  workspace_path: string | null;
  observed_prompt: string;
  observed_tools: string[];
  /** The bounded backend-owned snapshot of the run this candidate was opened
   * against. Read-only evidence — nothing in it authorizes an install, and the
   * reflection brief the model reads is rendered from it in Rust. */
  evidence: RunEvidence | null;
  correction: CorrectionEvidence | null;
  approval_id: string | null;
}

export interface ToolEvidence {
  event_id: string;
  tool_call_id: string;
  tool_name: string;
  succeeded: boolean;
  mutation: boolean;
  arguments: string | null;
  output_excerpt: string | null;
  outcome: string;
  failure_excerpt: string | null;
  path: string | null;
}

export interface VerificationEvidence {
  event_id: string;
  name: string;
  passed: boolean;
  summary: string;
  sequence: number;
}

export interface InvokedSkillEvidence {
  command: string;
  scope: string;
  sha256: string;
}

export interface RunEvidence {
  run_id: string;
  completed: boolean;
  failed: boolean;
  cancelled: boolean;
  user_text: string;
  tool_calls: ToolEvidence[];
  verifications: VerificationEvidence[];
  changed_files: string[];
  invoked_skills: InvokedSkillEvidence[];
  summary: string;
  failure_signatures: string[];
}

export interface CorrectionEvidence {
  previous_skill_sha256: string;
  previous_run_id: string;
  correction_run_id: string;
  correction_event_ids: string[];
  failure_signature: string | null;
  corrected_execution_succeeded: boolean;
}

/** Backend-owned learning settings. The UI and the CLI read the same values;
 * neither holds an authoritative copy. */
export interface LearningSettings {
  mode: LearningMode;
  allow_global_scope: boolean;
}

export interface EvaluationSandbox {
  arm: string;
  path: string;
}

export interface EvaluationCase {
  case_id: string;
  kind: "positive" | "regression";
  name: string;
  prompt: string;
  required_tools: string[];
  forbidden_tools: string[];
  /** The observed run ended verified, so this case is only reproduced if the
   * arm verifies too. A missing result leaves the evaluation unevaluated —
   * scored in the backend, never here. */
  verification_required: boolean;
}

export interface EvaluationPlan {
  evaluation_id: string;
  candidate_id: string;
  command: string;
  title: string;
  candidate_sha256: string;
  skill_instructions: string;
  allowed_tools: string[];
  cases: EvaluationCase[];
  /** The workspace the observed run happened in. `null` means no reproducible
   * isolated environment can be built, which is an `unevaluated`, never a run
   * against the user's live files. */
  workspace_path: string | null;
  /** The observed run changed files, so the rebuilt starting state must not
   * already look finished. False for a read-only procedure, where an
   * already-passing workspace is the normal condition. */
  observed_mutation: boolean;
}

export interface EvaluationCaseReport {
  case_id: string;
  arm: "baseline" | "candidate";
  completed: boolean;
  used_tools: string[];
  verification_passed: boolean | null;
  latency_ms: number;
  input_tokens: number;
  output_tokens: number;
  cost_micros: number | null;
  permission_requests: string[];
  /** Tool calls that actually ran and failed. Empty for a preflight report,
   * which executes nothing. */
  tool_failures: string[];
  error: string | null;
}

export interface EvaluationRecord {
  evaluation_id: string;
  candidate_id: string;
  cases: EvaluationCase[];
  reports: EvaluationCaseReport[];
  verdict: EvaluationVerdict;
  mode: EvaluationMode;
  summary: string;
  created_at_unix_ms: number;
  finished_at_unix_ms: number | null;
}

export interface LearnedProvenance {
  origin: string;
  candidate_id: string;
  source_run_ids: string[];
  source_kind: string;
  parent_skill_sha256: string | null;
  installed_sha256: string;
  evaluation_ids: string[];
  promotion_policy: string;
  approval_id: string | null;
  promoted_at_unix_ms: number;
}

export interface LearnedSkillSummary {
  command: string;
  scope: NativeSkillScope;
  version: string;
  active_sha256: string;
  enabled: boolean;
  deprecated: boolean;
  deprecation_reason: string | null;
  provenance: LearnedProvenance;
  previous_sha256: string[];
  uses: number;
  failures: number;
  corrections: number;
  last_used_at_unix_ms: number | null;
}

export interface EffectivenessRecord {
  command: string;
  scope: NativeSkillScope;
  skill_sha256: string;
  run_id: string;
  session_id: string | null;
  outcome: RunOutcome;
  verification_passed: boolean | null;
  tool_failures: string[];
  failure_signature: string | null;
  user_corrected: boolean;
  recorded_at_unix_ms: number;
}

export type PromotionOutcome =
  | { kind: "promoted"; candidate: LearningCandidate; mutation: NativeSkillMutationResult }
  | { kind: "awaiting_approval"; candidate: LearningCandidate; reasons: string[] }
  | { kind: "refused"; candidate: LearningCandidate; reasons: string[] };

export type CaptureOutcome =
  | { kind: "created"; candidate: LearningCandidate }
  | { kind: "existing"; candidate: LearningCandidate }
  | { kind: "already_installed"; candidate: LearningCandidate };

/**
 * Last mode the backend reported. Read synchronously by `agentLoop.ts` to
 * decide whether to offer `manage_skill_learning` at all — a per-turn IPC
 * round trip for a value that changes only when the user changes it would be
 * waste. `null` means "not yet known", which callers treat as off: the tool
 * is a capability, so an unknown state must not grant it.
 */
let cachedMode: LearningMode | null = null;

export function cachedLearningMode(): LearningMode | null {
  return cachedMode;
}

export const skillLearningClient = {
  mode: async () => {
    cachedMode = await invoke<LearningMode>("skill_learning_mode");
    return cachedMode;
  },
  setMode: async (mode: LearningMode) => {
    cachedMode = await invoke<LearningMode>("skill_learning_set_mode", { mode });
    return cachedMode;
  },
  settings: async () => {
    const settings = await invoke<LearningSettings>("skill_learning_settings");
    cachedMode = settings.mode;
    return settings;
  },
  setSettings: async (settings: LearningSettings) => {
    const next = await invoke<LearningSettings>("skill_learning_set_settings", { settings });
    cachedMode = next.mode;
    return next;
  },
  /** The bounded evidence brief the reflection pass reads, rendered in Rust
   * from the snapshot the backend persisted with the candidate. */
  reflectionBrief: (candidateId: string) =>
    invoke<string>("skill_learning_reflection_brief", { candidateId }),
  /** Classifies a finished durable run. The backend reads that run's own
   * events from the ledger; `userText` is the user's turn text, the one input
   * the ledger does not carry. */
  detect: (runId: string, userText: string, scope: NativeSkillScope) =>
    invoke<LearningCandidate | null>("skill_learning_detect", { runId, userText, scope }),
  captureEligibility: (runId: string, userText: string) =>
    invoke<NativeSkillScope | null>("skill_learning_capture_eligibility", { runId, userText }),
  scopeForRun: (runId: string) =>
    invoke<NativeSkillScope | null>("skill_learning_scope_for_run", { runId }),
  capture: (runId: string, userText: string) =>
    invoke<CaptureOutcome>("skill_learning_capture", { runId, userText }),
  listCandidates: () => invoke<LearningCandidate[]>("skill_learning_list_candidates"),
  candidate: (candidateId: string) => invoke<LearningCandidate>("skill_learning_candidate", { candidateId }),
  beginReflection: (candidateId: string) =>
    invoke<LearningCandidate>("skill_learning_begin_reflection", { candidateId }),
  stage: (candidateId: string, proposal: CandidateProposal, runId?: string) =>
    invoke<LearningCandidate>("skill_learning_stage", { candidateId, proposal, runId: runId ?? null }),
  planEvaluation: (candidateId: string) => invoke<EvaluationPlan>("skill_learning_plan_evaluation", { candidateId }),
  /** `mode` says how the reports were produced. Only `real_isolated` — arms
   * that actually executed their tool calls — can score a promotion-grade
   * pass; the backend downgrades a clean `preflight` to `unevaluated`. */
  reportEvaluation: (evaluationId: string, mode: EvaluationMode, reports: EvaluationCaseReport[]) =>
    invoke<EvaluationRecord>("skill_learning_report_evaluation", { evaluationId, mode, reports }),
  markUnevaluated: (evaluationId: string, reason: string) =>
    invoke<EvaluationRecord>("skill_learning_mark_unevaluated", { evaluationId, reason }),
  evaluations: (candidateId: string) => invoke<EvaluationRecord[]>("skill_learning_evaluations", { candidateId }),
  /** One disposable copy of the candidate's own workspace per arm, all made
   * before any arm runs. The backend resolves what they are copies of. */
  createSandboxes: (evaluationId: string, arms: string[]) =>
    invoke<EvaluationSandbox[]>("skill_learning_create_sandboxes", { evaluationId, arms }),
  destroySandboxes: (evaluationId: string) =>
    invoke<void>("skill_learning_destroy_sandboxes", { evaluationId }),
  /** Asks the user, through the app's own permission system, and installs on
   * an allow decision. There is no `approved` boolean: the approval is a
   * durable decision bound to this exact candidate's digest, issued in Rust.
   * `unattended` is the auto-promote path instead — strictly narrower than an
   * approval (it also needs the configured mode, a clean policy, and a passing
   * real isolated evaluation), never wider. */
  promote: (candidateId: string, unattended = false) =>
    invoke<PromotionOutcome>("skill_learning_promote", { candidateId, unattended }),
  reject: (candidateId: string, reason: string) =>
    invoke<LearningCandidate>("skill_learning_reject", { candidateId, reason }),
  /** Finalizes effectiveness for a run that reached a terminal state. The
   * backend reads which learned versions the run actually invoked from that
   * run's own durable events — a caller cannot name a hash. */
  finalizeRun: (runId: string, sessionId: string | null) =>
    invoke<LearningCandidate[]>("skill_learning_finalize_run", { runId, sessionId }),
  /** Attributes a correction to the learned version the session's previous
   * turn used, durably — the attribution survives a reload and a restart. */
  recordCorrection: (sessionId: string, runId: string, userText: string) =>
    invoke<LearningCandidate | null>("skill_learning_record_correction", { sessionId, runId, userText }),
  effectiveness: () => invoke<EffectivenessRecord[]>("skill_learning_effectiveness"),
  learnedSkills: () => invoke<LearnedSkillSummary[]>("skill_learning_learned_skills"),
  deprecate: (scope: NativeSkillScope, command: string, reason: string) =>
    invoke<NativeSkillMutationResult>("skill_learning_deprecate", { scope, command, reason }),
  /** Discovery with learned provenance attached — the same descriptors
   * `native_skills_discover` returns, plus `learned` where this loop installed
   * the exact active content hash. */
  discover: () => invoke<NativeSkillDescriptor[]>("skill_learning_discover"),
};
