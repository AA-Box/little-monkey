import { invoke } from "@tauri-apps/api/core";

import { agentWorktreeClient } from "./agentWorktree";
import { resolveTarget, snapshotForResolvedTarget } from "./targetRouting";
import type { ResolvedTarget } from "./turnEngine";
import {
  attachDurableRun, beginDurableRun, defaultRunBudgets, modelTargetToRunWire,
  permissionPolicyForRun, workspaceToRunWire,
} from "./durableRun";
import { runSubagentTask, runSubagentTaskStructured } from "./subagent";
import { executeDeliveryMutation, prepareDeliveryMutation, type DeliveryMutation } from "./gitDelivery";
import {
  buildWorkerContext, canRunTaskNodesTogether, createAutonomousTask, createTaskPlan, getReadyTaskPlanNodes,
  hasAuthoritativeAcceptanceEvidence, installTaskPlan, taskEvent, taskEventToRunEvent, validateTaskPlan,
  type AutonomousTask, type CreateAutonomousTaskInput, type TaskArtifact, type TaskEvent,
  type TaskExecutionOwner, type TaskExecutionPlacement, type TaskPlan, type TaskPlanNode, type TaskPlanningContext, type TaskUsage, type TaskWorker, type VerificationEvidence,
} from "./autonomousTask";
import { appendRunEvent } from "./runProtocol";
import type { RunSpecWire } from "./runProtocol";
import { usePermissionStore } from "../store/permissionStore";
import { primaryRoot, useWorkspaceStore } from "../store/workspaceStore";
import { effortForTarget } from "../store/modelStore";

export interface TaskNodeResult {
  ok: boolean;
  summary: string;
  failureCode?: string;
  failureKind?: string;
  worktreePath?: string;
  artifacts?: TaskArtifact[];
  evidence?: VerificationEvidence[];
  changedFiles?: string[];
  mutation?: { beforeRevision: string; afterRevision: string; changedFiles: string[]; patchDigest: string };
  worktree?: { id: string; path: string; branch: string; baseRevision: string; diffDigest: string };
  workspaceRevision?: string;
  usage?: Partial<TaskUsage>;
  awaitingApproval?: boolean;
  approval?: { requestId: string; operationDigest: string; expiresAtMs: number; confirmationPhrase?: string };
  deliveryStep?: "commit" | "push" | "create_draft_pr" | "update_draft_pr";
  review?: StructuredReviewResult;
  resultId?: string;
  reviewRequired?: boolean;
}

export interface StructuredReviewResult { verdict: "pass" | "changes_required"; findings: Array<{ severity: "blocking" | "warning" | "suggestion"; path: string; title: string; body: string }>; filesReviewed: string[]; acceptanceCriteria: string[]; securityFindings: string[]; testCoverageFindings: string[]; }
export interface TaskPlanResult { plan: TaskPlan; acceptanceCriteria?: AutonomousTask["acceptanceCriteria"]; planningContext?: Partial<TaskPlanningContext>; summary?: string; }

export interface AutonomousTaskRuntimeContext {
  resolvedTarget: ResolvedTarget;
  signal: AbortSignal;
  worker: TaskWorker;
  placement?: TaskExecutionPlacement;
  planningContext?: TaskPlanningContext;
  approval?: { requestId: string; confirmation: string; operationDigest?: string };
  beforeSideEffect?: () => void;
  beforeSideEffectAsync?: () => Promise<void>;
}

export interface AutonomousTaskRuntime {
  plan?: (task: AutonomousTask, context: AutonomousTaskRuntimeContext) => Promise<TaskPlanResult>;
  executeNode: (task: AutonomousTask, node: TaskPlanNode, context: AutonomousTaskRuntimeContext) => Promise<TaskNodeResult>;
  integrate?: (task: AutonomousTask, node: TaskPlanNode, results: TaskNodeResult[], context: AutonomousTaskRuntimeContext) => Promise<TaskNodeResult>;
  verify?: (task: AutonomousTask, node: TaskPlanNode, context: AutonomousTaskRuntimeContext) => Promise<TaskNodeResult>;
  review?: (task: AutonomousTask, node: TaskPlanNode, context: AutonomousTaskRuntimeContext) => Promise<TaskNodeResult>;
  deliver?: (task: AutonomousTask, context: AutonomousTaskRuntimeContext) => Promise<TaskNodeResult>;
}

export type AutonomousTaskPlacementAdapter = (
  task: AutonomousTask,
  node: TaskPlanNode,
  context: AutonomousTaskRuntimeContext,
) => Promise<TaskNodeResult>;

export interface AutonomousTaskPlacementAdapters {
  /** Generic executor seam. `context.placement.kind` selects the configured target. */
  target?: AutonomousTaskPlacementAdapter;
  /** @deprecated use `target`; retained for persisted integrations during migration. */
  docker?: AutonomousTaskPlacementAdapter;
  /** @deprecated use `target`; retained for persisted integrations during migration. */
  remote_node?: AutonomousTaskPlacementAdapter;
}

function consumedPlacementNode(node: TaskPlanNode, kind: string): TaskPlanNode {
  const requestedPlacement = structuredClone(node.executionPlacement);
  return {
    ...structuredClone(node),
    dependencies: [],
    isolation: node.isolation,
    requestedExecutionPlacement: requestedPlacement,
    executionPlacement: {
      kind: "local",
      targetId: "local",
      nodeId: node.nodeId,
      reason: `already fulfilled by ${kind} placement executor`,
      requestedPlacement,
      placementFulfilled: true,
    },
    executionRequirements: node.executionRequirements ? { ...structuredClone(node.executionRequirements), isolation: node.isolation } : undefined,
  };
}

export function buildAutonomousPlacementRunSpec(task: AutonomousTask, node: TaskPlanNode, kind: string): RunSpecWire {
  const placedNode = consumedPlacementNode(node, kind);
  const taskSnapshot = {
    ...structuredClone(task),
    plan: task.plan ? { ...structuredClone(task.plan), nodes: [placedNode] } : null,
    outcome: "RUNNING",
    executionOwner: { kind: "remote", instanceId: `placement-${task.taskId}-${node.nodeId}`, leaseEpoch: 1, leaseExpiresAtMs: Date.now() + task.budgetSnapshot.wallTimeMs },
    updatedAtMs: Date.now(),
  };
  return {
    schema_version: 1,
    run_id: `${task.taskId}-${node.nodeId}`,
    idempotency_key: `autonomous-placement/${task.taskId}/${node.nodeId}/${node.attempt}`,
    created_at_ms: Date.now(),
    kind: "autonomous_task",
    submitted_by: { client_id: "little-monkey-desktop-autonomous", instance_id: task.taskId, kind: "desktop", version: "1.0.0" },
    task: placedNode.objective,
    instructions: "Execute only the frozen autonomous task node and return durable evidence.",
    input_artifact_ids: [],
    target: modelTargetToRunWire(task.targetSnapshot),
    workspace: workspaceToRunWire(task.workspaceRoots, "read_write"),
    permission_policy: permissionPolicyForRun(task.permissionSnapshot.mode as Parameters<typeof permissionPolicyForRun>[0], { unattended: true, allowNetwork: task.permissionSnapshot.allowNetwork, allowExternalMutations: task.permissionSnapshot.allowExternalMutations }),
    budgets: {
      wall_time_ms: task.budgetSnapshot.wallTimeMs,
      max_iterations: Math.max(1, Math.min(10_000, task.budgetSnapshot.maxModelCalls)),
      max_model_calls: task.budgetSnapshot.maxModelCalls,
      max_tool_calls: task.budgetSnapshot.maxToolCalls,
      max_input_tokens: 1_000_000_000,
      max_output_tokens: 1_000_000_000,
      max_cost_micros: task.budgetSnapshot.maxCostMicros ?? null,
      max_artifact_bytes: task.budgetSnapshot.maxArtifactBytes ?? 1 << 30,
      max_event_count: 1_000_000,
    },
    autonomous_task: {
      schema_version: 1,
      task_id: `${task.taskId}-${node.nodeId}`,
      objective: placedNode.objective,
      source: task.source,
      relevant_files: placedNode.relevantFiles ?? task.planningContext?.relevantFiles ?? [],
      current_workspace_revision: task.workspaceRevision ?? task.planningContext?.currentWorkspaceRevision ?? "unknown",
      max_repair_rounds: task.budgetSnapshot.maxRepairRounds,
      max_workers: 1,
      guidance: task.guidance.map((item) => ({ guidance_id: item.guidanceId, text: item.text, applies_to: item.appliesTo })),
      delivery_intent: "leave_worktree",
      execution_owner: { kind: "remote", instance_id: `placement-${task.taskId}-${node.nodeId}`, lease_epoch: 1, lease_expires_at_ms: Date.now() + task.budgetSnapshot.wallTimeMs },
      previous_execution_owner: null,
      task_snapshot: taskSnapshot,
      completed_nodes: [],
      next_node_id: placedNode.nodeId,
    },
  };
}

