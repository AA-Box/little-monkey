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
  | "user_correction"
  | "verification_repair"
  | "successful_novel_procedure"
  | "repeated_failure_resolution";

export type DedupOutcome = "new_skill" | "update_existing" | "possible_duplicate" | "conflict";

export type EvaluationVerdict = "passed" | "failed" | "unevaluated";

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
}

export interface EvaluationCase {
  case_id: string;
  kind: "positive" | "regression";
  name: string;
  prompt: string;
  required_tools: string[];
  forbidden_tools: string[];
}

export interface EvaluationPlan {
  evaluation_id: string;
  candidate_id: string;
  command: string;
  title: string;
  skill_instructions: string;
  allowed_tools: string[];
  cases: EvaluationCase[];
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
  error: string | null;
}

export interface EvaluationRecord {
  evaluation_id: string;
  candidate_id: string;
  cases: EvaluationCase[];
  reports: EvaluationCaseReport[];
  verdict: EvaluationVerdict;
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

export interface SkillUsageReport {
  command: string;
  scope: NativeSkillScope;
  skill_sha256: string;
  run_id: string;
  succeeded: boolean;
  verification_passed: boolean | null;
  tool_failures: string[];
  user_corrected: boolean;
}

export type PromotionOutcome =
  | { kind: "promoted"; candidate: LearningCandidate; mutation: NativeSkillMutationResult }
  | { kind: "awaiting_approval"; candidate: LearningCandidate; reasons: string[] }
  | { kind: "refused"; candidate: LearningCandidate; reasons: string[] };

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
  /** Classifies a finished durable run. The backend reads that run's own
   * events from the ledger; `userText` is the user's turn text, the one input
   * the ledger does not carry. */
  detect: (runId: string, userText: string, scope: NativeSkillScope) =>
    invoke<LearningCandidate | null>("skill_learning_detect", { runId, userText, scope }),
  listCandidates: () => invoke<LearningCandidate[]>("skill_learning_list_candidates"),
  candidate: (candidateId: string) => invoke<LearningCandidate>("skill_learning_candidate", { candidateId }),
  beginReflection: (candidateId: string) =>
    invoke<LearningCandidate>("skill_learning_begin_reflection", { candidateId }),
  stage: (candidateId: string, proposal: CandidateProposal, runId?: string) =>
    invoke<LearningCandidate>("skill_learning_stage", { candidateId, proposal, runId: runId ?? null }),
  planEvaluation: (candidateId: string) => invoke<EvaluationPlan>("skill_learning_plan_evaluation", { candidateId }),
  reportEvaluation: (evaluationId: string, reports: EvaluationCaseReport[]) =>
    invoke<EvaluationRecord>("skill_learning_report_evaluation", { evaluationId, reports }),
  markUnevaluated: (evaluationId: string, reason: string) =>
    invoke<EvaluationRecord>("skill_learning_mark_unevaluated", { evaluationId, reason }),
  evaluations: (candidateId: string) => invoke<EvaluationRecord[]>("skill_learning_evaluations", { candidateId }),
  /** `unattended` is the auto-promote path: strictly narrower than an
   * approval (it also needs the configured mode, a clean policy, and a passing
   * evaluation), never wider. */
  promote: (candidateId: string, approved: boolean, unattended = false) =>
    invoke<PromotionOutcome>("skill_learning_promote", { candidateId, approved, unattended }),
  reject: (candidateId: string, reason: string) =>
    invoke<LearningCandidate>("skill_learning_reject", { candidateId, reason }),
  recordUse: (report: SkillUsageReport) =>
    invoke<LearningCandidate | null>("skill_learning_record_use", { report }),
  learnedSkills: () => invoke<LearnedSkillSummary[]>("skill_learning_learned_skills"),
  deprecate: (scope: NativeSkillScope, command: string, reason: string) =>
    invoke<NativeSkillMutationResult>("skill_learning_deprecate", { scope, command, reason }),
  /** Discovery with learned provenance attached — the same descriptors
   * `native_skills_discover` returns, plus `learned` where this loop installed
   * the exact active content hash. */
  discover: () => invoke<NativeSkillDescriptor[]>("skill_learning_discover"),
};
