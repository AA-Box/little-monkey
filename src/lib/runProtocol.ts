import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";

export const RUN_PROTOCOL_SCHEMA_VERSION = 1 as const;
export const RUNS_CHANGED_EVENT = "runs://changed";
export const RUN_CANCELLATION_REQUESTED_EVENT = "runs://cancellation-requested";

export type ClientKind = "desktop" | "cli" | "acp" | "scheduler" | "daemon" | "workflow" | "remote_runner" | "test";
export interface ClientIdentityWire {
  client_id: string;
  instance_id: string;
  kind: ClientKind;
  version: string;
}

export type RunKind =
  | "interactive"
  | "comparison_branch"
  | "comparison_synthesis"
  | "crew_member"
  | "crew_coordinator"
  | "workflow"
  | "scheduled"
  | "browser"
  | "acp"
  | "background"
  | "autonomous_task"
  | "remote_desktop_control";
export type RunStatus =
  | "queued"
  | "running"
  | "waiting_for_permission"
  | "paused"
  | "cancelling"
  | "succeeded"
  | "failed"
  | "cancelled"
  | "needs_reconciliation";

export type CapabilityStateWire = "supported" | "unsupported" | "unknown";
export interface CapabilityAssessmentWire {
  state: CapabilityStateWire;
  evidence: string;
}
export interface FimTemplateMetadataWire {
  prompt_template: string | null;
  prefix_token: string | null;
  suffix_token: string | null;
  middle_token: string | null;
  stop_tokens: string[];
  max_prefix_tokens: number | null;
  max_suffix_tokens: number | null;
  max_completion_tokens: number | null;
}
export interface ModelCapabilitiesSnapshotWire {
  tool_calling: CapabilityAssessmentWire;
  vision: CapabilityAssessmentWire;
  embeddings: CapabilityAssessmentWire;
  structured_output: CapabilityAssessmentWire;
  image_generation: CapabilityAssessmentWire;
  audio: CapabilityAssessmentWire;
  runtime_lifecycle: CapabilityAssessmentWire;
  fim: CapabilityAssessmentWire;
  code_completion: CapabilityAssessmentWire;
  inline_edit: CapabilityAssessmentWire;
  fim_metadata: FimTemplateMetadataWire | null;
}
export type ModelTargetSnapshotWire =
  | {
      kind: "managed_llama";
      target_id: string;
      label: string;
      model_id: string;
      model_path: string;
      capabilities: ModelCapabilitiesSnapshotWire;
      estimated_memory_bytes: number | null;
    }
  | {
      kind: "ollama";
      target_id: string;
      label: string;
      base_url: string;
      model: string;
      is_cloud: boolean;
      capabilities: ModelCapabilitiesSnapshotWire;
      estimated_memory_bytes: number | null;
    }
  | {
      kind: "provider";
      target_id: string;
      label: string;
      provider_id: string;
      endpoint: string;
      model: string;
      credential_ref_id: string;
      capabilities: ModelCapabilitiesSnapshotWire;
    };

export interface RootGrantWire {
  root_id: string;
  canonical_path: string;
  access: "read_only" | "read_write";
  allow_symlinks_within_root: boolean;
}
export interface RepositoryPolicyWire {
  root_id: string;
  owned_worktree_required: boolean;
  allowed_remote_names: string[];
  allowed_branch_prefixes: string[];
  allow_commit: boolean;
  allow_push: boolean;
  allow_create_pull_request: boolean;
  allow_review_comment: boolean;
  allow_merge: boolean;
  allow_force_push: boolean;
}
export interface WorkspaceContextWire {
  workspace_id: string;
  primary_root_id: string;
  roots: RootGrantWire[];
  repository_policy: RepositoryPolicyWire | null;
}
export type PermissionModeWire = "manual" | "acceptEdits" | "smart" | "plan" | "auto" | "bypass";
export type ToolPolicyDecisionWire = "allow" | "prompt" | "deny";
export interface PermissionPolicySnapshotWire {
  mode: PermissionModeWire;
  unattended: boolean;
  approval_timeout_ms: number;
  default_tool_decision: ToolPolicyDecisionWire;
  tool_rules: Array<{ tool: string; decision: ToolPolicyDecisionWire }>;
  allow_network: boolean;
  allow_external_mutations: boolean;
}
export interface RunBudgetsWire {
  wall_time_ms: number;
  max_iterations: number;
  max_model_calls: number;
  max_tool_calls: number;
  max_input_tokens: number;
  max_output_tokens: number;
  max_cost_micros: number | null;
  max_artifact_bytes: number;
  max_event_count: number;
}
export interface RunSpecWire {
  schema_version: typeof RUN_PROTOCOL_SCHEMA_VERSION;
  run_id: string;
  idempotency_key: string;
  created_at_ms: number;
  kind: RunKind;
  /** The Rust host replaces this identity for desktop submissions. */
  submitted_by: ClientIdentityWire;
  task: string;
  instructions: string | null;
  input_artifact_ids: string[];
  target: ModelTargetSnapshotWire;
  workspace: WorkspaceContextWire | null;
  permission_policy: PermissionPolicySnapshotWire;
  budgets: RunBudgetsWire;
}

