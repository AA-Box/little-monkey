import type { ModelTargetSnapshot } from "./modelTargets";
import type { AutonomousTaskEventType, RunEventWire } from "./runProtocol";
import type { WorkspaceRootInfo } from "../store/workspaceStore";

export const AUTONOMOUS_TASK_SCHEMA_VERSION = 1 as const;
export const MAX_TASK_PLAN_NODES = 64;
export const MAX_TASK_WORKERS = 16;
export const MAX_TASK_GUIDANCE_ITEMS = 32;

export type TaskExecutionStrategy = "DIRECT" | "PLAN" | "DELEGATE" | "PARALLEL_DELEGATE";
export type TaskClass = "investigation" | "implementation" | "integration" | "verification" | "review" | "delivery";
export type TaskNodeStatus = "pending" | "ready" | "running" | "succeeded" | "failed" | "blocked" | "cancelled";
export type TaskIsolation = "shared" | "worktree";
export type TaskOutcome =
  | "RUNNING"
  | "SUCCEEDED"
  | "FAILED"
  | "PARTIALLY_COMPLETED"
  | "WAITING_APPROVAL"
  | "WAITING_USER"
  | "CANCELLED"
  | "BUDGET_EXHAUSTED"
  | "EXECUTION_TARGET_LOST"
  | "VERIFICATION_FAILED"
  | "DELIVERY_FAILED";
export type CriterionStatus = "pending" | "passed" | "failed" | "not_mechanically_verifiable";
export type CriterionMethod = "user" | "plan" | "verification_command" | "review" | "worker_report";

export interface TaskBudgetSnapshot {
  wallTimeMs: number;
  maxModelCalls: number;
  maxToolCalls: number;
  maxRepairRounds: number;
  maxWorkers: number;
}

export interface TaskPermissionSnapshot {
  mode: string;
  unattended: boolean;
  allowNetwork: boolean;
  allowExternalMutations: boolean;
}

export interface TaskConstraints {
  strategy?: TaskExecutionStrategy;
  requiresApproval?: boolean;
  allowParallel?: boolean;
  allowExternalDelivery?: boolean;
  source?: "user" | "issue" | "workflow" | "schedule" | "api";
  untrustedSource?: boolean;
}

export interface AcceptanceCriterion {
  id: string;
  description: string;
  status: CriterionStatus;
  method: CriterionMethod;
  source: string;
  evidenceIds: string[];
  blocking: boolean;
}

export interface TaskPlanNode {
  nodeId: string;
  taskClass: TaskClass;
  objective: string;
  dependencies: string[];
  mutationScope: string[];
  isolation: TaskIsolation;
  status: TaskNodeStatus;
  attempt: number;
  workerId: string | null;
  resultSummary: string | null;
}

export interface TaskPlan {
  planId: string;
  strategy: TaskExecutionStrategy;
  nodes: TaskPlanNode[];
  createdAtMs: number;
  revision: number;
  rationale: string;
}

export interface TaskWorker {
  workerId: string;
  nodeId: string;
  profile: "explore" | "code";
  isolation: TaskIsolation;
  targetSnapshot: ModelTargetSnapshot;
  startedAtMs: number | null;
  finishedAtMs: number | null;
}

export interface TaskArtifact {
  artifactId: string;
  kind: "patch" | "report" | "verification" | "review" | "delivery";
  label: string;
  path: string | null;
  digest: string | null;
  createdAtMs: number;
}

export interface VerificationEvidence {
  evidenceId: string;
  criterionId: string | null;
  name: string;
  passed: boolean;
  authoritative: boolean;
  stale: boolean;
  summary: string;
  exitCode: number | null;
  durationMs: number;
  createdAtMs: number;
}

export interface TaskGuidance {
  guidanceId: string;
  text: string;
  receivedAtMs: number;
  appliesTo: "current_node" | "future_nodes" | "whole_task";
}