function defaultAutonomousTaskPlacementAdapters(): AutonomousTaskPlacementAdapters {
  const executionError = (error: unknown): { code: string; detail: string } => {
    const raw = error instanceof Error ? error.message : String(error);
    try {
      const parsed = JSON.parse(raw) as { code?: unknown; detail?: unknown; error?: unknown };
      if (typeof parsed.code === "string") return { code: parsed.code, detail: String(parsed.detail ?? parsed.error ?? raw) };
    } catch { /* Tauri may return a plain structured error string. */ }
    const match = raw.match(/^([A-Z][A-Z0-9_]+):\s*(.*)$/s);
    return match ? { code: match[1], detail: match[2] } : { code: "EXECUTION_FAILED", detail: raw };
  };
  const execute = async (kind: string, task: AutonomousTask, node: TaskPlanNode): Promise<TaskNodeResult> => {
    const spec = buildAutonomousPlacementRunSpec(task, node, kind);
    const targetId = node.executionPlacement?.targetId?.trim();
    if (!targetId) {
      return {
        ok: false,
        failureCode: "FAILED",
        failureKind: "EXECUTION_FAILED",
        summary: `Execution placement '${kind}' has no target id.`,
      };
    }
    try {
      type Recovery = TaskNodeResult & { known: boolean; pending?: boolean; remoteRunId?: string };
      let recovery = await invoke<Recovery>("autonomous_task_recover_node", { targetId, runId: spec.run_id });
      while (recovery.known && recovery.pending) {
        await new Promise((resolve) => setTimeout(resolve, 500));
        recovery = await invoke<Recovery>("autonomous_task_recover_node", { targetId, runId: spec.run_id });
      }
      if (recovery.known) {
        const { known: _known, pending: _pending, remoteRunId: _remoteRunId, ...result } = recovery;
        return result;
      }
      return await invoke<TaskNodeResult>("autonomous_task_place_node", { request: { kind, targetId, runSpec: spec } });
    } catch (error) {
      const parsed = executionError(error);
      return { ok: false, failureCode: parsed.code, failureKind: parsed.code, summary: `${kind} execution failed: ${parsed.detail}` };
    }
  };
  return {
    target: (task, node, context) => execute(context.placement?.kind ?? "local", task, node),
  };
}

export interface AutonomousTaskEventSink {
  (event: TaskEvent): Promise<void> | void;
}

export interface RunAutonomousTaskParams {
  task: AutonomousTask;
  resolvedTarget: ResolvedTarget;
  runtime: AutonomousTaskRuntime;
  signal?: AbortSignal;
  eventSink?: AutonomousTaskEventSink;
  onUpdate?: (task: AutonomousTask) => void;
  control?: AutonomousTaskControl;
  approval?: { requestId: string; confirmation: string; operationDigest?: string };
  ownerFence?: (owner?: TaskExecutionOwner) => Promise<void>;
}

export class AutonomousTaskControl {
  private paused = false;
  private wake: (() => void) | null = null;
  private readonly controller = new AbortController();
  private guidance: AutonomousTask["guidance"] = [];
  private approval: { requestId: string; confirmation: string; operationDigest?: string } | null = null;
  private activeExecutions = 0;
  private coordinatorWork = 0;
  private safePointWaiters: Array<() => void> = [];
  private ownerReplacement: TaskExecutionOwner | null = null;

  get signal(): AbortSignal { return this.controller.signal; }
  get isPaused(): boolean { return this.paused; }
  pause(): void { if (!this.controller.signal.aborted) this.paused = true; }
  resume(): void { this.paused = false; this.wake?.(); this.wake = null; }
  cancel(): void { this.controller.abort(); this.resume(); }
  beginExecution(): void { this.activeExecutions += 1; }
  endExecution(): void {
    this.activeExecutions = Math.max(0, this.activeExecutions - 1);
    this.resolveSafePointWaiters();
  }
  beginCoordinatorWork(): void { this.coordinatorWork += 1; }
  endCoordinatorWork(): void { this.coordinatorWork = Math.max(0, this.coordinatorWork - 1); this.resolveSafePointWaiters(); }
  private resolveSafePointWaiters(): void {
    if (this.activeExecutions !== 0 || this.coordinatorWork !== 0) return;
    const waiters = this.safePointWaiters.splice(0);
    for (const resolve of waiters) resolve();
  }
  waitForSafePoint(): Promise<void> {
    if (this.activeExecutions === 0 && this.coordinatorWork === 0) return Promise.resolve();
    return new Promise((resolve) => this.safePointWaiters.push(resolve));
  }
  async freezeForHandoff<T>(readSnapshot: () => T): Promise<T> {
    this.pause();
    await this.waitForSafePoint();
    return readSnapshot();
  }
  relinquish(): void { this.controller.abort(); this.resume(); }
  adoptExecutionOwner(owner: TaskExecutionOwner): void { this.ownerReplacement = structuredClone(owner); }
  takeExecutionOwner(): TaskExecutionOwner | null { const next = this.ownerReplacement; this.ownerReplacement = null; return next; }
  guide(guidance: AutonomousTask["guidance"][number]): void { this.guidance.push(structuredClone(guidance)); this.guidance = this.guidance.slice(-8); this.wake?.(); this.wake = null; }
  drainGuidance(): AutonomousTask["guidance"] { const next = this.guidance; this.guidance = []; return next; }
  approve(requestId: string, confirmation: string, operationDigest?: string): void { this.approval = { requestId, confirmation, operationDigest }; this.wake?.(); this.wake = null; }
  drainApproval(): { requestId: string; confirmation: string; operationDigest?: string } | null { const next = this.approval; this.approval = null; return next; }
  async waitIfPaused(): Promise<void> {
    if (!this.paused) return;
    await new Promise<void>((resolve) => { this.wake = resolve; });
    if (this.controller.signal.aborted) throw new Error("Task cancelled.");
  }
}

function mergeSignals(left: AbortSignal, right?: AbortSignal): AbortSignal {
  if (!right) return left;
  const controller = new AbortController();
  const abort = () => controller.abort();
  if (left.aborted || right.aborted) controller.abort();
  else { left.addEventListener("abort", abort, { once: true }); right.addEventListener("abort", abort, { once: true }); }
  return controller.signal;
}

function workerFor(task: AutonomousTask, node: TaskPlanNode): TaskWorker {
  const executionPlacement = node.executionPlacement ?? { kind: node.isolation === "worktree" ? "worktree" as const : "local" as const, targetId: "local", nodeId: node.nodeId, reason: node.isolation === "worktree" ? "mutating worker isolation" : "local coordinator execution" };
  return {
    workerId: `worker-${task.taskId}-${node.nodeId}-${node.attempt + 1}`,
    nodeId: node.nodeId,
    profile: node.taskClass === "investigation" || node.taskClass === "review" ? "explore" : "code",
    isolation: node.isolation,
    targetSnapshot: structuredClone(task.targetSnapshot),
    startedAtMs: null,
    finishedAtMs: null,
    executionPlacement,
  };
}

function updateNode(task: AutonomousTask, nodeId: string, update: Partial<TaskPlanNode>): AutonomousTask {
  if (!task.plan) return task;
  return { ...task, plan: { ...task.plan, nodes: task.plan.nodes.map((node) => node.nodeId === nodeId ? { ...node, ...update } : node) }, updatedAtMs: Date.now() };
}

function addEvidence(task: AutonomousTask, evidence: VerificationEvidence): AutonomousTask {
  const criteria = task.acceptanceCriteria.map((criterion) => {
    if (evidence.criterionId !== criterion.id) return criterion;
    const evidenceIds = criterion.evidenceIds.includes(evidence.evidenceId) ? criterion.evidenceIds : [...criterion.evidenceIds, evidence.evidenceId];
    return { ...criterion, evidenceIds, status: evidence.passed ? "passed" as const : "failed" as const };
  });
  return { ...task, acceptanceCriteria: criteria, verificationEvidence: [...task.verificationEvidence, evidence], updatedAtMs: Date.now() };
}

function makeEvidence(name: string, criterionId: string | null, passed: boolean, summary: string, authoritative: boolean, exitCode: number | null = null, durationMs = 0, metadata: Partial<VerificationEvidence> = {}): VerificationEvidence {
  return { evidenceId: `evidence-${Date.now()}-${Math.random().toString(36).slice(2)}`, criterionId, name, passed, authoritative, stale: false, summary: summary.slice(0, 8_000), exitCode, durationMs, createdAtMs: Date.now(), ...metadata };
}

function addUsage(task: AutonomousTask, usage: Partial<TaskUsage> = {}): AutonomousTask {
  const current = task.usage ?? { modelCalls: 0, toolCalls: 0, inputTokens: 0, outputTokens: 0, costMicros: 0, artifactBytes: 0, workersStarted: 0 };
  return { ...task, usage: { modelCalls: current.modelCalls + (usage.modelCalls ?? 0), toolCalls: current.toolCalls + (usage.toolCalls ?? 0), inputTokens: current.inputTokens + (usage.inputTokens ?? 0), outputTokens: current.outputTokens + (usage.outputTokens ?? 0), costMicros: current.costMicros + (usage.costMicros ?? 0), artifactBytes: current.artifactBytes + (usage.artifactBytes ?? 0), workersStarted: current.workersStarted + (usage.workersStarted ?? 0) }, updatedAtMs: Date.now() };
}