export interface UsageSnapshotWire {
  input_tokens: number;
  output_tokens: number;
  cached_input_tokens: number;
  model_calls: number;
  tool_calls: number;
  cost_micros: number | null;
}
export type PermissionDecision = "allow_once" | "allow_for_run" | "deny" | "expired";
export type RiskLevel = "low" | "medium" | "high";
export type ToolOutcome = "succeeded" | "failed" | "denied" | "cancelled";
export type ArtifactKind = "file" | "document" | "image" | "audio" | "video" | "archive" | "report" | "other";
export type CheckpointKind = "workspace" | "git" | "conversation" | "external_state";
export type MutationKind = "filesystem" | "git" | "network" | "external_service" | "other";

type Event<T extends string, P> = { type: T; payload: P };
export type AutonomousTaskEventType =
  | "plan_created"
  | "plan_changed"
  | "criterion_added"
  | "criterion_verified"
  | "worker_started"
  | "worker_finished"
  | "worker_failed"
  | "patch_ready"
  | "patch_integrated"
  | "verification_started"
  | "verification_finished"
  | "review_finished"
  | "guidance_received"
  | "delivery_started"
  | "execution_handoff"
  | "task_completed";
export type RunEventWire =
  | Event<"queued", { queue: string | null }>
  | Event<"started", { engine_id: string }>
  | Event<"model_delta", { message_id: string; channel: "assistant" | "status"; text: string }>
  | Event<"tool_proposed", {
      tool_call_id: string;
      tool_name: string;
      arguments: { value: unknown; redaction: "applied" | "not_needed" };
      arguments_sha256: string;
      mutation: boolean;
    }>
  | Event<"permission_requested", {
      request_id: string;
      tool_call_id: string;
      tool_name: string;
      operation_sha256: string;
      expires_at_ms: number;
      detail: string;
      risk_level: RiskLevel | null;
      risk_reason: string | null;
    }>
  | Event<"permission_decided", {
      request_id: string;
      operation_sha256: string;
      decision: PermissionDecision;
      decided_by: ClientIdentityWire;
    }>
  /** Mirrors `RunEvent::RoutingDecided` — which dispatch policy chose this
   * run's target and why (roadmap K9). Every field but the two the reader
   * cannot do without is nullable, because "no policy matched" is a real
   * answer rather than a missing one. */
  | Event<
      "routing_decided",
      {
        task_class: string;
        policy_id: string | null;
        policy_name: string | null;
        chosen_key: string | null;
        changed_from_active: boolean;
        reason: string;
      }
    >
  | Event<"tool_started", { tool_call_id: string }>
  /** Mirrors `RunEvent::SkillInvoked` — the exact native skill version this
   * run froze into its prompt. Written by the invocation path itself, so a
   * historical run identifies the content it ran rather than whatever is
   * installed when the question is asked. */
  | Event<"skill_invoked", { command: string; scope: "global" | "workspace"; sha256: string }>
  | Event<"tool_finished", {
      tool_call_id: string;
      outcome: ToolOutcome;
      output_excerpt: string | null;
      output_sha256: string | null;
      duration_ms: number;
    }>
  | Event<"artifact_added", {
      artifact_id: string;
      kind: ArtifactKind;
      name: string;
      media_type: string;
      content_sha256: string;
      size_bytes: number;
    }>
  | Event<"checkpoint_linked", {
      checkpoint_id: string;
      kind: CheckpointKind;
      label: string;
      content_sha256: string | null;
    }>
  | Event<"verification_finished", {
      verification_id: string;
      name: string;
      passed: boolean;
      summary: string;
      artifact_ids: string[];
      duration_ms: number;
    }>
  | Event<"usage_recorded", { usage: UsageSnapshotWire }>
  | Event<"cancellation_requested", { requested_by: ClientIdentityWire; reason: string | null }>
  | Event<"external_mutation_prepared", {
      mutation_id: string;
      tool_call_id: string;
      kind: MutationKind;
      idempotency_key: string | null;
      summary: string;
    }>
  | Event<"external_mutation_confirmed", {
      mutation_id: string;
      confirmation_ref: string | null;
      summary: string;
    }>
  | Event<"awaiting_approval", {
      request_id: string;
      operation_sha256: string;
      expires_at_ms: number;
      reason: string | null;
    }>
  | Event<"paused", { reason: string | null }>
  | Event<"cancelling", { reason: string | null }>
  | Event<"completed", { summary: string | null; result_artifact_ids: string[]; usage: UsageSnapshotWire }>
  | Event<"failed", { code: string; message: string; retryable: boolean }>
  | Event<"cancelled", { reason: string | null }>
  | Event<"needs_reconciliation", { mutation_id: string; reason: string }>
  | Event<"task_event", {
      task_id: string;
      event_type: AutonomousTaskEventType;
      payload: Record<string, unknown>;
    }>
  /** Mirrors `RunEvent::MigrationDeparted` — this run's frozen process image
   * left for another owned node (roadmap K18). Not terminal: the target can
   * still refuse it, and a run whose move was refused carries on here. */
  | Event<"migration_departed", {
      target_node_id: string;
      payload_sha256: string;
      checkpoint_id: string;
    }>
  /** Mirrors `RunEvent::MigrationArrived` — the first event of the target's
   * half. `origin_last_event_hash` is the seam: it names the origin's chain
   * tip, and because it sits inside the hashed envelope the two halves are one
   * chain without any column spanning both machines. */
  | Event<"migration_arrived", {
      origin_node_id: string;
      origin_last_sequence: number;
      origin_last_event_hash: string;
      payload_sha256: string;
    }>;