export interface AutonomousTask {
  schemaVersion: typeof AUTONOMOUS_TASK_SCHEMA_VERSION;
  taskId: string;
  sessionId: string | null;
  objective: string;
  source: NonNullable<TaskConstraints["source"]>;
  untrustedSource: boolean;
  createdAtMs: number;
  updatedAtMs: number;
  targetSnapshot: ModelTargetSnapshot;
  workspaceRoots: WorkspaceRootInfo[];
  permissionSnapshot: TaskPermissionSnapshot;
  budgetSnapshot: TaskBudgetSnapshot;
  constraints: TaskConstraints;
  plan: TaskPlan | null;
  acceptanceCriteria: AcceptanceCriterion[];
  workers: TaskWorker[];
  artifacts: TaskArtifact[];
  verificationEvidence: VerificationEvidence[];
  guidance: TaskGuidance[];
  outcome: TaskOutcome;
  summary: string | null;
  repairRounds: number;
}

export interface CreateAutonomousTaskInput {
  objective: string;
  sessionId?: string | null;
  targetSnapshot: ModelTargetSnapshot;
  workspaceRoots: WorkspaceRootInfo[];
  permissionSnapshot: TaskPermissionSnapshot;
  budgetSnapshot?: Partial<TaskBudgetSnapshot>;
  constraints?: TaskConstraints;
}

export interface TaskEvent {
  eventId: string;
  taskId: string;
  eventType: AutonomousTaskEventType;
  occurredAtMs: number;
  payload: Record<string, unknown>;
}

function id(prefix: string): string {
  const suffix = typeof crypto !== "undefined" && typeof crypto.randomUUID === "function" ? crypto.randomUUID() : `${Date.now()}-${Math.random().toString(36).slice(2)}`;
  return `${prefix}-${suffix}`;
}

function now(): number { return Date.now(); }

export function chooseTaskExecutionStrategy(input: Pick<CreateAutonomousTaskInput, "objective" | "constraints" | "budgetSnapshot">): TaskExecutionStrategy {
  if (input.constraints?.strategy) return input.constraints.strategy;
  if (input.constraints?.allowParallel && input.budgetSnapshot?.maxWorkers !== 1 && /\b(parallel|independent|several|multiple)\b/i.test(input.objective)) return "PARALLEL_DELEGATE";
  if (/\b(simple|rename|typo|change one|update one)\b/i.test(input.objective)) return "DIRECT";
  if (/\b(investigate|compare|analy[sz]e|research|audit)\b/i.test(input.objective)) return "DELEGATE";
  return "PLAN";
}

export function deriveAcceptanceCriteria(objective: string, source = "coordinator"): AcceptanceCriterion[] {
  const prefix = id("criterion");
  return [
    { id: `${prefix}-objective`, description: `The requested objective is implemented: ${objective}`, status: "pending", method: "worker_report", source, evidenceIds: [], blocking: true },
    { id: `${prefix}-scope`, description: "The change stays within the requested scope and preserves unrelated behavior.", status: "pending", method: "review", source, evidenceIds: [], blocking: true },
    { id: `${prefix}-verification`, description: "Relevant repository verification passes, or a user-visible reason explains why it could not run.", status: "pending", method: "verification_command", source, evidenceIds: [], blocking: true },
  ];
}

function node(nodeId: string, taskClass: TaskClass, objective: string, dependencies: string[], mutationScope: string[], isolation: TaskIsolation): TaskPlanNode {
  return { nodeId, taskClass, objective, dependencies, mutationScope, isolation, status: dependencies.length ? "pending" : "ready", attempt: 0, workerId: null, resultSummary: null };
}