function advanceWorkspaceRevision(task: AutonomousTask, revision?: string): AutonomousTask {
  if (!revision || revision === task.workspaceRevision) return task;
  return { ...task, workspaceRevision: revision, verificationEvidence: task.verificationEvidence.map((evidence) => ({ ...evidence, stale: true })), acceptanceCriteria: task.acceptanceCriteria.map((criterion) => criterion.method === "verification_command" || criterion.method === "review" ? { ...criterion, status: "pending", evidenceIds: [] } : criterion), updatedAtMs: Date.now() };
}

function mutatingRepairSources(plan: TaskPlan, failedNode: TaskPlanNode): TaskPlanNode[] {
  const byId = new Map(plan.nodes.map((node) => [node.nodeId, node]));
  const sources: TaskPlanNode[] = [];
  const integrations: TaskPlanNode[] = [];
  const seen = new Set<string>();
  const visit = (node: TaskPlanNode): void => {
    if (seen.has(node.nodeId)) return;
    seen.add(node.nodeId);
    if (node.taskClass === "implementation") sources.push(node);
    if (node.taskClass === "integration") integrations.push(node);
    for (const dependency of node.dependencies) {
      const parent = byId.get(dependency);
      if (parent) visit(parent);
    }
  };
  if (failedNode.taskClass === "implementation") sources.push(failedNode);
  else visit(failedNode);
  if (integrations.length > 0) return [...new Map(integrations.map((node) => [node.nodeId, node])).values()];
  return [...new Map(sources.map((node) => [node.nodeId, node])).values()];
}

