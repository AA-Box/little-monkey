import { invoke } from "@tauri-apps/api/core";

import { agentWorktreeClient } from "./agentWorktree";
import { resolveTarget, snapshotForResolvedTarget } from "./targetRouting";
import type { ResolvedTarget } from "./turnEngine";
import { attachDurableRun, beginDurableRun, defaultRunBudgets } from "./durableRun";
import { runSubagentTask, runSubagentTaskStructured } from "./subagent";
import {
  buildWorkerContext, canRunTaskNodesTogether, createAutonomousTask, createTaskPlan, getReadyTaskPlanNodes,
  hasAuthoritativeAcceptanceEvidence, installTaskPlan, taskEvent, taskEventToRunEvent,
  type AutonomousTask, type CreateAutonomousTaskInput, type TaskArtifact, type TaskEvent,
  type TaskExecutionPlacement, type TaskPlan, type TaskPlanNode, type TaskPlanningContext, type TaskUsage, type TaskWorker, type VerificationEvidence,
} from "./autonomousTask";
import { appendRunEvent } from "./runProtocol";
import { usePermissionStore } from "../store/permissionStore";
import { primaryRoot, useWorkspaceStore } from "../store/workspaceStore";
import { effortForTarget } from "../store/modelStore";

export interface TaskNodeResult {
  ok: boolean;
  summary: string;
  worktreePath?: string;
  artifacts?: TaskArtifact[];
  evidence?: VerificationEvidence[];
  changedFiles?: string[];
  worktree?: { id: string; path: string; branch: string; baseRevision: string; diffDigest: string };
  workspaceRevision?: string;
  usage?: Partial<TaskUsage>;
  awaitingApproval?: boolean;
  review?: StructuredReviewResult;
}

export interface StructuredReviewResult { verdict: "pass" | "changes_required"; findings: Array<{ severity: "blocking" | "warning" | "suggestion"; path: string; title: string; body: string }>; filesReviewed: string[]; acceptanceCriteria: string[]; securityFindings: string[]; testCoverageFindings: string[]; }
export interface TaskPlanResult { plan: TaskPlan; acceptanceCriteria?: AutonomousTask["acceptanceCriteria"]; planningContext?: Partial<TaskPlanningContext>; summary?: string; }

export interface AutonomousTaskRuntimeContext {
  resolvedTarget: ResolvedTarget;
  signal: AbortSignal;
  worker: TaskWorker;
  placement?: TaskExecutionPlacement;
  planningContext?: TaskPlanningContext;
}

export interface AutonomousTaskRuntime {
  plan?: (task: AutonomousTask, context: AutonomousTaskRuntimeContext) => Promise<TaskPlanResult>;
  executeNode: (task: AutonomousTask, node: TaskPlanNode, context: AutonomousTaskRuntimeContext) => Promise<TaskNodeResult>;
  integrate?: (task: AutonomousTask, node: TaskPlanNode, results: TaskNodeResult[], context: AutonomousTaskRuntimeContext) => Promise<TaskNodeResult>;
  verify?: (task: AutonomousTask, node: TaskPlanNode, context: AutonomousTaskRuntimeContext) => Promise<TaskNodeResult>;
  review?: (task: AutonomousTask, node: TaskPlanNode, context: AutonomousTaskRuntimeContext) => Promise<TaskNodeResult>;
  deliver?: (task: AutonomousTask, context: AutonomousTaskRuntimeContext) => Promise<TaskNodeResult>;
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
}

export class AutonomousTaskControl {
  private paused = false;
  private wake: (() => void) | null = null;
  private readonly controller = new AbortController();
  private guidance: AutonomousTask["guidance"] = [];