export function createTaskPlan(objective: string, strategy: TaskExecutionStrategy, maxWorkers = 4): TaskPlan {
  const planId = id("plan");
  const implementObjective = `Implement the smallest safe change for: ${objective}`;
  const nodes: TaskPlanNode[] = [];
  if (strategy === "DIRECT") {
    nodes.push(node("implement", "implementation", implementObjective, [], ["workspace"], "shared"));
    nodes.push(node("verify", "verification", "Run the relevant configured verification commands.", ["implement"], ["workspace"], "shared"));
    nodes.push(node("review", "review", "Review the resulting diff against the objective and acceptance criteria.", ["verify"], ["workspace"], "shared"));
  } else if (strategy === "PARALLEL_DELEGATE") {
    nodes.push(node("investigate", "investigation", `Inspect the repository and identify independent implementation slices for: ${objective}`, [], [], "shared"));
    const count = Math.max(2, Math.min(maxWorkers, 4));
    for (let index = 0; index < count; index += 1) nodes.push(node(`implement-${index + 1}`, "implementation", `${implementObjective} (scoped slice ${index + 1})`, ["investigate"], [`scope-${index + 1}`], "worktree"));
    nodes.push(node("integrate", "integration", "Integrate worker patches, resolving conflicts conservatively.", nodes.slice(1).map((entry) => entry.nodeId), ["workspace"], "shared"));
    nodes.push(node("verify", "verification", "Run the relevant configured verification commands.", ["integrate"], ["workspace"], "shared"));
    nodes.push(node("review", "review", "Review the integrated diff against the objective and acceptance criteria.", ["verify"], ["workspace"], "shared"));
  } else {
    nodes.push(node("investigate", "investigation", `Inspect the repository and plan the implementation for: ${objective}`, [], [], "shared"));
    nodes.push(node("implement", "implementation", implementObjective, ["investigate"], ["workspace"], strategy === "DELEGATE" ? "worktree" : "shared"));
    nodes.push(node("integrate", "integration", "Integrate the implementation into the target workspace.", ["implement"], ["workspace"], "shared"));
    nodes.push(node("verify", "verification", "Run the relevant configured verification commands.", ["integrate"], ["workspace"], "shared"));
    nodes.push(node("review", "review", "Review the resulting diff against the objective and acceptance criteria.", ["verify"], ["workspace"], "shared"));
  }
  return { planId, strategy, nodes, createdAtMs: now(), revision: 1, rationale: `Selected ${strategy} from task constraints and objective complexity.` };
}

export function validateTaskPlan(plan: TaskPlan): string[] {
  const errors: string[] = [];
  if (plan.nodes.length === 0 || plan.nodes.length > MAX_TASK_PLAN_NODES) errors.push(`plan must contain 1..${MAX_TASK_PLAN_NODES} nodes`);
  const ids = new Set<string>();
  for (const current of plan.nodes) {
    if (ids.has(current.nodeId)) errors.push(`duplicate node ${current.nodeId}`);
    ids.add(current.nodeId);
  }
  const byId = new Map(plan.nodes.map((current) => [current.nodeId, current]));
  for (const current of plan.nodes) for (const dependency of current.dependencies) if (!byId.has(dependency)) errors.push(`${current.nodeId} depends on missing node ${dependency}`);
  const visiting = new Set<string>();
  const visited = new Set<string>();
  const visit = (currentId: string): void => {
    if (visiting.has(currentId)) { errors.push(`cycle involving ${currentId}`); return; }
    if (visited.has(currentId)) return;
    visiting.add(currentId);
    for (const dependency of byId.get(currentId)?.dependencies ?? []) if (byId.has(dependency)) visit(dependency);
    visiting.delete(currentId); visited.add(currentId);
  };
  for (const current of plan.nodes) visit(current.nodeId);
  if (plan.strategy === "PARALLEL_DELEGATE" && !plan.nodes.some((current) => current.taskClass === "integration")) errors.push("parallel plans require an integration node");
  return [...new Set(errors)];
}

export function createAutonomousTask(input: CreateAutonomousTaskInput): AutonomousTask {
  const objective = input.objective.trim();
  if (!objective) throw new Error("Autonomous task objective must not be empty.");
  const constraints = input.constraints ?? {};
  const strategy = chooseTaskExecutionStrategy(input);
  const timestamp = now();
  return {
    schemaVersion: AUTONOMOUS_TASK_SCHEMA_VERSION,
    taskId: id("task"), sessionId: input.sessionId ?? null, objective, source: constraints.source ?? "user", untrustedSource: constraints.untrustedSource ?? false,
    createdAtMs: timestamp, updatedAtMs: timestamp, targetSnapshot: structuredClone(input.targetSnapshot), workspaceRoots: structuredClone(input.workspaceRoots),
    permissionSnapshot: structuredClone(input.permissionSnapshot),
    budgetSnapshot: { wallTimeMs: 30 * 60_000, maxModelCalls: 64, maxToolCalls: 128, maxRepairRounds: 2, maxWorkers: 4, ...input.budgetSnapshot },
    constraints: { ...constraints, strategy }, plan: null, acceptanceCriteria: deriveAcceptanceCriteria(objective, constraints.source ?? "user"), workers: [], artifacts: [], verificationEvidence: [], guidance: [], outcome: "RUNNING", summary: null, repairRounds: 0,
  };
}

