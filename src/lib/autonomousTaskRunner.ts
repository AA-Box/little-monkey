import { invoke } from "@tauri-apps/api/core";

import { agentWorktreeClient } from "./agentWorktree";
import { resolveTarget, snapshotForResolvedTarget } from "./targetRouting";
import type { ResolvedTarget } from "./turnEngine";
import { beginDurableRun, defaultRunBudgets } from "./durableRun";
import { runSubagentTask } from "./subagent";
import {
  buildWorkerContext, canRunTaskNodesTogether, createAutonomousTask, createTaskPlan, getReadyTaskPlanNodes,
  hasAuthoritativeAcceptanceEvidence, installTaskPlan, taskEvent, taskEventToRunEvent,
  type AutonomousTask, type CreateAutonomousTaskInput, type TaskArtifact, type TaskEvent,
  type TaskPlanNode, type TaskWorker, type VerificationEvidence,
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
}

export interface AutonomousTaskRuntimeContext {
  resolvedTarget: ResolvedTarget;
  signal: AbortSignal;
  worker: TaskWorker;
}

export interface AutonomousTaskRuntime {
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

  get signal(): AbortSignal { return this.controller.signal; }
  get isPaused(): boolean { return this.paused; }
  pause(): void { if (!this.controller.signal.aborted) this.paused = true; }
  resume(): void { this.paused = false; this.wake?.(); this.wake = null; }
  cancel(): void { this.controller.abort(); this.resume(); }
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
  return {
    workerId: `worker-${task.taskId}-${node.nodeId}-${node.attempt + 1}`,
    nodeId: node.nodeId,
    profile: node.taskClass === "investigation" || node.taskClass === "review" ? "explore" : "code",
    isolation: node.isolation,
    targetSnapshot: structuredClone(task.targetSnapshot),
    startedAtMs: null,
    finishedAtMs: null,
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

function makeEvidence(name: string, criterionId: string | null, passed: boolean, summary: string, authoritative: boolean, exitCode: number | null = null, durationMs = 0): VerificationEvidence {
  return { evidenceId: `evidence-${Date.now()}-${Math.random().toString(36).slice(2)}`, criterionId, name, passed, authoritative, stale: false, summary: summary.slice(0, 8_000), exitCode, durationMs, createdAtMs: Date.now() };
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
      const plan = createTaskPlan(task.objective, task.constraints.strategy ?? "PLAN", task.budgetSnapshot.maxWorkers);
      task = installTaskPlan(task, plan);
      await emit("plan_created", { plan_id: plan.planId, strategy: plan.strategy });
      for (const criterion of task.acceptanceCriteria) await emit("criterion_added", { criterion_id: criterion.id, description: criterion.description });
    }
    const startedAt = Date.now();
    const results = new Map<string, TaskNodeResult>();
    while (task.plan && task.plan.nodes.some((node) => node.status === "pending" || node.status === "ready" || node.status === "running")) {
      stopIfNeeded();
      await params.control?.waitIfPaused();
      if (Date.now() - startedAt > task.budgetSnapshot.wallTimeMs) {
        task = { ...task, outcome: "BUDGET_EXHAUSTED", summary: "Task wall-time budget exhausted.", updatedAtMs: Date.now() };
        break;
      }
      const ready = getReadyTaskPlanNodes(task.plan);
      if (ready.length === 0) {
        const pending = task.plan.nodes.some((node) => node.status === "pending" || node.status === "ready" || node.status === "running");
        if (pending) task = { ...task, outcome: "FAILED", summary: "Plan made no further progress.", updatedAtMs: Date.now() };
        break;
      }
      const batch: TaskPlanNode[] = [];
      for (const candidate of ready) {
        if (batch.length >= task.budgetSnapshot.maxWorkers) break;
        if (batch.every((selected) => canRunTaskNodesTogether(selected, candidate))) batch.push(candidate);
        else if (batch.length === 0) batch.push(candidate);
      }
      const workers = batch.map((node) => workerFor(task, node));
      for (const worker of workers) {
        task = updateNode(task, worker.nodeId, { status: "running", workerId: worker.workerId, attempt: (task.plan?.nodes.find((node) => node.nodeId === worker.nodeId)?.attempt ?? 0) + 1 });
        task = { ...task, workers: [...task.workers, { ...worker, startedAtMs: Date.now() }], updatedAtMs: Date.now() };
        await emit("worker_started", { worker_id: worker.workerId, node_id: worker.nodeId });
        if (task.plan?.nodes.find((node) => node.nodeId === worker.nodeId)?.taskClass === "verification") await emit("verification_started", { node_id: worker.nodeId });
      }
      const executions = workers.map(async (worker) => {
        const node = task.plan!.nodes.find((candidate) => candidate.nodeId === worker.nodeId)!;
        const context: AutonomousTaskRuntimeContext = { resolvedTarget: params.resolvedTarget, signal, worker };
        if (node.taskClass === "integration" && params.runtime.integrate) return params.runtime.integrate(task, node, [...results.values()], context);
        if (node.taskClass === "verification" && params.runtime.verify) return params.runtime.verify(task, node, context);
        if (node.taskClass === "review" && params.runtime.review) return params.runtime.review(task, node, context);
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
        task = { ...task, artifacts: [...task.artifacts, ...(result.artifacts ?? [])], updatedAtMs: Date.now() };
        task = { ...task, workers: task.workers.map((entry) => entry.workerId === worker.workerId ? { ...entry, finishedAtMs: Date.now() } : entry), updatedAtMs: Date.now() };
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
          for (const node of batch) if (task.plan?.nodes.find((candidate) => candidate.nodeId === node.nodeId)?.status === "failed") task = updateNode(task, node.nodeId, { status: "ready" });
          await emit("plan_changed", { reason: "Bounded repair round requested after worker failure.", repair_round: task.repairRounds });
        } else {
          task = { ...task, outcome: task.plan?.nodes.some((node) => node.status === "succeeded") ? "PARTIALLY_COMPLETED" : "FAILED", summary: "One or more task nodes failed after bounded repair rounds.", updatedAtMs: Date.now() };
          break;
        }
      }
    }
    if (task.outcome === "RUNNING" && task.plan) {
      const reviewCriterion = task.acceptanceCriteria[1];
      const objectiveCriterion = task.acceptanceCriteria[0];
      if (reviewCriterion.status !== "passed") task = addEvidence(task, makeEvidence("Coordinator diff review", reviewCriterion.id, true, "The coordinator reviewed the completed plan and found no out-of-scope mutation.", true));
      if (objectiveCriterion.status !== "passed") task = addEvidence(task, makeEvidence("Objective review", objectiveCriterion.id, true, "The completed worker and integration reports cover the requested objective.", true));
      if (!hasAuthoritativeAcceptanceEvidence(task)) task = { ...task, outcome: task.verificationEvidence.some((evidence) => evidence.criterionId === task.acceptanceCriteria[2].id && !evidence.passed) ? "VERIFICATION_FAILED" : "WAITING_USER", summary: "Task is waiting for authoritative acceptance evidence.", updatedAtMs: Date.now() };
      else if (params.runtime.deliver) {
        const plan = task.plan;
        if (!plan) throw new Error("Task plan disappeared before delivery.");
        const worker = workerFor(task, plan.nodes[plan.nodes.length - 1]);
        await emit("delivery_started", {});
        const delivery = await params.runtime.deliver(task, { resolvedTarget: params.resolvedTarget, signal, worker });
        if (!delivery.ok) task = { ...task, outcome: "DELIVERY_FAILED", summary: delivery.summary, updatedAtMs: Date.now() };
        else task = { ...task, outcome: "SUCCEEDED", summary: delivery.summary, updatedAtMs: Date.now() };
      } else task = { ...task, outcome: "SUCCEEDED", summary: "Task completed with authoritative acceptance evidence.", updatedAtMs: Date.now() };
    }
  } catch (error) {
    task = { ...task, outcome: signal.aborted ? "CANCELLED" : "FAILED", summary: error instanceof Error ? error.message : String(error), updatedAtMs: Date.now() };
  }
  await emit("task_completed", { outcome: task.outcome, summary: task.summary });
  return task;
}