export interface RunEventEnvelopeWire {
  schema_version: typeof RUN_PROTOCOL_SCHEMA_VERSION;
  event_id: string;
  run_id: string;
  sequence: number;
  occurred_at_ms: number;
  actor_id: string | null;
  emitter: ClientIdentityWire;
  event: RunEventWire;
}
export interface RunRecord {
  spec: RunSpecWire;
  status: RunStatus;
  lastSequence: number;
  terminalSequence: number | null;
  updatedAtMs: number;
  archivedAtMs: number | null;
}
export interface RunSubmitResponse { run: RunRecord; inserted: boolean }
export interface RunAppendResponse { envelope: RunEventEnvelopeWire; status: RunStatus; terminal: boolean }
export interface RunChangedPayload { runId: string; status: RunStatus; lastSequence: number }
export interface RunCancellationRequestedPayload { runId: string }
export interface RunLedgerIntegrity { ok: boolean; violations: string[] }

export function runProtocolVersion(): Promise<number> {
  return invoke<number>("run_protocol_version");
}
export function submitRun(spec: RunSpecWire): Promise<RunSubmitResponse> {
  return invoke<RunSubmitResponse>("run_submit", { spec });
}
export function appendRunEvent(runId: string, event: RunEventWire, actorId: string | null = null): Promise<RunAppendResponse> {
  return invoke<RunAppendResponse>("run_append_event", { runId, actorId, event });
}
export function decideRunPermission(
  runId: string,
  requestId: string,
  operationSha256: string,
  decision: PermissionDecision,
): Promise<RunAppendResponse> {
  return invoke<RunAppendResponse>("run_decide_permission", { runId, requestId, operationSha256, decision });
}
export function requestRunCancellation(runId: string, reason: string | null = null): Promise<RunAppendResponse> {
  return invoke<RunAppendResponse>("run_request_cancellation", { runId, reason });
}
export function getRun(runId: string): Promise<RunRecord | null> {
  return invoke<RunRecord | null>("run_get", { runId });
}
export function listRuns(limit = 200, includeArchived = false): Promise<RunRecord[]> {
  return invoke<RunRecord[]>("run_list", { limit, includeArchived });
}
export function archiveRun(runId: string): Promise<RunRecord> {
  return invoke<RunRecord>("run_archive", { runId });
}
export function unarchiveRun(runId: string): Promise<RunRecord> {
  return invoke<RunRecord>("run_unarchive", { runId });
}
export function loadRunEvents(runId: string, afterSequence = 0, limit = 1_000): Promise<RunEventEnvelopeWire[]> {
  return invoke<RunEventEnvelopeWire[]>("run_events", { runId, afterSequence, limit });
}
export function checkRunLedgerIntegrity(): Promise<RunLedgerIntegrity> {
  return invoke<RunLedgerIntegrity>("run_integrity_check");
}
export function onRunsChanged(handler: (payload: RunChangedPayload) => void): Promise<UnlistenFn> {
  return listen<RunChangedPayload>(RUNS_CHANGED_EVENT, (event) => handler(event.payload));
}
export function onRunCancellationRequested(
  handler: (payload: RunCancellationRequestedPayload) => void,
): Promise<UnlistenFn> {
  return listen<RunCancellationRequestedPayload>(RUN_CANCELLATION_REQUESTED_EVENT, (event) => handler(event.payload));
}

export const EMPTY_USAGE: UsageSnapshotWire = Object.freeze({
  input_tokens: 0,
  output_tokens: 0,
  cached_input_tokens: 0,
  model_calls: 0,
  tool_calls: 0,
  cost_micros: null,
});