  get signal(): AbortSignal { return this.controller.signal; }
  get isPaused(): boolean { return this.paused; }
  pause(): void { if (!this.controller.signal.aborted) this.paused = true; }
  resume(): void { this.paused = false; this.wake?.(); this.wake = null; }
  cancel(): void { this.controller.abort(); this.resume(); }
  guide(guidance: AutonomousTask["guidance"][number]): void { this.guidance.push(structuredClone(guidance)); this.guidance = this.guidance.slice(-8); this.wake?.(); this.wake = null; }
  drainGuidance(): AutonomousTask["guidance"] { const next = this.guidance; this.guidance = []; return next; }
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

function replaceDependency(plan: TaskPlan, oldId: string, newId: string): TaskPlan { return { ...plan, revision: plan.revision + 1, nodes: plan.nodes.map((node) => ({ ...node, dependencies: node.dependencies.map((dependency) => dependency === oldId ? newId : dependency) })) }; }
function insertRepairNode(task: AutonomousTask, failedNode: TaskPlanNode, summary: string): AutonomousTask {
  if (!task.plan) return task;
  const repairId = `${failedNode.nodeId}-repair-${task.repairRounds}`;
  const repairNode: TaskPlanNode = { ...failedNode, nodeId: repairId, taskClass: "implementation", objective: `Diagnose and repair ${failedNode.nodeId} using bounded failure evidence: ${summary.slice(0, 2_000)}`, dependencies: failedNode.dependencies, status: failedNode.dependencies.length ? "pending" : "ready", attempt: 0, workerId: null, resultSummary: null, repairOf: failedNode.nodeId };
  return { ...task, plan: replaceDependency({ ...task.plan, nodes: [...task.plan.nodes, repairNode] }, failedNode.nodeId, repairId), updatedAtMs: Date.now() };
}

function parseStructuredReview(value: string): StructuredReviewResult | null {
  const candidate = value.match(/\{[\s\S]*\}/)?.[0];
  if (!candidate) return null;
  try { const parsed = JSON.parse(candidate) as Partial<StructuredReviewResult>; if (parsed.verdict !== "pass" && parsed.verdict !== "changes_required") return null; if (!Array.isArray(parsed.findings) || !Array.isArray(parsed.filesReviewed) || !Array.isArray(parsed.acceptanceCriteria) || !Array.isArray(parsed.securityFindings) || !Array.isArray(parsed.testCoverageFindings)) return null; return parsed as StructuredReviewResult; } catch { return null; }
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
      const planned = await params.runtime.plan?.(task, planningContext);
      const plan = planned?.plan ?? createTaskPlan(task.objective, task.constraints.strategy ?? "PLAN", Math.min(task.budgetSnapshot.maxWorkers, task.budgetSnapshot.maxConcurrentWorkers ?? task.budgetSnapshot.maxWorkers), task.planningContext);
      if (planned?.acceptanceCriteria) task = { ...task, acceptanceCriteria: structuredClone(planned.acceptanceCriteria) };
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
    for (const worker of task.workers) if (worker.worktree) results.set(worker.nodeId, { ok: true, summary: "Recovered worker result from the durable task snapshot.", worktree: worker.worktree, changedFiles: worker.changedFiles });
    while (task.outcome === "RUNNING" && task.plan && task.plan.nodes.some((node) => node.status === "pending" || node.status === "ready" || node.status === "running")) {
      stopIfNeeded();
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
        const context: AutonomousTaskRuntimeContext = { resolvedTarget: params.resolvedTarget, signal, worker, placement: worker.executionPlacement, planningContext: task.planningContext };
        if (node.taskClass === "integration" && params.runtime.integrate) return params.runtime.integrate(task, node, [...results.values()], context);
        if (node.taskClass === "verification" && params.runtime.verify) return params.runtime.verify(task, node, context);
        if (node.taskClass === "review" && params.runtime.review) return params.runtime.review(task, node, context);
        if (node.taskClass === "delivery" && params.runtime.deliver) return params.runtime.deliver(task, context);
        return params.runtime.executeNode(task, node, context);
      });
      const completed = await Promise.all(executions);
      for (let index = 0; index < completed.length; index += 1) {
        stopIfNeeded();
        const worker = workers[index];
        const node = task.plan?.nodes.find((candidate) => candidate.nodeId === worker.nodeId);
        if (!node) throw new Error(`Task node ${worker.nodeId} disappeared from the plan.`);
        const result = completed[index];
        results.set(node.nodeId, result);
        const status: TaskPlanNode["status"] = result.ok ? "succeeded" : "failed";
        task = updateNode(task, node.nodeId, { status, resultSummary: result.summary.slice(0, 2_000) });
        task = addUsage(task, result.usage ?? { modelCalls: 1 });
        if (task.usage && ((task.usage.modelCalls > task.budgetSnapshot.maxModelCalls) || (task.usage.toolCalls > task.budgetSnapshot.maxToolCalls) || (task.budgetSnapshot.maxCostMicros !== null && task.usage.costMicros > (task.budgetSnapshot.maxCostMicros ?? Number.MAX_SAFE_INTEGER)))) { task = { ...task, outcome: "BUDGET_EXHAUSTED", summary: "Model, tool, or cost budget exhausted.", updatedAtMs: Date.now() }; }
        task = advanceWorkspaceRevision(task, result.workspaceRevision);
        task = addUsage(task, { artifactBytes: (result.artifacts ?? []).reduce((total, artifact) => total + artifact.label.length + (artifact.path?.length ?? 0), 0) });
        if ((task.budgetSnapshot.maxArtifactBytes ?? Number.MAX_SAFE_INTEGER) < (task.usage?.artifactBytes ?? 0)) task = { ...task, outcome: "BUDGET_EXHAUSTED", summary: "Artifact budget exhausted.", updatedAtMs: Date.now() };
        task = { ...task, artifacts: [...task.artifacts, ...(result.artifacts ?? [])], updatedAtMs: Date.now() };
        task = { ...task, workers: task.workers.map((entry) => entry.workerId === worker.workerId ? { ...entry, finishedAtMs: Date.now() } : entry), updatedAtMs: Date.now() };
        task = { ...task, workers: task.workers.map((entry) => entry.workerId === worker.workerId ? { ...entry, worktree: result.worktree, changedFiles: result.changedFiles, usage: result.usage } : entry), updatedAtMs: Date.now() };
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
      const failed = completed.some((result) => !result.ok);
      if (failed) {
        if (task.repairRounds < task.budgetSnapshot.maxRepairRounds) {
          task = { ...task, repairRounds: task.repairRounds + 1, updatedAtMs: Date.now() };
          for (const node of batch) { const failedNode = task.plan?.nodes.find((candidate) => candidate.nodeId === node.nodeId); const failedResult = completed.find((_result, resultIndex) => workers[resultIndex]?.nodeId === node.nodeId); if (failedNode?.status === "failed" && failedResult && !failedResult.ok) task = insertRepairNode(task, failedNode, failedResult.summary); }
          await emit("plan_changed", { reason: "Bounded repair round requested after worker failure.", repair_round: task.repairRounds });
        } else {
          task = { ...task, outcome: task.plan?.nodes.some((node) => node.status === "succeeded") ? "PARTIALLY_COMPLETED" : "FAILED", summary: "One or more task nodes failed after bounded repair rounds.", updatedAtMs: Date.now() };
          break;
        }
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
    task = { ...task, outcome: signal.aborted ? "CANCELLED" : "FAILED", summary: error instanceof Error ? error.message : String(error), updatedAtMs: Date.now() };
  }
  await emit("task_completed", { outcome: task.outcome, summary: task.summary });
  return task;
}

function defaultRuntime(resolvedTarget: ResolvedTarget): AutonomousTaskRuntime {
  return {
    plan: async (task, context) => {
      const result = await runSubagentTask({ sessionId: task.sessionId ?? task.taskId, runId: task.taskId, parentCheckpointId: null, taskId: `${task.taskId}-planner`, description: "Plan autonomous task", prompt: `Inspect the repository and return JSON only. Build a context-aware DAG with real file scopes; never invent numbered scopes. Schema: {plan:{planId,strategy,nodes:[...]},acceptanceCriteria:[...],summary}\n${buildWorkerContext(task, { nodeId: "planner", taskClass: "investigation", objective: task.objective, dependencies: [], mutationScope: [], isolation: "shared", status: "running", attempt: 1, workerId: null, resultSummary: null })}`, profile: "explore", target: resolvedTarget, effort: effortForTarget(resolvedTarget), parentSignal: context.signal });
      const candidate = result.match(/\{[\s\S]*\}/)?.[0];
      if (candidate) { try { const parsed = JSON.parse(candidate) as Partial<TaskPlanResult>; if (parsed.plan && Array.isArray(parsed.plan.nodes)) return { plan: parsed.plan as TaskPlan, acceptanceCriteria: Array.isArray(parsed.acceptanceCriteria) ? parsed.acceptanceCriteria as AutonomousTask["acceptanceCriteria"] : undefined, summary: parsed.summary }; } catch { /* deterministic fallback below */ } }
      return { plan: createTaskPlan(task.objective, task.constraints.strategy ?? "PLAN", task.budgetSnapshot.maxWorkers, task.planningContext), summary: "Model plan was invalid; used repository-context fallback." };
    },
    executeNode: async (task, node, context) => {
      if (node.executionPlacement && node.executionPlacement.kind !== "local" && node.executionPlacement.kind !== "worktree") return { ok: false, summary: `Execution placement ${node.executionPlacement.kind} is unavailable in this runtime.` };
      const result = await runSubagentTaskStructured({ sessionId: task.sessionId ?? task.taskId, runId: task.taskId, parentCheckpointId: null, taskId: `${task.taskId}-${node.nodeId}`, description: node.objective.slice(0, 120), prompt: buildWorkerContext(task, node), profile: node.taskClass === "investigation" || node.taskClass === "review" ? "explore" : "code", isolation: node.isolation === "worktree" ? "worktree" : undefined, target: resolvedTarget, effort: effortForTarget(resolvedTarget), parentSignal: context.signal });
      return { ok: result.outcome === "done", summary: result.report.slice(0, 8_000), worktree: result.worktree, changedFiles: result.changedFiles, artifacts: result.worktree ? [{ artifactId: `patch-${node.nodeId}-${Date.now()}`, kind: "patch", label: `Patch from ${node.nodeId}`, path: result.worktree.path, digest: result.worktree.diffDigest, createdAtMs: Date.now(), workspaceRevision: result.worktree.baseRevision }] : undefined, usage: result.usage };
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
        try { const response = await agentWorktreeClient.apply(path); applied.push(...response.applied_files); }
        catch (error) { return { ok: false, summary: `Integration conflict: ${error instanceof Error ? error.message : String(error)}` }; }
      }
      return { ok: true, summary: `Integrated ${paths.length} worker patch(es) after scope inspection.`, changedFiles: applied, workspaceRevision: await agentWorktreeClient.workspaceRevision().catch(() => undefined) };
    },
    verify: async (task) => {
      const config = await invoke<{ commands: Array<{ id: string; label: string; enabled: boolean }> }>("verify_get_config", {}).catch(() => ({ commands: [] }));
      const enabled = config.commands.filter((command) => command.enabled);
      if (enabled.length === 0) return { ok: true, summary: "No configured verification commands; acceptance requires user confirmation." };
      const evidence: VerificationEvidence[] = [];
      for (const command of enabled) {
        const started = Date.now();
        const result = await invoke<{ code: number | null; timedOut: boolean; stdout?: string; stderr?: string }>("verify_run", { commandId: command.id, turnId: task.taskId }).catch((error) => ({ code: null, timedOut: false, stdout: "", stderr: error instanceof Error ? error.message : String(error) }));
        const passed = !result.timedOut && result.code === 0;
        const criterion = task.acceptanceCriteria.find((candidate) => candidate.method === "verification_command");
        evidence.push(makeEvidence(command.label || command.id, criterion?.id ?? null, passed, `${result.stdout ?? ""}\n${result.stderr ?? ""}`.trim() || (passed ? "passed" : "failed"), true, result.code, Date.now() - started, { command: command.id, commandDigest: await textDigest(command.id), workspaceRevision: task.workspaceRevision, testedRevision: task.workspaceRevision, source: "command" }));
        if (!passed) return { ok: false, summary: evidence[evidence.length - 1].summary, evidence };
      }
      return { ok: true, summary: `${enabled.length} verification command(s) passed.`, evidence };
    },
    review: async (task, node, context) => {
      const result = await runSubagentTask({ sessionId: task.sessionId ?? task.taskId, runId: task.taskId, parentCheckpointId: null, taskId: `${task.taskId}-${node.nodeId}`, description: "Review task diff", prompt: `${buildWorkerContext(task, node)}\nReview only. Do not mutate files. Return strict JSON with verdict, findings, filesReviewed, acceptanceCriteria, securityFindings, testCoverageFindings.`, profile: "explore", target: resolvedTarget, effort: effortForTarget(resolvedTarget), parentSignal: context.signal });
      const review = parseStructuredReview(result); const criteria = task.acceptanceCriteria.filter((candidate) => candidate.method === "review");
      if (!review) return { ok: false, summary: "Review response was not valid structured JSON; no review evidence accepted.", evidence: criteria.map((criterion) => makeEvidence("Structured worker diff review", criterion.id, false, result, false, null, 0, { workspaceRevision: task.workspaceRevision, testedRevision: task.workspaceRevision, source: "review" })) };
      const passed = review.verdict === "pass" && !review.findings.some((finding) => finding.severity === "blocking");
      return { ok: passed, summary: JSON.stringify(review).slice(0, 8_000), review, evidence: criteria.map((criterion) => makeEvidence("Structured worker diff review", criterion.id, passed, JSON.stringify(review), true, passed ? 0 : 1, 0, { workspaceRevision: task.workspaceRevision, testedRevision: task.workspaceRevision, source: "review" })) };
    },
    deliver: async (task) => task.deliveryIntent === "leave_worktree" ? { ok: true, summary: "Changes left in the managed worktree." } : { ok: false, awaitingApproval: true, summary: `Delivery intent ${task.deliveryIntent} requires explicit Git delivery approval.` },
  };
}