function scopeContains(scope: string, path: string): boolean {
  const normalizedScope = scope.replace(/^\.\//, "").replace(/\/$/, "");
  const normalizedPath = path.replace(/^\.\//, "");
  return normalizedScope === "workspace" || normalizedScope === normalizedPath || normalizedPath.startsWith(`${normalizedScope}/`);
}

function outOfScopeFiles(node: TaskPlanNode, changedFiles: string[]): string[] {
  return [...new Set(changedFiles.filter((path) => !node.mutationScope.some((scope) => scopeContains(scope, path))))];
}

function isExecutionTargetLost(result: TaskNodeResult): boolean {
  const targetLossCodes = new Set([
    "EXECUTION_TARGET_LOST",
    "TARGET_UNREACHABLE",
    "TARGET_IDENTITY_CHANGED",
    "HOST_KEY_CHANGED",
    "PROTOCOL_INCOMPATIBLE",
    "RUNNER_LOST",
    "RUNNER_RESTARTED",
  ]);
  return targetLossCodes.has(result.failureCode ?? "")
    || targetLossCodes.has(result.failureKind ?? "")
    || /^[A-Z][A-Z0-9_]+\s*:/i.test(result.summary) && targetLossCodes.has(result.summary.split(":", 1)[0]);
}

function insertRepairNode(task: AutonomousTask, failedNode: TaskPlanNode, summary: string): AutonomousTask {
  if (!task.plan || failedNode.taskClass === "delivery") return task;
  const sources = mutatingRepairSources(task.plan, failedNode);
  if (sources.length === 0) return task;
  const mutatingFailedNode = failedNode.taskClass === "implementation" || failedNode.taskClass === "integration";
  const repairSources = mutatingFailedNode ? [failedNode] : sources;
  if (repairSources.some((source) => source.mutationScope.length === 0 || !source.capabilities?.includes("mutate"))) return task;
  const repairBase = `${failedNode.nodeId}-repair-${task.repairRounds}`;
  const usedIds = new Set(task.plan.nodes.map((node) => node.nodeId));
  const repairIds = repairSources.map((_, index) => {
    let candidate = repairSources.length === 1 ? repairBase : `${repairBase}-${index + 1}`;
    let suffix = 1;
    while (usedIds.has(candidate)) candidate = `${repairBase}-${suffix++}`;
    usedIds.add(candidate);
    return candidate;
  });
  let repairDependencies = failedNode.dependencies;
  let retriedDependencies = repairIds;
  const nodes = task.plan.nodes.map((node) => {
    if (failedNode.taskClass === "review" && node.taskClass === "verification" && failedNode.dependencies.includes(node.nodeId)) {
      repairDependencies = node.dependencies;
      retriedDependencies = failedNode.dependencies;
      return { ...node, dependencies: repairIds, status: "pending" as const, attempt: 0, workerId: null, resultSummary: null, mutationRevision: null };
    }
    return node;
  });
  const retriedNode = { ...failedNode, dependencies: retriedDependencies, status: "pending" as const, attempt: 0, workerId: null, resultSummary: null, mutationRevision: null };
  const repairNodes = repairSources.map((source, index) => {
    const isolation = mutatingFailedNode ? failedNode.isolation : source.isolation;
    const capabilities = [...new Set((mutatingFailedNode ? failedNode.capabilities : source.capabilities) ?? ["read", "mutate", "verify"])]
      .filter((capability) => capability !== "mutate").concat("mutate");
    const requirements = mutatingFailedNode ? failedNode.executionRequirements : source.executionRequirements;
    const executionPlacement = structuredClone(mutatingFailedNode ? failedNode.executionPlacement : source.executionPlacement);
    if (executionPlacement) executionPlacement.nodeId = repairIds[index];
    return { ...failedNode, nodeId: repairIds[index], taskClass: "implementation" as const, objective: `Diagnose and repair ${failedNode.nodeId} using bounded failure evidence: ${summary.slice(0, 2_000)}`, dependencies: repairDependencies, mutationScope: [...source.mutationScope].sort(), isolation, capabilities, executionPlacement, requestedExecutionPlacement: mutatingFailedNode ? failedNode.requestedExecutionPlacement : source.requestedExecutionPlacement, executionRequirements: { needsWorkspaceWrite: true, needsNetwork: requirements?.needsNetwork ?? false, isolation, platform: requirements?.platform }, relevantFiles: source.relevantFiles, upstreamDecisions: source.upstreamDecisions, status: repairDependencies.length ? "pending" as const : "ready" as const, attempt: 0, workerId: null, resultSummary: null, mutationRevision: null, repairOf: failedNode.nodeId };
  });
  return { ...task, plan: { ...task.plan, revision: task.plan.revision + 1, nodes: [...nodes.map((node) => node.nodeId === failedNode.nodeId ? retriedNode : node), ...repairNodes] }, updatedAtMs: Date.now() };
}

function parseStructuredReview(value: string): StructuredReviewResult | null {
  const candidate = value.match(/\{[\s\S]*\}/)?.[0];
  if (!candidate) return null;
  try { const parsed = JSON.parse(candidate) as Partial<StructuredReviewResult>; if (parsed.verdict !== "pass" && parsed.verdict !== "changes_required") return null; if (!Array.isArray(parsed.findings) || !Array.isArray(parsed.filesReviewed) || !Array.isArray(parsed.acceptanceCriteria) || !Array.isArray(parsed.securityFindings) || !Array.isArray(parsed.testCoverageFindings)) return null; return parsed as StructuredReviewResult; } catch { return null; }
}

function validatePlannedTaskPlan(plan: TaskPlan, task: AutonomousTask): boolean {
  const errors = validateTaskPlan(plan, task.planningContext);
  if (errors.length > 0) return false;
  const required = new Set(["verification", "review"]);
  if (![...required].every((taskClass) => plan.nodes.some((node) => node.taskClass === taskClass))) return false;
  return plan.nodes.every((node) => node.taskClass !== "implementation" || (node.relevantFiles?.length ?? 0) > 0);
}

async function textDigest(value: string): Promise<string | undefined> {
  try { return Array.from(new Uint8Array(await crypto.subtle.digest("SHA-256", new TextEncoder().encode(value)))).map((byte) => byte.toString(16).padStart(2, "0")).join(""); } catch { return undefined; }
}

export async function runAutonomousTask(params: RunAutonomousTaskParams): Promise<AutonomousTask> {
  const signal = params.signal ?? new AbortController().signal;
  let task = params.task;
  const emit = async (eventType: Parameters<typeof taskEvent>[0], payload: Record<string, unknown> = {}): Promise<void> => {
    const event = taskEvent(eventType, task, payload);
    await params.eventSink?.(event);
    params.onUpdate?.(task);
  };
  const stopIfNeeded = (): void => { if (signal.aborted) throw new Error("Task cancelled."); };

  try {
    if (!task.plan) {
      const planningNode: TaskPlanNode = { nodeId: "planner", taskClass: "investigation", objective: "Build a structured repository-aware plan.", dependencies: [], mutationScope: [], isolation: "shared", status: "running", attempt: 1, workerId: null, resultSummary: null };
      const planningWorker = workerFor(task, planningNode);
      const planningContext: AutonomousTaskRuntimeContext = { resolvedTarget: params.resolvedTarget, signal, worker: planningWorker, planningContext: task.planningContext };
      const hasPlanner = Boolean(params.runtime.plan);
      const planned = await params.runtime.plan?.(task, planningContext);
      let acceptedPlan: TaskPlan | null = null;
      if (planned?.plan && validatePlannedTaskPlan(planned.plan, task)) acceptedPlan = planned.plan;
      if (hasPlanner && !acceptedPlan) throw new Error("Autonomous planner returned an invalid or incomplete repository-aware DAG.");
      const plannedBase = acceptedPlan ?? createTaskPlan(task.objective, task.constraints.strategy ?? "PLAN", Math.min(task.budgetSnapshot.maxWorkers, task.budgetSnapshot.maxConcurrentWorkers ?? task.budgetSnapshot.maxWorkers), task.planningContext);
      const selectedPlacement = task.constraints.executionPlacement;
      const plan = selectedPlacement
        ? { ...plannedBase, revision: plannedBase.revision + 1, nodes: plannedBase.nodes.map((node) => ({ ...node, executionPlacement: { ...selectedPlacement, nodeId: node.nodeId }, requestedExecutionPlacement: undefined })) }
        : plannedBase;
      const plannedCriteria = planned?.acceptanceCriteria;
      const criteriaAreStructured = Boolean(plannedCriteria && plannedCriteria.length >= 3 && plannedCriteria.some((criterion) => criterion.method === "verification_command") && plannedCriteria.some((criterion) => criterion.method === "review") && plannedCriteria.every((criterion) => Boolean(criterion.provenance?.kind && criterion.provenance.fragment)));
      if (hasPlanner && !criteriaAreStructured) throw new Error("Autonomous planner returned incomplete acceptance criteria; verification and review provenance are required.");
      if (acceptedPlan && criteriaAreStructured) task = { ...task, acceptanceCriteria: structuredClone(plannedCriteria!) };
      if (planned?.planningContext) task = { ...task, planningContext: { currentWorkspaceRevision: planned.planningContext.currentWorkspaceRevision ?? task.planningContext?.currentWorkspaceRevision ?? "unknown", relevantFiles: planned.planningContext.relevantFiles ?? task.planningContext?.relevantFiles ?? [], repositoryConventions: planned.planningContext.repositoryConventions ?? task.planningContext?.repositoryConventions ?? [], sourceMaterial: planned.planningContext.sourceMaterial ?? task.planningContext?.sourceMaterial ?? [], dependencyArtifactIds: planned.planningContext.dependencyArtifactIds ?? task.planningContext?.dependencyArtifactIds ?? [], upstreamDecisions: planned.planningContext.upstreamDecisions ?? task.planningContext?.upstreamDecisions ?? [] }, workspaceRevision: planned.planningContext.currentWorkspaceRevision ?? task.workspaceRevision };
      task = installTaskPlan(task, plan);
      if (task.deliveryIntent !== "leave_worktree" && !plan.nodes.some((node) => node.taskClass === "delivery")) {
        const last = plan.nodes[plan.nodes.length - 1];
        task = installTaskPlan(task, { ...plan, revision: plan.revision + 1, nodes: [...plan.nodes, { nodeId: "delivery", taskClass: "delivery", objective: `Prepare ${task.deliveryIntent} only after authoritative verification and review.`, dependencies: last ? [last.nodeId] : [], mutationScope: ["git"], isolation: "shared", status: last ? "pending" : "ready", attempt: 0, workerId: null, resultSummary: null, capabilities: ["read", "git"], repairOf: null }] });
      }
      await emit("plan_created", { plan_id: plan.planId, strategy: plan.strategy });
      for (const criterion of task.acceptanceCriteria) await emit("criterion_added", { criterion_id: criterion.id, description: criterion.description });
    }
    const startedAt = Date.now();
    const results = new Map<string, TaskNodeResult>();
    for (const worker of task.workers) if (worker.worktree || worker.mutation || worker.artifacts || worker.usage) results.set(worker.nodeId, { ok: true, summary: worker.resultSummary ?? "Recovered worker result from the durable task snapshot.", worktree: worker.worktree, changedFiles: worker.changedFiles, mutation: worker.mutation, artifacts: worker.artifacts, usage: worker.usage });
    if (task.waitingApproval && task.plan) {
      const approval = params.approval ?? params.control?.drainApproval();
      if (approval?.requestId === task.waitingApproval.requestId) {
        task = updateNode(task, task.waitingApproval.nodeId, { status: "ready" });
        task = { ...task, outcome: "RUNNING", waitingReason: null, waitingApproval: null, updatedAtMs: Date.now() };
      }
    }
    while (task.outcome === "RUNNING" && task.plan && task.plan.nodes.some((node) => node.status === "pending" || node.status === "ready" || node.status === "running")) {
      stopIfNeeded();
      const replacement = params.control?.takeExecutionOwner();
      if (replacement) {
        task = { ...task, executionOwner: replacement, updatedAtMs: Date.now() };
        await emit("execution_owner_rollback", { owner: replacement });
      }
      const incomingGuidance = params.control?.drainGuidance() ?? [];
      if (incomingGuidance.length) { task = { ...task, guidance: [...task.guidance, ...incomingGuidance].slice(-32), updatedAtMs: Date.now() }; await emit("guidance_received", { guidance: incomingGuidance }); }
      await params.control?.waitIfPaused();
      if (Date.now() - startedAt > task.budgetSnapshot.wallTimeMs) {
        task = { ...task, outcome: "BUDGET_EXHAUSTED", summary: "Task wall-time budget exhausted.", updatedAtMs: Date.now() };
        break;
      }
      const currentPlan = task.plan;
      if (!currentPlan) break;
      const ready = getReadyTaskPlanNodes(currentPlan);
      if (ready.length === 0) {
        const pending = currentPlan.nodes.some((node) => node.status === "pending" || node.status === "ready" || node.status === "running");
        if (pending) task = { ...task, outcome: "FAILED", summary: "Plan made no further progress.", updatedAtMs: Date.now() };
        break;
      }
      const batch: TaskPlanNode[] = [];
      for (const candidate of ready) {
        if (batch.length >= (task.budgetSnapshot.maxConcurrentWorkers ?? task.budgetSnapshot.maxWorkers)) break;
        if (batch.every((selected) => canRunTaskNodesTogether(selected, candidate))) batch.push(candidate);
        else if (batch.length === 0) batch.push(candidate);
      }
      const workers = batch.map((node) => workerFor(task, node));
      for (const worker of workers) {
        task = updateNode(task, worker.nodeId, { status: "running", workerId: worker.workerId, attempt: (task.plan?.nodes.find((node) => node.nodeId === worker.nodeId)?.attempt ?? 0) + 1 });
        task = { ...task, workers: [...task.workers, { ...worker, startedAtMs: Date.now() }], updatedAtMs: Date.now() };
        task = addUsage(task, { workersStarted: 1 });
        await emit("worker_started", { worker_id: worker.workerId, node_id: worker.nodeId });
        if (task.plan?.nodes.find((node) => node.nodeId === worker.nodeId)?.taskClass === "verification") await emit("verification_started", { node_id: worker.nodeId });
      }
      const executions = workers.map(async (worker) => {
        const node = task.plan!.nodes.find((candidate) => candidate.nodeId === worker.nodeId)!;
        const context: AutonomousTaskRuntimeContext = { resolvedTarget: params.resolvedTarget, signal, worker, placement: worker.executionPlacement, planningContext: task.planningContext, approval: params.approval ?? params.control?.drainApproval() ?? undefined, beforeSideEffect: stopIfNeeded, beforeSideEffectAsync: () => params.ownerFence?.(task.executionOwner) ?? Promise.resolve() };
        params.control?.beginExecution();
        try {
          await params.ownerFence?.(task.executionOwner);
          const externalPlacement = context.placement
            && context.placement.kind !== "local"
            && context.placement.kind !== "worktree";
          if (externalPlacement) return await params.runtime.executeNode(task, node, context);
          if (node.taskClass === "integration" && params.runtime.integrate) return await params.runtime.integrate(task, node, [...results.values()], context);
          if (node.taskClass === "verification" && params.runtime.verify) return await params.runtime.verify(task, node, context);
          if (node.taskClass === "review" && params.runtime.review) return await params.runtime.review(task, node, context);
          if (node.taskClass === "delivery" && params.runtime.deliver) return await params.runtime.deliver(task, context);
          return await params.runtime.executeNode(task, node, context);
        } finally { params.control?.endExecution(); }
      });
      params.control?.beginCoordinatorWork();
      try {
        const completed = await Promise.all(executions);
        for (let index = 0; index < completed.length; index += 1) {
        stopIfNeeded();
        const worker = workers[index];
        const node = task.plan?.nodes.find((candidate) => candidate.nodeId === worker.nodeId);
        if (!node) throw new Error(`Task node ${worker.nodeId} disappeared from the plan.`);
        const rawResult = completed[index];
        const mutatingNode = node.taskClass === "implementation" || node.taskClass === "integration";
        const unauthorized = mutatingNode ? outOfScopeFiles(node, rawResult.changedFiles ?? rawResult.mutation?.changedFiles ?? []) : [];
        const scopedResult = unauthorized.length > 0
          ? { ...rawResult, ok: false, summary: `Node '${node.nodeId}' changed files outside its frozen mutation scope: ${unauthorized.join(", ")}` }
          : rawResult;
        const result: TaskNodeResult = isExecutionTargetLost(scopedResult)
          ? { ...scopedResult, ok: false }
          : scopedResult.ok && mutatingNode && scopedResult.reviewRequired && scopedResult.resultId
            ? scopedResult
            : scopedResult.ok && mutatingNode && !scopedResult.mutation && !scopedResult.workspaceRevision
              ? { ...scopedResult, ok: false, summary: "Mutating node did not return a revision-bound mutation record." }
              : scopedResult.ok && mutatingNode && !scopedResult.mutation && scopedResult.workspaceRevision
                ? { ...scopedResult, mutation: { beforeRevision: task.workspaceRevision ?? "unknown", afterRevision: scopedResult.workspaceRevision, changedFiles: scopedResult.changedFiles ?? [], patchDigest: scopedResult.workspaceRevision } }
                : scopedResult;
        results.set(node.nodeId, result);
        const awaitingApproval = !result.ok && result.awaitingApproval === true;
        const status: TaskPlanNode["status"] = result.ok ? "succeeded" : awaitingApproval ? "waiting_approval" : "failed";
        task = updateNode(task, node.nodeId, { status, resultSummary: result.summary.slice(0, 2_000) });
        if (isExecutionTargetLost(result)) task = { ...task, outcome: "EXECUTION_TARGET_LOST", summary: result.summary, updatedAtMs: Date.now() };
        task = addUsage(task, result.usage ?? { modelCalls: 1 });
        if (task.usage && ((task.usage.modelCalls > task.budgetSnapshot.maxModelCalls) || (task.usage.toolCalls > task.budgetSnapshot.maxToolCalls) || (task.budgetSnapshot.maxCostMicros !== null && task.usage.costMicros > (task.budgetSnapshot.maxCostMicros ?? Number.MAX_SAFE_INTEGER)))) { task = { ...task, outcome: "BUDGET_EXHAUSTED", summary: "Model, tool, or cost budget exhausted.", updatedAtMs: Date.now() }; }
        task = advanceWorkspaceRevision(task, result.workspaceRevision);
        if (result.mutation) {
          task = advanceWorkspaceRevision(task, result.mutation.afterRevision);
          task = { ...task, plan: task.plan ? { ...task.plan, nodes: task.plan.nodes.map((candidate) => candidate.nodeId === node.nodeId ? { ...candidate, mutationRevision: result.mutation?.afterRevision } : candidate) } : null };
        }
        task = addUsage(task, { artifactBytes: (result.artifacts ?? []).reduce((total, artifact) => total + artifact.label.length + (artifact.path?.length ?? 0), 0) });
        if ((task.budgetSnapshot.maxArtifactBytes ?? Number.MAX_SAFE_INTEGER) < (task.usage?.artifactBytes ?? 0)) task = { ...task, outcome: "BUDGET_EXHAUSTED", summary: "Artifact budget exhausted.", updatedAtMs: Date.now() };
        task = { ...task, artifacts: [...task.artifacts, ...(result.artifacts ?? [])], updatedAtMs: Date.now() };
        task = { ...task, workers: task.workers.map((entry) => entry.workerId === worker.workerId ? { ...entry, finishedAtMs: Date.now() } : entry), updatedAtMs: Date.now() };
        task = { ...task, workers: task.workers.map((entry) => entry.workerId === worker.workerId ? { ...entry, worktree: result.worktree, changedFiles: result.changedFiles, mutation: result.mutation, artifacts: result.artifacts, resultId: result.resultId, failureCode: result.failureCode, failureKind: result.failureKind, resultSummary: result.summary.slice(0, 2_000), usage: result.usage } : entry), updatedAtMs: Date.now() };
        if (result.deliveryStep) task = { ...task, deliveryStep: result.deliveryStep };
        if (awaitingApproval && result.approval) task = { ...task, outcome: "WAITING_APPROVAL", waitingReason: result.summary, waitingApproval: { ...result.approval, nodeId: node.nodeId }, updatedAtMs: Date.now() };
        if (result.evidence) for (const evidence of result.evidence) {
          task = addEvidence(task, evidence);
          if (evidence.criterionId) await emit("criterion_verified", { criterion_id: evidence.criterionId, evidence_id: evidence.evidenceId, passed: evidence.passed });
        }
        await emit(result.ok ? "worker_finished" : "worker_failed", { worker_id: worker.workerId, node_id: worker.nodeId, summary: result.summary });
        if (result.artifacts?.some((artifact) => artifact.kind === "patch")) await emit("patch_ready", { node_id: node.nodeId, artifact_ids: result.artifacts.filter((artifact) => artifact.kind === "patch").map((artifact) => artifact.artifactId) });
        if (node.taskClass === "verification") await emit("verification_finished", { node_id: node.nodeId, passed: result.ok, summary: result.summary });
        if (node.taskClass === "review") await emit("review_finished", { node_id: node.nodeId, passed: result.ok, summary: result.summary });
        if (node.taskClass === "integration" && result.ok) await emit("patch_integrated", { node_id: node.nodeId, summary: result.summary });
        }
        if (task.outcome === "WAITING_APPROVAL") break;
        if (task.outcome === "EXECUTION_TARGET_LOST") break;
        const failed = completed.some((result) => !result.ok);
        if (failed) {
          if (task.repairRounds < task.budgetSnapshot.maxRepairRounds) {
            task = { ...task, repairRounds: task.repairRounds + 1, updatedAtMs: Date.now() };
            for (const node of batch) { const failedNode = task.plan?.nodes.find((candidate) => candidate.nodeId === node.nodeId); const failedResult = completed.find((_result, resultIndex) => workers[resultIndex]?.nodeId === node.nodeId); if (failedNode?.status === "failed" && failedResult && !failedResult.ok) { const repaired = insertRepairNode(task, failedNode, failedResult.summary); if (repaired === task) task = { ...task, outcome: "FAILED", summary: `No causal mutating source is available to repair '${failedNode.nodeId}'.`, updatedAtMs: Date.now() }; else task = repaired; } }
            await emit("plan_changed", { reason: "Bounded repair round requested after worker failure.", repair_round: task.repairRounds });
          } else {
            task = { ...task, outcome: task.plan?.nodes.some((node) => node.status === "succeeded") ? "PARTIALLY_COMPLETED" : "FAILED", summary: "One or more task nodes failed after bounded repair rounds.", updatedAtMs: Date.now() };
            break;
          }
        }
      } finally {
        params.control?.endCoordinatorWork();
      }
    }
    if (task.outcome === "RUNNING" && task.plan) {
      const verificationCriteria = task.acceptanceCriteria.filter((criterion) => criterion.method === "verification_command");
      if (!hasAuthoritativeAcceptanceEvidence(task)) task = { ...task, outcome: task.verificationEvidence.some((evidence) => verificationCriteria.some((criterion) => evidence.criterionId === criterion.id && !evidence.passed)) ? "VERIFICATION_FAILED" : "WAITING_USER", summary: "Task is waiting for authoritative acceptance evidence.", updatedAtMs: Date.now() };
      else if (task.deliveryIntent !== "leave_worktree" && task.plan.nodes.some((node) => node.taskClass === "delivery")) {
        const delivery = results.get("delivery");
        if (!delivery) task = { ...task, outcome: "DELIVERY_FAILED", summary: "Delivery node produced no result.", updatedAtMs: Date.now() };
        else if (!delivery.ok) task = { ...task, outcome: delivery.awaitingApproval ? "WAITING_APPROVAL" : "DELIVERY_FAILED", waitingReason: delivery.awaitingApproval ? delivery.summary : null, summary: delivery.summary, updatedAtMs: Date.now() };
        else task = { ...task, outcome: "SUCCEEDED", summary: delivery.summary, updatedAtMs: Date.now() };
      } else if (params.runtime.deliver) {
        const plan = task.plan;
        if (!plan) throw new Error("Task plan disappeared before delivery.");
        const worker = workerFor(task, plan.nodes[plan.nodes.length - 1]);
        await emit("delivery_started", {});
        const delivery = await params.runtime.deliver(task, { resolvedTarget: params.resolvedTarget, signal, worker });
        if (!delivery.ok) task = { ...task, outcome: delivery.awaitingApproval ? "WAITING_APPROVAL" : "DELIVERY_FAILED", waitingReason: delivery.awaitingApproval ? delivery.summary : null, summary: delivery.summary, updatedAtMs: Date.now() };
        else task = { ...task, outcome: "SUCCEEDED", summary: delivery.summary, updatedAtMs: Date.now() };
      } else task = { ...task, outcome: "SUCCEEDED", summary: "Task completed with authoritative acceptance evidence.", updatedAtMs: Date.now() };
    }
  } catch (error) {
    const summary = error instanceof Error ? error.message : String(error);
    task = { ...task, outcome: signal.aborted ? "CANCELLED" : /^EXECUTION_TARGET_LOST\s*:/i.test(summary) ? "EXECUTION_TARGET_LOST" : "FAILED", summary, updatedAtMs: Date.now() };
  }
  await emit("task_completed", { outcome: task.outcome, summary: task.summary });
  return task;
}

export function defaultAutonomousTaskRuntime(resolvedTarget: ResolvedTarget, placementAdapters: AutonomousTaskPlacementAdapters = {}): AutonomousTaskRuntime {
  return {
    plan: async (task, context) => {
      const result = await runSubagentTask({ sessionId: task.sessionId ?? task.taskId, runId: task.taskId, parentCheckpointId: null, taskId: `${task.taskId}-planner`, description: "Plan autonomous task", prompt: `Inspect the repository and return JSON only. Build a context-aware DAG with real file scopes; never invent numbered scopes. Schema: {plan:{planId,strategy,nodes:[...]},acceptanceCriteria:[...],summary}\n${buildWorkerContext(task, { nodeId: "planner", taskClass: "investigation", objective: task.objective, dependencies: [], mutationScope: [], isolation: "shared", status: "running", attempt: 1, workerId: null, resultSummary: null })}`, profile: "explore", capabilities: ["read"], target: resolvedTarget, effort: effortForTarget(resolvedTarget), parentSignal: context.signal });
      const candidate = result.match(/\{[\s\S]*\}/)?.[0];
      if (!candidate) throw new Error("Autonomous planner did not return structured JSON.");
      try {
        const parsed = JSON.parse(candidate) as Partial<TaskPlanResult>;
        if (!parsed.plan || !Array.isArray(parsed.plan.nodes)) throw new Error("planner JSON did not contain plan.nodes");
        return { plan: parsed.plan as TaskPlan, acceptanceCriteria: Array.isArray(parsed.acceptanceCriteria) ? parsed.acceptanceCriteria as AutonomousTask["acceptanceCriteria"] : undefined, planningContext: parsed.planningContext, summary: parsed.summary };
      } catch (error) {
        throw new Error(`Autonomous planner response was invalid: ${error instanceof Error ? error.message : String(error)}`);
      }
    },
    executeNode: async (task, node, context) => {
      const placementKind = node.executionPlacement?.kind;
      if (placementKind && placementKind !== "local" && placementKind !== "worktree") {
        const adapter = placementAdapters.target
          ?? (placementAdapters as unknown as Record<string, AutonomousTaskPlacementAdapter | undefined>)[placementKind];
        if (!adapter) return { ok: false, summary: "Execution placement '" + placementKind + "' has no registered backend adapter; refusing local fallback." };
        return adapter(task, node, context);
      }
      const required = new Set(node.capabilities ?? ["read"]);
      const allowed = new Set(["read", "verify"]);
      if (!/(^|\b)(readonly|plan)(\b|$)/i.test(task.permissionSnapshot.mode)) allowed.add("mutate");
      if ((task.budgetSnapshot.maxNestingDepth ?? 0) > 0) allowed.add("delegate");
      if (task.permissionSnapshot.allowNetwork) allowed.add("network");
      if (task.permissionSnapshot.allowExternalMutations) allowed.add("git");
      for (const capability of [...required]) if (!allowed.has(capability)) {
        return { ok: false, summary: `Node '${node.nodeId}' requested capability '${capability}' outside the frozen permission ceiling.` };
      }
      const beforeRevision = await agentWorktreeClient.workspaceRevision().catch(() => task.workspaceRevision ?? "unknown");
      const mutatingNode = node.taskClass === "implementation" || node.taskClass === "integration";
      const snapshot = mutatingNode && node.isolation === "shared" ? await agentWorktreeClient.workspaceSnapshot().catch(() => undefined) : undefined;
      let result: Awaited<ReturnType<typeof runSubagentTaskStructured>>;
      try {
        result = await runSubagentTaskStructured({ sessionId: task.sessionId ?? task.taskId, runId: task.taskId, parentCheckpointId: null, taskId: `${task.taskId}-${node.nodeId}`, description: node.objective.slice(0, 120), prompt: buildWorkerContext(task, node), profile: node.taskClass === "investigation" || node.taskClass === "review" ? "explore" : "code", capabilities: [...required], isolation: node.isolation === "worktree" ? "worktree" : undefined, target: resolvedTarget, effort: effortForTarget(resolvedTarget), parentSignal: context.signal, beforeToolCall: context.beforeSideEffectAsync ?? context.beforeSideEffect });
      } catch (error) {
        if (snapshot) {
          const changed = await agentWorktreeClient.workspaceChangedFilesSinceSnapshot(snapshot.id).catch(() => []);
          const unauthorized = outOfScopeFiles(node, changed);
          if (unauthorized.length) await agentWorktreeClient.workspaceRestorePaths(snapshot.id, unauthorized).catch(() => undefined);
          await agentWorktreeClient.workspaceSnapshotDiscard(snapshot.id).catch(() => undefined);
        }
        throw error;
      }
      const afterRevision = node.isolation === "shared" ? await agentWorktreeClient.workspaceRevision().catch(() => undefined) : result.worktree?.diffDigest;
      const changedFiles = snapshot ? await agentWorktreeClient.workspaceChangedFilesSinceSnapshot(snapshot.id).catch(() => result.changedFiles ?? []) : node.isolation === "shared" ? await agentWorktreeClient.workspaceChangedFiles().catch(() => result.changedFiles ?? []) : result.changedFiles ?? [];
      const unauthorized = mutatingNode ? outOfScopeFiles(node, changedFiles) : [];
      if (snapshot) {
        if (unauthorized.length) await agentWorktreeClient.workspaceRestorePaths(snapshot.id, unauthorized).catch(() => undefined);
        await agentWorktreeClient.workspaceSnapshotDiscard(snapshot.id).catch(() => undefined);
      }
      if (unauthorized.length) return { ok: false, summary: `Node '${node.nodeId}' changed files outside its frozen mutation scope: ${unauthorized.join(", ")}`, changedFiles: unauthorized };
      const patchDigest = node.isolation === "worktree" ? result.worktree?.diffDigest : afterRevision ? await textDigest(JSON.stringify({ afterRevision, changedFiles })) ?? afterRevision : undefined;
      return { ok: result.outcome === "done", summary: result.report.slice(0, 8_000), worktree: result.worktree, changedFiles, mutation: mutatingNode && afterRevision ? { beforeRevision, afterRevision, changedFiles, patchDigest: patchDigest ?? afterRevision } : undefined, artifacts: result.worktree ? [{ artifactId: `patch-${node.nodeId}-${Date.now()}`, kind: "patch", label: `Patch from ${node.nodeId}`, path: result.worktree.path, digest: result.worktree.diffDigest, createdAtMs: Date.now(), workspaceRevision: result.worktree.baseRevision }] : undefined, usage: result.usage };
    },
    integrate: async (task, _node, results, context) => {
      const workerResults = results.filter((result) => result.worktree);
      const paths = workerResults.map((result) => result.worktree!.path);
      const declaredScopes = new Set(task.plan?.nodes.filter((candidate) => candidate.taskClass === "implementation").flatMap((candidate) => candidate.mutationScope) ?? []);
      const changed = workerResults.flatMap((result) => result.changedFiles ?? []);
      const overlap = changed.filter((path, index) => changed.indexOf(path) !== index);
      const outOfScope = changed.filter((path) => !declaredScopes.has("workspace") && ![...declaredScopes].some((scope) => path === scope || path.startsWith(`${scope.replace(/\/$/, "")}/`)));
      if (overlap.length) return { ok: false, summary: `Integration rejected overlapping worker edits: ${[...new Set(overlap)].join(", ")}`, changedFiles: changed };
      if (outOfScope.length) return { ok: false, summary: `Integration rejected out-of-scope worker edits: ${[...new Set(outOfScope)].join(", ")}`, changedFiles: changed };
      const applied: string[] = [];
      for (const path of paths) {
        if (context.signal.aborted) return { ok: false, summary: "Integration cancelled." };
        await context.beforeSideEffectAsync?.();
        try { const response = await agentWorktreeClient.apply(path); applied.push(...response.applied_files); }
        catch (error) { return { ok: false, summary: `Integration conflict: ${error instanceof Error ? error.message : String(error)}` }; }
      }
      const workspaceRevision = await agentWorktreeClient.workspaceRevision().catch(() => undefined);
      return { ok: true, summary: `Integrated ${paths.length} worker patch(es) after scope inspection.`, changedFiles: applied, workspaceRevision, mutation: workspaceRevision ? { beforeRevision: task.workspaceRevision ?? "unknown", afterRevision: workspaceRevision, changedFiles: applied, patchDigest: await textDigest(applied.join("\n")) ?? workspaceRevision } : undefined };
    },
    verify: async (task) => {
      const config = await invoke<{ commands: Array<{ id: string; label: string; command: string; enabled: boolean }> }>("verify_get_config", {}).catch(() => ({ commands: [] }));
      const enabled = config.commands.filter((command) => command.enabled);
      if (enabled.length === 0) return { ok: true, summary: "No configured verification commands; acceptance requires user confirmation." };
      const evidence: VerificationEvidence[] = [];
      for (const command of enabled) {
        const started = Date.now();
        const beforeRevision = await agentWorktreeClient.workspaceRevision().catch(() => task.workspaceRevision);
        const result = await invoke<{ code: number | null; timedOut: boolean; stdout?: string; stderr?: string }>("verify_run", { commandId: command.id, turnId: task.taskId }).catch((error) => ({ code: null, timedOut: false, stdout: "", stderr: error instanceof Error ? error.message : String(error) }));
        const testedRevision = await agentWorktreeClient.workspaceRevision().catch(() => task.workspaceRevision);
        const stale = Boolean(beforeRevision && testedRevision && beforeRevision !== testedRevision);
        const passed = !result.timedOut && result.code === 0 && !stale;
        const criterion = task.acceptanceCriteria.find((candidate) => candidate.method === "verification_command");
        evidence.push(makeEvidence(command.label || command.id, criterion?.id ?? null, passed, stale ? "Verification command ran while the workspace revision changed." : `${result.stdout ?? ""}\n${result.stderr ?? ""}`.trim() || (passed ? "passed" : "failed"), true, result.code, Date.now() - started, { stale, command: command.command, commandDigest: await textDigest(command.command), workspaceRevision: testedRevision, testedRevision, source: "command" }));
        if (!passed) return { ok: false, summary: evidence[evidence.length - 1].summary, evidence };
      }
      return { ok: true, summary: `${enabled.length} verification command(s) passed.`, evidence };
    },
    review: async (task, node, context) => {
      const beforeRevision = await agentWorktreeClient.workspaceRevision().catch(() => task.workspaceRevision);
      const result = await runSubagentTask({ sessionId: task.sessionId ?? task.taskId, runId: task.taskId, parentCheckpointId: null, taskId: `${task.taskId}-${node.nodeId}`, description: "Review task diff", prompt: `${buildWorkerContext(task, node)}\nReview only. Do not mutate files. Return strict JSON with verdict, findings, filesReviewed, acceptanceCriteria, securityFindings, testCoverageFindings.`, profile: "explore", target: resolvedTarget, effort: effortForTarget(resolvedTarget), parentSignal: context.signal, beforeToolCall: context.beforeSideEffectAsync ?? context.beforeSideEffect });
      const testedRevision = await agentWorktreeClient.workspaceRevision().catch(() => task.workspaceRevision);
      const mutated = Boolean(beforeRevision && testedRevision && beforeRevision !== testedRevision);
      const review = parseStructuredReview(result); const criteria = task.acceptanceCriteria.filter((candidate) => candidate.method === "review");
      const staleSummary = "Review mutated the workspace after its evidence boundary; the evidence is stale.";
      if (!review) return { ok: false, summary: mutated ? staleSummary : "Review response was not valid structured JSON; no review evidence accepted.", evidence: criteria.map((criterion) => makeEvidence("Structured worker diff review", criterion.id, false, mutated ? staleSummary : result, false, null, 0, { stale: mutated, workspaceRevision: testedRevision, testedRevision, source: "review" })) };
      const passed = !mutated && review.verdict === "pass" && !review.findings.some((finding) => finding.severity === "blocking");
      return { ok: passed, summary: mutated ? staleSummary : JSON.stringify(review).slice(0, 8_000), review, evidence: criteria.map((criterion) => makeEvidence("Structured worker diff review", criterion.id, passed, mutated ? staleSummary : JSON.stringify(review), !mutated, passed ? 0 : 1, 0, { stale: mutated, workspaceRevision: testedRevision, testedRevision, source: "review" })) };
    },
    deliver: async (task, context) => {
      if (task.deliveryIntent === "leave_worktree") return { ok: true, summary: "Changes left in the managed worktree." };
      const target = task.deliveryTarget;
      if (!target) return { ok: false, summary: "Git delivery requires an app-owned delivery worktree." };
      const step = task.deliveryStep ?? "commit";
      const paths = target.changedFiles?.length ? target.changedFiles : task.workers.flatMap((worker) => worker.changedFiles ?? []);
      if (step === "update_draft_pr" && (!target.prNumber || target.prNumber < 1)) return { ok: false, summary: "Updating a draft PR requires the existing PR number in the frozen delivery target." };
      const mutation: DeliveryMutation = step === "commit"
        ? { kind: "commit", payload: { worktreeId: target.worktreeId, paths: paths.length ? paths : ["."], message: task.objective.slice(0, 120) } }
        : step === "push"
          ? { kind: "push", payload: { worktreeId: target.worktreeId, remote: target.remote } }
          : step === "update_draft_pr"
          ? { kind: "update_draft_pr", payload: { worktreeId: target.worktreeId, prNumber: target.prNumber!, title: target.title, body: target.body } }
            : { kind: "create_draft_pr", payload: { worktreeId: target.worktreeId, base: target.base, title: target.title, body: target.body } };
      const nextStep = (): TaskNodeResult["deliveryStep"] => {
        if (step === "commit" && (task.deliveryIntent === "push_owned_branch" || task.deliveryIntent === "open_or_update_pr")) return "push";
        if (step === "push" && task.deliveryIntent === "open_or_update_pr") return "create_draft_pr";
        return undefined;
      };
      const approval = context.approval;
      if (!approval) {
        await context.beforeSideEffectAsync?.();
        const preview = await prepareDeliveryMutation(mutation);
        return { ok: false, awaitingApproval: true, summary: `${preview.summary}. Type ${preview.confirmationPhrase} to approve.`, approval: { requestId: `delivery-${task.taskId}-${step}`, operationDigest: preview.digest, expiresAtMs: preview.expiresAtMs, confirmationPhrase: preview.confirmationPhrase }, deliveryStep: step };
      }
      await context.beforeSideEffectAsync?.();
      const result = await executeDeliveryMutation(mutation, approval.operationDigest ?? task.waitingApproval?.operationDigest ?? approval.requestId, approval.confirmation);
      const following = nextStep();
      if (following) {
        const nextMutation = following === "push"
          ? { kind: "push", payload: { worktreeId: target.worktreeId, remote: target.remote } } as DeliveryMutation
          : { kind: "create_draft_pr", payload: { worktreeId: target.worktreeId, base: target.base, title: target.title, body: target.body } } as DeliveryMutation;
        const preview = await prepareDeliveryMutation(nextMutation);
        return { ok: false, awaitingApproval: true, summary: `${preview.summary}. Type ${preview.confirmationPhrase} to approve.`, approval: { requestId: `delivery-${task.taskId}-${following}`, operationDigest: preview.digest, expiresAtMs: preview.expiresAtMs, confirmationPhrase: preview.confirmationPhrase }, deliveryStep: following };
      }
      return { ok: true, summary: `${step} completed through the approved Git delivery layer.`, artifacts: [{ artifactId: `delivery-${task.taskId}-${step}`, kind: "delivery", label: JSON.stringify(result).slice(0, 500), path: null, digest: task.waitingApproval?.operationDigest ?? null, createdAtMs: Date.now() }] };
    },
  };
}

export interface StartAutonomousTaskParams {
  objective: string;
  sessionId?: string | null;
  constraints?: CreateAutonomousTaskInput["constraints"];
  planningContext?: CreateAutonomousTaskInput["planningContext"];
  deliveryIntent?: CreateAutonomousTaskInput["deliveryIntent"];
  deliveryTarget?: CreateAutonomousTaskInput["deliveryTarget"];
  onUpdate?: (task: AutonomousTask) => void;
  runtime?: AutonomousTaskRuntime;
  signal?: AbortSignal;
}

export interface StartedAutonomousTask {
  task: AutonomousTask;
  runId: string;
  control: AutonomousTaskControl;
  completion: Promise<AutonomousTask>;
}

export interface AutonomousTaskDaemonSubmission {
  job_id: string;
  run_id: string;
  state: string;
  rollback_owner?: { kind: "desktop"; instance_id: string; lease_epoch: number; lease_expires_at_ms: number };
  error?: string;
}

export async function submitAutonomousTaskToDaemon(task: AutonomousTask): Promise<AutonomousTaskDaemonSubmission> {
  const root = primaryRoot(task.workspaceRoots);
  if (!root) throw new Error("Autonomous task handoff requires an open workspace.");
  const target = task.targetSnapshot.kind === "provider"
    ? { provider: task.targetSnapshot.providerId, model: task.targetSnapshot.model, ollama: null, local_url: null, managed_model: null }
    : task.targetSnapshot.kind === "ollama"
      ? { provider: null, model: null, ollama: task.targetSnapshot.model, local_url: null, managed_model: null }
      : { provider: null, model: null, ollama: null, local_url: null, managed_model: task.targetSnapshot.modelId };
  const daemonOwner = { kind: "daemon" as const, instanceId: `daemon-${task.taskId}`, leaseEpoch: task.executionOwner.leaseEpoch + 1, leaseExpiresAtMs: Date.now() + task.budgetSnapshot.wallTimeMs };
  return invoke<AutonomousTaskDaemonSubmission>("autonomous_task_submit", {
    request: {
      taskId: task.taskId,
      recipe: {
        version: 1,
        name: `autonomous-${task.taskId}`,
        description: "Frozen Universal AutonomousTask handoff to the resident daemon.",
        target,
        workspace: root.path,
        permission_mode: "auto",
        system: "Execute the frozen Universal AutonomousTask snapshot through its bounded plan, implementation, verification, review, and delivery phases.",
        prompt: task.objective,
        params: {},
        max_iterations: Math.min(128, Math.max(1, task.budgetSnapshot.maxModelCalls)),
        timeout_seconds: Math.max(1, Math.ceil(task.budgetSnapshot.wallTimeMs / 1_000)),
        output: { json: true },
        channel_send: null,
        desktop_turn: null,
        placed_run: null,
        autonomous_task: {
          schema_version: 1,
          task_id: task.taskId,
          objective: task.objective,
          source: task.source,
          relevant_files: task.planningContext?.relevantFiles ?? [],
          current_workspace_revision: task.workspaceRevision ?? task.planningContext?.currentWorkspaceRevision ?? "unknown",
          max_repair_rounds: task.budgetSnapshot.maxRepairRounds,
          max_workers: Math.min(16, Math.max(1, task.budgetSnapshot.maxWorkers)),
          guidance: task.guidance.map((item) => ({ guidance_id: item.guidanceId, text: item.text, applies_to: item.appliesTo })),
          delivery_intent: task.deliveryIntent ?? "leave_worktree",
          execution_owner: { kind: daemonOwner.kind, instance_id: daemonOwner.instanceId, lease_epoch: daemonOwner.leaseEpoch, lease_expires_at_ms: daemonOwner.leaseExpiresAtMs },
          previous_execution_owner: { kind: task.executionOwner.kind, instance_id: task.executionOwner.instanceId, lease_epoch: task.executionOwner.leaseEpoch, lease_expires_at_ms: task.executionOwner.leaseExpiresAtMs },
          task_snapshot: { ...structuredClone(task), executionOwner: daemonOwner, updatedAtMs: Date.now() },
          completed_nodes: task.plan?.nodes.filter((node) => node.status === "succeeded").map((node) => node.nodeId) ?? [],
          next_node_id: task.plan?.nodes.find((node) => ["running", "waiting_approval", "ready", "pending", "waiting_user"].includes(node.status))?.nodeId ?? "plan",
        },
      },
    },
  });
}

async function finishAutonomousRun(recorder: Awaited<ReturnType<typeof attachDurableRun>>, result: AutonomousTask): Promise<void> {
  if (result.outcome === "SUCCEEDED") await recorder?.complete(result.summary);
  else if (result.outcome === "CANCELLED") await recorder?.cancel(result.summary);
  else if (result.outcome === "WAITING_APPROVAL") await recorder?.awaitApproval(result.waitingReason ?? result.summary ?? "Task delivery requires approval.");
  else await recorder?.fail(new Error(`${result.outcome}: ${result.summary ?? "Autonomous task did not complete."}`), result.outcome === "WAITING_USER");
}

export async function startAutonomousTask(params: StartAutonomousTaskParams): Promise<StartedAutonomousTask> {
  const resolvedTarget = await resolveTarget();
  const targetSnapshot = snapshotForResolvedTarget(resolvedTarget);
  if (!targetSnapshot) throw new Error("The selected execution target is unavailable.");
  const roots = useWorkspaceStore.getState().roots;
  const primary = primaryRoot(roots);
  if (!primary) throw new Error("Open a workspace before starting an autonomous task.");
  const permission = usePermissionStore.getState();
  const runBudgets = defaultRunBudgets(false);
  const task = createAutonomousTask({ objective: params.objective, sessionId: params.sessionId, targetSnapshot, workspaceRoots: structuredClone(roots), permissionSnapshot: { mode: permission.mode, unattended: true, allowNetwork: false, allowExternalMutations: false }, constraints: params.constraints, planningContext: params.planningContext, deliveryIntent: params.deliveryIntent, deliveryTarget: params.deliveryTarget, budgetSnapshot: { wallTimeMs: runBudgets.wall_time_ms, maxModelCalls: runBudgets.max_model_calls, maxToolCalls: runBudgets.max_tool_calls, maxRepairRounds: 2, maxWorkers: 16, maxConcurrentWorkers: 4, maxArtifactBytes: runBudgets.max_artifact_bytes, maxCostMicros: runBudgets.max_cost_micros } });
  const ownerFence = async (owner = task.executionOwner): Promise<void> => {
    await invoke("autonomous_task_owner_fence", { request: { taskId: task.taskId, owner: { kind: owner.kind, instance_id: owner.instanceId, lease_epoch: owner.leaseEpoch, lease_expires_at_ms: owner.leaseExpiresAtMs } } });
  };
  await ownerFence();
  const control = new AutonomousTaskControl();
  const signal = mergeSignals(control.signal, params.signal);
  const recorder = await beginDurableRun({ runId: task.taskId, kind: "autonomous_task", task: task.objective, instructions: "Universal autonomous task coordinator", target: targetSnapshot, roots, permissionMode: permission.mode, allowNetwork: false, allowExternalMutations: false, budgets: runBudgets, workspaceAccess: "read_write" });
  const eventSink: AutonomousTaskEventSink = async (event) => { if (recorder) await appendRunEvent(task.taskId, taskEventToRunEvent(event)); };
  const completion = runAutonomousTask({ task, resolvedTarget, runtime: params.runtime ?? defaultAutonomousTaskRuntime(resolvedTarget, defaultAutonomousTaskPlacementAdapters()), signal, eventSink, onUpdate: params.onUpdate, control, ownerFence }).then(async (result) => {
    await finishAutonomousRun(recorder, result); return result;
  });
  void completion.catch(async (error) => { await recorder?.fail(error); });
  return { task, runId: task.taskId, control, completion };
}

export interface ResumeAutonomousTaskParams { task: AutonomousTask; onUpdate?: (task: AutonomousTask) => void; runtime?: AutonomousTaskRuntime; signal?: AbortSignal; approval?: { requestId: string; confirmation: string; operationDigest?: string }; }

export async function resumeAutonomousTask(params: ResumeAutonomousTaskParams): Promise<StartedAutonomousTask> {
  const resolvedTarget = await resolveTarget(); const control = new AutonomousTaskControl(); const signal = mergeSignals(control.signal, params.signal);
  const task = { ...params.task, outcome: "RUNNING" as const, repairRounds: ["FAILED", "VERIFICATION_FAILED", "DELIVERY_FAILED", "WAITING_USER", "EXECUTION_TARGET_LOST"].includes(params.task.outcome) ? 0 : params.task.repairRounds, plan: params.task.plan ? { ...params.task.plan, nodes: params.task.plan.nodes.map((node) => node.status === "running" || node.status === "failed" || node.status === "blocked" || (params.task.waitingApproval && node.nodeId === params.task.waitingApproval.nodeId) ? { ...node, status: "pending" as const, workerId: null } : node) } : null, waitingReason: null, waitingApproval: null, updatedAtMs: Date.now() };
  if (task.executionOwner.kind !== "desktop") throw new Error("Only the current desktop owner may resume an autonomous task.");
  const ownerFence = async (owner = task.executionOwner): Promise<void> => {
    await invoke("autonomous_task_owner_fence", { request: { taskId: task.taskId, owner: { kind: owner.kind, instance_id: owner.instanceId, lease_epoch: owner.leaseEpoch, lease_expires_at_ms: owner.leaseExpiresAtMs } } });
  };
  await ownerFence();
  const recorder = await attachDurableRun({ runId: task.taskId, roots: task.workspaceRoots });
  const eventSink: AutonomousTaskEventSink = async (event) => { if (recorder) await appendRunEvent(task.taskId, taskEventToRunEvent(event)); };
  const completion = runAutonomousTask({ task, resolvedTarget, runtime: params.runtime ?? defaultAutonomousTaskRuntime(resolvedTarget, defaultAutonomousTaskPlacementAdapters()), signal, eventSink, onUpdate: params.onUpdate, control, approval: params.approval, ownerFence }).then(async (result) => { await finishAutonomousRun(recorder, result); return result; });
  void completion.catch(async (error) => { await recorder?.fail(error); });
  return { task, runId: task.taskId, control, completion };
}