export function installTaskPlan(task: AutonomousTask, plan: TaskPlan): AutonomousTask {
  const errors = validateTaskPlan(plan);
  if (errors.length) throw new Error(`Invalid task plan: ${errors.join("; ")}`);
  return { ...task, plan: structuredClone(plan), updatedAtMs: now() };
}

export function getReadyTaskPlanNodes(plan: TaskPlan): TaskPlanNode[] {
  const completed = new Set(plan.nodes.filter((current) => current.status === "succeeded").map((current) => current.nodeId));
  return plan.nodes.filter((current) => (current.status === "pending" || current.status === "ready") && current.dependencies.every((dependency) => completed.has(dependency))).map((current) => ({ ...current, status: "ready" }));
}

export function canRunTaskNodesTogether(left: TaskPlanNode, right: TaskPlanNode): boolean {
  if (left.isolation === "shared" || right.isolation === "shared") return false;
  const leftScopes = new Set(left.mutationScope);
  return !right.mutationScope.some((scope) => leftScopes.has(scope));
}

export function buildWorkerContext(task: AutonomousTask, nodeToRun: TaskPlanNode): string {
  const objective = task.untrustedSource ? `<untrusted-task-objective>\n${task.objective}\n</untrusted-task-objective>` : task.objective;
  return [`Task node: ${nodeToRun.nodeId}`, `Node objective: ${nodeToRun.objective}`, `Task objective: ${objective}`, "The task snapshot, workspace roots, permissions, target, and budget are immutable. Do not broaden scope or access secrets.", "Return a concise report of work performed, files changed, checks run, and any blocker."].join("\n");
}

export function taskEvent(eventType: AutonomousTaskEventType, task: AutonomousTask, payload: Record<string, unknown> = {}): TaskEvent {
  const snapshot = structuredClone(task);
  snapshot.objective = snapshot.objective.slice(0, 8_000);
  snapshot.guidance = snapshot.guidance.slice(-8).map((guidance) => ({ ...guidance, text: guidance.text.slice(0, 2_000) }));
  snapshot.verificationEvidence = snapshot.verificationEvidence.slice(-32).map((evidence) => ({ ...evidence, summary: evidence.summary.slice(0, 2_000) }));
  snapshot.artifacts = snapshot.artifacts.slice(-64).map((artifact) => ({ ...artifact, label: artifact.label.slice(0, 500) }));
  if (snapshot.plan) snapshot.plan = { ...snapshot.plan, nodes: snapshot.plan.nodes.map((node) => ({ ...node, objective: node.objective.slice(0, 2_000), resultSummary: node.resultSummary?.slice(0, 2_000) ?? null })) };
  return { eventId: id("task-event"), taskId: task.taskId, eventType, occurredAtMs: now(), payload: { ...payload, snapshot } };
}

export function taskEventToRunEvent(event: TaskEvent): RunEventWire {
  return { type: "task_event", payload: { task_id: event.taskId, event_type: event.eventType, payload: event.payload } };
}

export function applyTaskEvent(task: AutonomousTask, event: TaskEvent): AutonomousTask {
  const snapshot = event.payload.snapshot;
  if (snapshot && typeof snapshot === "object" && (snapshot as AutonomousTask).taskId === task.taskId) return structuredClone(snapshot as AutonomousTask);
  return { ...task, updatedAtMs: event.occurredAtMs };
}

export function evidenceForCriterion(task: AutonomousTask, criterion: AcceptanceCriterion): VerificationEvidence[] {
  return task.verificationEvidence.filter((evidence) => criterion.evidenceIds.includes(evidence.evidenceId));
}

export function hasAuthoritativeAcceptanceEvidence(task: AutonomousTask): boolean {
  return task.acceptanceCriteria.filter((criterion) => criterion.blocking).every((criterion) => criterion.status === "passed" && evidenceForCriterion(task, criterion).some((evidence) => evidence.authoritative && evidence.passed && !evidence.stale));
}

export function taskSnapshotPayload(task: AutonomousTask): Record<string, unknown> {
  return structuredClone(task) as unknown as Record<string, unknown>;
}