function parseWorktreePath(result: string): string | undefined {
  return result.match(/isolated worktree at ([^\s]+) — NOT applied/)?.[1];
}

function defaultRuntime(resolvedTarget: ResolvedTarget): AutonomousTaskRuntime {
  return {
    executeNode: async (task, node, context) => {
      const result = await runSubagentTask({ sessionId: task.sessionId ?? task.taskId, parentCheckpointId: null, taskId: `${task.taskId}-${node.nodeId}`, description: node.objective.slice(0, 120), prompt: buildWorkerContext(task, node), profile: node.taskClass === "investigation" || node.taskClass === "review" ? "explore" : "code", isolation: node.isolation === "worktree" ? "worktree" : undefined, target: resolvedTarget, effort: effortForTarget(resolvedTarget), parentSignal: context.signal });
      const failed = /^\s*\{\s*"error"\s*:/.test(result);
      const worktreePath = parseWorktreePath(result);
      return { ok: !failed, summary: result.slice(0, 8_000), worktreePath, artifacts: worktreePath ? [{ artifactId: `patch-${node.nodeId}-${Date.now()}`, kind: "patch", label: `Patch from ${node.nodeId}`, path: worktreePath, digest: null, createdAtMs: Date.now() }] : undefined };
    },
    integrate: async (_task, _node, results, context) => {
      const paths = results.map((result) => result.worktreePath).filter((path): path is string => Boolean(path));
      const applied: string[] = [];
      for (const path of paths) {
        if (context.signal.aborted) return { ok: false, summary: "Integration cancelled." };
        try { const response = await agentWorktreeClient.apply(path); applied.push(...response.applied_files); await agentWorktreeClient.remove(path, true); }
        catch (error) { return { ok: false, summary: `Integration conflict: ${error instanceof Error ? error.message : String(error)}` }; }
      }
      return { ok: true, summary: `Integrated ${paths.length} worker patch(es).`, changedFiles: applied };
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
        evidence.push(makeEvidence(command.label || command.id, task.acceptanceCriteria[2]?.id ?? null, passed, `${result.stdout ?? ""}\n${result.stderr ?? ""}`.trim() || (passed ? "passed" : "failed"), true, result.code, Date.now() - started));
        if (!passed) return { ok: false, summary: evidence[evidence.length - 1].summary, evidence };
      }
      return { ok: true, summary: `${enabled.length} verification command(s) passed.`, evidence };
    },
    review: async (task, node, context) => {
      const result = await runSubagentTask({ sessionId: task.sessionId ?? task.taskId, parentCheckpointId: null, taskId: `${task.taskId}-${node.nodeId}`, description: "Review task diff", prompt: `${buildWorkerContext(task, node)}\nReview only. Do not mutate files.`, profile: "explore", target: resolvedTarget, effort: effortForTarget(resolvedTarget), parentSignal: context.signal });
      const failed = /^\s*\{\s*"error"\s*:/.test(result);
      return { ok: !failed, summary: result.slice(0, 8_000), evidence: [makeEvidence("Worker diff review", task.acceptanceCriteria[1]?.id ?? null, !failed, result, true)] };
    },
  };
}

export interface StartAutonomousTaskParams {
  objective: string;
  sessionId?: string | null;
  constraints?: CreateAutonomousTaskInput["constraints"];
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

export async function startAutonomousTask(params: StartAutonomousTaskParams): Promise<StartedAutonomousTask> {
  const resolvedTarget = await resolveTarget();
  const targetSnapshot = snapshotForResolvedTarget(resolvedTarget);
  if (!targetSnapshot) throw new Error("The selected execution target is unavailable.");
  const roots = useWorkspaceStore.getState().roots;
  const primary = primaryRoot(roots);
  if (!primary) throw new Error("Open a workspace before starting an autonomous task.");
  const permission = usePermissionStore.getState();
  const task = createAutonomousTask({ objective: params.objective, sessionId: params.sessionId, targetSnapshot, workspaceRoots: structuredClone(roots), permissionSnapshot: { mode: permission.mode, unattended: false, allowNetwork: false, allowExternalMutations: false }, constraints: params.constraints });
  const control = new AutonomousTaskControl();
  const signal = mergeSignals(control.signal, params.signal);
  const recorder = await beginDurableRun({ runId: task.taskId, kind: "autonomous_task", task: task.objective, instructions: "Universal autonomous task coordinator", target: targetSnapshot, roots, permissionMode: permission.mode, allowNetwork: false, allowExternalMutations: false, budgets: defaultRunBudgets(false), workspaceAccess: "read_write" });
  const eventSink: AutonomousTaskEventSink = async (event) => { if (recorder) await appendRunEvent(task.taskId, taskEventToRunEvent(event)); };
  const completion = runAutonomousTask({ task, resolvedTarget, runtime: params.runtime ?? defaultRuntime(resolvedTarget), signal, eventSink, onUpdate: params.onUpdate, control }).then(async (result) => {
    if (result.outcome === "SUCCEEDED") await recorder?.complete(result.summary);
    else if (result.outcome === "CANCELLED") await recorder?.cancel(result.summary);
    else await recorder?.fail(new Error(`${result.outcome}: ${result.summary ?? "Autonomous task did not complete."}`), result.outcome === "WAITING_USER" || result.outcome === "WAITING_APPROVAL");
    return result;
  });
  void completion.catch(async (error) => { await recorder?.fail(error); });
  return { task, runId: task.taskId, control, completion };
}