export interface StartAutonomousTaskParams {
  objective: string;
  sessionId?: string | null;
  constraints?: CreateAutonomousTaskInput["constraints"];
  planningContext?: CreateAutonomousTaskInput["planningContext"];
  deliveryIntent?: CreateAutonomousTaskInput["deliveryIntent"];
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
  const task = createAutonomousTask({ objective: params.objective, sessionId: params.sessionId, targetSnapshot, workspaceRoots: structuredClone(roots), permissionSnapshot: { mode: permission.mode, unattended: true, allowNetwork: false, allowExternalMutations: false }, constraints: params.constraints, planningContext: params.planningContext, deliveryIntent: params.deliveryIntent, budgetSnapshot: { wallTimeMs: runBudgets.wall_time_ms, maxModelCalls: runBudgets.max_model_calls, maxToolCalls: runBudgets.max_tool_calls, maxRepairRounds: 2, maxWorkers: 16, maxConcurrentWorkers: 4, maxArtifactBytes: runBudgets.max_artifact_bytes, maxCostMicros: runBudgets.max_cost_micros } });
  const control = new AutonomousTaskControl();
  const signal = mergeSignals(control.signal, params.signal);
  const recorder = await beginDurableRun({ runId: task.taskId, kind: "autonomous_task", task: task.objective, instructions: "Universal autonomous task coordinator", target: targetSnapshot, roots, permissionMode: permission.mode, allowNetwork: false, allowExternalMutations: false, budgets: runBudgets, workspaceAccess: "read_write" });
  const eventSink: AutonomousTaskEventSink = async (event) => { if (recorder) await appendRunEvent(task.taskId, taskEventToRunEvent(event)); };
  const completion = runAutonomousTask({ task, resolvedTarget, runtime: params.runtime ?? defaultRuntime(resolvedTarget), signal, eventSink, onUpdate: params.onUpdate, control }).then(async (result) => {
    await finishAutonomousRun(recorder, result); return result;
  });
  void completion.catch(async (error) => { await recorder?.fail(error); });
  return { task, runId: task.taskId, control, completion };
}

export interface ResumeAutonomousTaskParams { task: AutonomousTask; onUpdate?: (task: AutonomousTask) => void; runtime?: AutonomousTaskRuntime; signal?: AbortSignal; }

export async function resumeAutonomousTask(params: ResumeAutonomousTaskParams): Promise<StartedAutonomousTask> {
  const resolvedTarget = await resolveTarget(); const control = new AutonomousTaskControl(); const signal = mergeSignals(control.signal, params.signal);
  const task = { ...params.task, outcome: "RUNNING" as const, plan: params.task.plan ? { ...params.task.plan, nodes: params.task.plan.nodes.map((node) => node.status === "running" ? { ...node, status: "pending" as const, workerId: null } : node) } : null, updatedAtMs: Date.now() };
  const recorder = await attachDurableRun({ runId: task.taskId, roots: task.workspaceRoots });
  const eventSink: AutonomousTaskEventSink = async (event) => { if (recorder) await appendRunEvent(task.taskId, taskEventToRunEvent(event)); };
  const completion = runAutonomousTask({ task, resolvedTarget, runtime: params.runtime ?? defaultRuntime(resolvedTarget), signal, eventSink, onUpdate: params.onUpdate, control }).then(async (result) => { await finishAutonomousRun(recorder, result); return result; });
  void completion.catch(async (error) => { await recorder?.fail(error); });
  return { task, runId: task.taskId, control, completion };
}
