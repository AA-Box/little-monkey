import type { ModelTargetSnapshot } from "./modelTargets";
import type { AutonomousTaskEventType, RunEventWire } from "./runProtocol";
import type { WorkspaceRootInfo } from "../store/workspaceStore";

export const AUTONOMOUS_TASK_SCHEMA_VERSION = 1 as const;
export const MAX_TASK_PLAN_NODES = 64;
export const MAX_TASK_WORKERS = 16;
export const MAX_TASK_GUIDANCE_ITEMS = 32;

export type TaskExecutionStrategy = "DIRECT" | "PLAN" | "DELEGATE" | "PARALLEL_DELEGATE";
export type TaskClass = "investigation" | "implementation" | "integration" | "verification" | "review" | "delivery";
export type TaskNodeStatus = "pending" | "ready" | "running" | "waiting_approval" | "succeeded" | "failed" | "blocked" | "cancelled";
export type TaskIsolation = "shared" | "worktree";
export type ExecutionPlacementKind = "local" | "worktree" | "docker" | "remote_node" | (string & {});
export type DeliveryIntent = "leave_worktree" | "commit" | "push_owned_branch" | "open_or_update_pr";
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
  maxConcurrentWorkers?: number;
  maxNestingDepth?: number;
  maxArtifactBytes?: number;
  maxCostMicros?: number | null;
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
  deliveryIntent?: DeliveryIntent;
  executionPlacement?: TaskExecutionPlacement;
}

export interface TaskExecutionRequirements { needsWorkspaceWrite: boolean; needsNetwork: boolean; isolation: TaskIsolation; platform?: string; }
export interface TaskExecutionPlacement { kind: ExecutionPlacementKind; targetId: string; nodeId: string; reason: string; capabilities?: string[]; requestedPlacement?: TaskExecutionPlacement; placementFulfilled?: boolean; }
export interface TaskExecutionOwner { kind: "desktop" | "daemon" | "remote"; instanceId: string; leaseEpoch: number; leaseExpiresAtMs: number; }
export interface TaskDeliveryTarget { worktreeId: string; repositorySlug: string; branch: string; remote: string; base: string; title: string; body: string; changedFiles?: string[]; prNumber?: number; }
export interface TaskPlanningContext { currentWorkspaceRevision: string; relevantFiles: string[]; repositoryConventions: string[]; sourceMaterial: string[]; dependencyArtifactIds: string[]; upstreamDecisions: string[]; }
export interface TaskUsage { modelCalls: number; toolCalls: number; inputTokens: number; outputTokens: number; costMicros: number; artifactBytes: number; workersStarted: number; }

export interface CriterionProvenance {
  kind: "user" | "source_material" | "github_issue" | "test" | "standard" | "repository";
  fragment: string;
  location?: string;
}

export interface AcceptanceCriterion {
  id: string;
  description: string;
  status: CriterionStatus;
  method: CriterionMethod;
  source: string;
  provenance?: CriterionProvenance;
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
  relevantFiles?: string[];
  capabilities?: string[];
  executionPlacement?: TaskExecutionPlacement;
  requestedExecutionPlacement?: TaskExecutionPlacement;
  executionRequirements?: TaskExecutionRequirements;
  budget?: Partial<TaskBudgetSnapshot>;
  upstreamDecisions?: string[];
  repairOf?: string | null;
  mutationRevision?: string | null;
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
  executionPlacement?: TaskExecutionPlacement;
  worktree?: { id: string; path: string; branch: string; baseRevision: string; diffDigest: string };
  changedFiles?: string[];
  mutation?: { beforeRevision: string; afterRevision: string; changedFiles: string[]; patchDigest: string };
  artifacts?: TaskArtifact[];
  resultSummary?: string;
  failureCode?: string;
  failureKind?: string;
  usage?: Partial<TaskUsage>;
  resultId?: string;
}

export interface TaskArtifact {
  artifactId: string;
  kind: "patch" | "report" | "verification" | "review" | "delivery";
  label: string;
  path: string | null;
  digest: string | null;
  createdAtMs: number;
  workspaceRevision?: string | null;
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
  command?: string | null;
  commandDigest?: string | null;
  workspaceRevision?: string | null;
  testedRevision?: string | null;
  source?: "command" | "diff" | "review" | "worker_report" | "user";
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
  planningContext?: TaskPlanningContext;
  workspaceRevision?: string;
  usage?: TaskUsage;
  deliveryIntent?: DeliveryIntent;
  deliveryTarget?: TaskDeliveryTarget;
  waitingReason?: string | null;
  waitingApproval?: { requestId: string; operationDigest: string; expiresAtMs: number; confirmationPhrase?: string; nodeId: string } | null;
  deliveryStep?: "commit" | "push" | "create_draft_pr" | "update_draft_pr";
  executionOwner: TaskExecutionOwner;
}

export interface CreateAutonomousTaskInput {
  objective: string;
  sessionId?: string | null;
  targetSnapshot: ModelTargetSnapshot;
  workspaceRoots: WorkspaceRootInfo[];
  permissionSnapshot: TaskPermissionSnapshot;
  budgetSnapshot?: Partial<TaskBudgetSnapshot>;
  constraints?: TaskConstraints;
  planningContext?: Partial<TaskPlanningContext>;
  deliveryIntent?: DeliveryIntent;
  deliveryTarget?: TaskDeliveryTarget;
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
  return deriveAcceptanceCriteriaFromContext(objective, source);
}

export function deriveAcceptanceCriteriaFromContext(objective: string, source = "coordinator", context?: Pick<TaskPlanningContext, "sourceMaterial" | "relevantFiles">): AcceptanceCriterion[] {
  const prefix = id("criterion");
  const extracted = [...(context?.sourceMaterial ?? [])]
    .flatMap((value) => value.split(/\r?\n/))
    .map((line) => line.replace(/^\s*(?:[-*]|\d+[.)])\s*/, "").trim())
    .filter((line) => line.length >= 8 && /\b(must|should|verify|accept|require|test|ensure|implement)\b/i.test(line))
    .slice(0, 8);
  return [
    ...extracted.map((description, index) => ({ id: `${prefix}-extracted-${index + 1}`, description, provenance: { kind: "source_material" as const, fragment: description, location: `sourceMaterial:${index + 1}` }, status: "pending" as const, method: "review" as const, source, evidenceIds: [], blocking: true })),
    { id: `${prefix}-objective`, description: `The requested objective is implemented: ${objective}`, provenance: { kind: "user" as const, fragment: objective }, status: "pending", method: "review", source, evidenceIds: [], blocking: true },
    { id: `${prefix}-scope`, description: "The change stays within the requested scope and preserves unrelated behavior.", provenance: { kind: "repository" as const, fragment: "requested scope and unrelated behavior" }, status: "pending", method: "review", source, evidenceIds: [], blocking: true },
    { id: `${prefix}-verification`, description: "Relevant repository verification passes against the current workspace revision.", provenance: { kind: "repository" as const, fragment: "configured repository verification" }, status: "pending", method: "verification_command", source, evidenceIds: [], blocking: true },
  ];
}

function node(nodeId: string, taskClass: TaskClass, objective: string, dependencies: string[], mutationScope: string[], isolation: TaskIsolation, context?: TaskPlanningContext): TaskPlanNode {
  const mutates = taskClass === "implementation" || taskClass === "integration";
  return { nodeId, taskClass, objective, dependencies, mutationScope, isolation, status: dependencies.length ? "pending" : "ready", attempt: 0, workerId: null, resultSummary: null, relevantFiles: context?.relevantFiles.slice(0, 64), capabilities: mutates ? ["read", "mutate", "verify"] : ["read"], executionRequirements: { needsWorkspaceWrite: mutates, needsNetwork: false, isolation }, upstreamDecisions: context?.upstreamDecisions.slice(-16), repairOf: null, mutationRevision: context?.currentWorkspaceRevision ?? null };
}

function independentSlices(files: readonly string[], maxWorkers: number): string[][] {
  const groups = new Map<string, string[]>();
  for (const file of files) { const normalized = file.split("\\").join("/").replace(/^\.?\//, ""); const key = normalized.split("/")[0] || "workspace"; groups.set(key, [...(groups.get(key) ?? []), normalized]); }
  return [...groups.values()].sort((left, right) => left[0].localeCompare(right[0])).slice(0, Math.max(1, Math.min(maxWorkers, 8)));
}

export function createTaskPlan(objective: string, strategy: TaskExecutionStrategy, maxWorkers = 4, context?: TaskPlanningContext): TaskPlan {
  const planId = id("plan");
  const implementObjective = `Implement the smallest safe change for: ${objective}`;
  const nodes: TaskPlanNode[] = [];
  if (strategy === "DIRECT") {
    nodes.push(node("implement", "implementation", implementObjective, [], context?.relevantFiles.length ? context.relevantFiles : ["workspace"], "shared", context));
    nodes.push(node("verify", "verification", "Run the relevant configured verification commands against the current revision.", ["implement"], ["workspace"], "shared", context));
    nodes.push(node("review", "review", "Review the resulting diff against the objective, extracted criteria, and planned scope.", ["verify"], ["workspace"], "shared", context));
  } else if (strategy === "PARALLEL_DELEGATE") {
    nodes.push(node("investigate", "investigation", `Inspect the repository and identify independent implementation slices for: ${objective}`, [], [], "shared"));
    const slices = independentSlices(context?.relevantFiles ?? [], maxWorkers);
    const workerNodes = slices.length > 1 ? slices : [context?.relevantFiles ?? []];
    for (let index = 0; index < workerNodes.length; index += 1) { const scope = workerNodes[index].length ? workerNodes[index] : ["workspace"]; nodes.push(node(`implement-${index + 1}`, "implementation", `${implementObjective} in ${scope.join(", ")}`, ["investigate"], scope, "worktree", context)); }
    nodes.push(node("integrate", "integration", "Inspect and integrate worker patches only after scope and overlap validation.", nodes.slice(1).map((entry) => entry.nodeId), ["workspace"], "shared", context));
    nodes.push(node("verify", "verification", "Run the relevant configured verification commands against the integrated revision.", ["integrate"], ["workspace"], "shared", context));
    nodes.push(node("review", "review", "Review the integrated diff against the extracted criteria and planned scopes.", ["verify"], ["workspace"], "shared", context));
  } else {
    nodes.push(node("investigate", "investigation", `Inspect the repository and produce a structured implementation plan for: ${objective}`, [], [], "shared", context));
    nodes.push(node("implement", "implementation", implementObjective, ["investigate"], context?.relevantFiles.length ? context.relevantFiles : ["workspace"], strategy === "DELEGATE" ? "worktree" : "shared", context));
    nodes.push(node("integrate", "integration", "Inspect and integrate the implementation patch after scope validation.", ["implement"], ["workspace"], "shared", context));
    nodes.push(node("verify", "verification", "Run the relevant configured verification commands against the integrated revision.", ["integrate"], ["workspace"], "shared", context));
    nodes.push(node("review", "review", "Review the resulting diff against the objective and planned scope.", ["verify"], ["workspace"], "shared", context));
  }
  if (context?.currentWorkspaceRevision) for (const entry of nodes) entry.executionPlacement = { kind: entry.isolation === "worktree" ? "worktree" : "local", targetId: "local", nodeId: entry.nodeId, reason: entry.isolation === "worktree" ? "mutating worker isolation" : "shared workspace coordinator stage" };
  return { planId, strategy, nodes, createdAtMs: now(), revision: 1, rationale: `Structured plan for ${strategy}; scopes come from repository context and are validated before mutation.` };
}

export function validateTaskPlan(plan: TaskPlan, context?: TaskPlanningContext): string[] {
  const errors: string[] = [];
  if (plan.nodes.length === 0 || plan.nodes.length > MAX_TASK_PLAN_NODES) errors.push(`plan must contain 1..${MAX_TASK_PLAN_NODES} nodes`);
  const ids = new Set<string>();
  for (const current of plan.nodes) {
    if (ids.has(current.nodeId)) errors.push(`duplicate node ${current.nodeId}`);
    ids.add(current.nodeId);
  }
  const byId = new Map(plan.nodes.map((current) => [current.nodeId, current]));
  const dependsOn = (nodeId: string, ancestorId: string, visiting = new Set<string>()): boolean => {
    if (nodeId === ancestorId) return true;
    if (visiting.has(nodeId)) return false;
    visiting.add(nodeId);
    const found = (byId.get(nodeId)?.dependencies ?? []).some((dependency) => dependsOn(dependency, ancestorId, visiting));
    visiting.delete(nodeId);
    return found;
  };
  for (const current of plan.nodes) for (const dependency of current.dependencies) if (!byId.has(dependency)) errors.push(`${current.nodeId} depends on missing node ${dependency}`);
  for (const current of plan.nodes) {
    if ((current.taskClass === "implementation" || current.taskClass === "integration") && current.mutationScope.length === 0) errors.push(`${current.nodeId} mutating nodes require a non-empty mutation scope`);
    if (current.mutationScope.some((scope) => scope.includes("..") || scope.startsWith("/"))) errors.push(`${current.nodeId} mutation scope may not escape the workspace`);
    if (current.executionPlacement && current.executionPlacement.nodeId !== current.nodeId) errors.push(`${current.nodeId} placement must identify the same node`);
    if (current.executionPlacement?.placementFulfilled && current.executionPlacement.kind !== "local" && current.executionPlacement.kind !== "worktree") errors.push(`${current.nodeId} cannot invoke another external placement after its placement was fulfilled`);
    if (current.executionPlacement?.placementFulfilled && !current.requestedExecutionPlacement) errors.push(`${current.nodeId} placement provenance is required after placement fulfillment`);
    if (current.requestedExecutionPlacement && !current.executionPlacement) errors.push(`${current.nodeId} placement provenance requires a receiver execution contract`);
    if (current.capabilities?.some((capability) => !["read", "mutate", "verify", "network", "git", "delegate"].includes(capability))) errors.push(`${current.nodeId} requests an unknown capability`);
    if (["investigation", "verification", "review"].includes(current.taskClass) && current.capabilities?.includes("mutate")) errors.push(`${current.nodeId} non-mutating nodes may not request mutate`);
    if (["investigation", "verification", "review"].includes(current.taskClass) && current.isolation !== "shared") errors.push(`${current.nodeId} non-mutating nodes must use shared isolation`);
    if (current.executionRequirements?.needsWorkspaceWrite && !current.capabilities?.includes("mutate")) errors.push(`${current.nodeId} requires workspace write capability but does not request mutate`);
    if (current.executionRequirements?.needsNetwork && !current.capabilities?.includes("network")) errors.push(`${current.nodeId} requires network capability but does not request network`);
    if (current.executionRequirements && current.executionRequirements.isolation !== current.isolation) errors.push(`${current.nodeId} execution requirements and node isolation disagree`);
    if (current.executionPlacement?.kind === "worktree" && current.isolation !== "worktree") errors.push(`${current.nodeId} worktree placement requires worktree isolation`);
    if (current.executionPlacement && current.executionPlacement.kind !== "local" && current.executionPlacement.kind !== "worktree" && !current.executionPlacement.targetId.trim()) errors.push(current.nodeId + " " + current.executionPlacement.kind + " placement requires a target id");
    if (context && context.relevantFiles.length > 0 && current.relevantFiles?.some((file) => file !== "workspace" && !context.relevantFiles.includes(file))) errors.push(`${current.nodeId} references files outside the planner's repository context`);
  }
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
  if (plan.strategy === "PARALLEL_DELEGATE") {
    const workers = plan.nodes.filter((current) => current.taskClass === "implementation");
    for (let index = 0; index < workers.length; index += 1) for (const right of workers.slice(index + 1)) {
      if (workers[index].mutationScope.some((scope) => right.mutationScope.includes(scope))) errors.push(`parallel worker scopes overlap: ${workers[index].nodeId} and ${right.nodeId}`);
    }
  }
  if (!plan.nodes.some((current) => current.taskClass === "verification")) errors.push("plans require a verification node");
  if (!plan.nodes.some((current) => current.taskClass === "review")) errors.push("plans require a review node");
  const verifications = plan.nodes.filter((current) => current.taskClass === "verification");
  for (const review of plan.nodes.filter((current) => current.taskClass === "review")) {
    if (!verifications.some((verification) => dependsOn(review.nodeId, verification.nodeId))) errors.push(`${review.nodeId} must depend on verification evidence`);
    for (const mutation of plan.nodes.filter((current) => current.taskClass === "implementation" || current.taskClass === "integration")) if (dependsOn(mutation.nodeId, review.nodeId)) errors.push(`${mutation.nodeId} is scheduled after review ${review.nodeId}`);
  }
  const terminalVerifications = verifications.filter((verification) => !verifications.some((other) => other.nodeId !== verification.nodeId && dependsOn(other.nodeId, verification.nodeId)));
  const terminalReviews = plan.nodes.filter((current) => current.taskClass === "review" && !plan.nodes.some((other) => other.taskClass === "review" && other.nodeId !== current.nodeId && dependsOn(other.nodeId, current.nodeId)));
  for (const mutation of plan.nodes.filter((current) => current.taskClass === "implementation" || current.taskClass === "integration")) {
    if (!terminalVerifications.some((verification) => dependsOn(verification.nodeId, mutation.nodeId))) errors.push(`${mutation.nodeId} must be an ancestor of final verification evidence`);
    if (!terminalReviews.some((review) => dependsOn(review.nodeId, mutation.nodeId))) errors.push(`${mutation.nodeId} must be an ancestor of final review evidence`);
  }
  for (const delivery of plan.nodes.filter((current) => current.taskClass === "delivery")) if (!terminalReviews.some((review) => dependsOn(delivery.nodeId, review.nodeId))) errors.push(`${delivery.nodeId} must depend on final review evidence`);
  return [...new Set(errors)];
}

export function createAutonomousTask(input: CreateAutonomousTaskInput): AutonomousTask {
  const objective = input.objective.trim();
  if (!objective) throw new Error("Autonomous task objective must not be empty.");
  const constraints = input.constraints ?? {};
  const strategy = chooseTaskExecutionStrategy(input);
  const timestamp = now();
  const planningContext: TaskPlanningContext = { currentWorkspaceRevision: input.planningContext?.currentWorkspaceRevision ?? "unknown", relevantFiles: [...(input.planningContext?.relevantFiles ?? [])].slice(0, 128), repositoryConventions: [...(input.planningContext?.repositoryConventions ?? [])].slice(0, 32), sourceMaterial: [...(input.planningContext?.sourceMaterial ?? [])].slice(0, 32), dependencyArtifactIds: [...(input.planningContext?.dependencyArtifactIds ?? [])].slice(0, 64), upstreamDecisions: [...(input.planningContext?.upstreamDecisions ?? [])].slice(0, 32) };
  return {
    schemaVersion: AUTONOMOUS_TASK_SCHEMA_VERSION,
    taskId: id("task"), sessionId: input.sessionId ?? null, objective, source: constraints.source ?? "user", untrustedSource: constraints.untrustedSource ?? false,
    createdAtMs: timestamp, updatedAtMs: timestamp, targetSnapshot: structuredClone(input.targetSnapshot), workspaceRoots: structuredClone(input.workspaceRoots),
    permissionSnapshot: structuredClone(input.permissionSnapshot),
    budgetSnapshot: { wallTimeMs: 30 * 60_000, maxModelCalls: 64, maxToolCalls: 128, maxRepairRounds: 2, maxWorkers: 4, maxConcurrentWorkers: 4, maxNestingDepth: 1, maxArtifactBytes: 256 * 1024 * 1024, maxCostMicros: null, ...input.budgetSnapshot },
    constraints: { ...constraints, strategy }, plan: null, acceptanceCriteria: deriveAcceptanceCriteriaFromContext(objective, constraints.source ?? "user", planningContext), workers: [], artifacts: [], verificationEvidence: [], guidance: [], outcome: "RUNNING", summary: null, repairRounds: 0, planningContext, workspaceRevision: planningContext.currentWorkspaceRevision, usage: { modelCalls: 0, toolCalls: 0, inputTokens: 0, outputTokens: 0, costMicros: 0, artifactBytes: 0, workersStarted: 0 }, deliveryIntent: input.deliveryIntent ?? constraints.deliveryIntent ?? "leave_worktree", deliveryTarget: input.deliveryTarget ? structuredClone(input.deliveryTarget) : undefined, waitingReason: null, waitingApproval: null, executionOwner: { kind: "desktop", instanceId: `desktop-${typeof crypto !== "undefined" && crypto.randomUUID ? crypto.randomUUID() : Date.now()}`, leaseEpoch: 1, leaseExpiresAtMs: timestamp + 60_000 },
  };
}

export function installTaskPlan(task: AutonomousTask, plan: TaskPlan): AutonomousTask {
  const errors = validateTaskPlan(plan, task.planningContext);
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
  return [`Task node: ${nodeToRun.nodeId}`, `Node objective: ${nodeToRun.objective}`, `Task objective: ${objective}`, `Relevant files: ${(nodeToRun.relevantFiles ?? task.planningContext?.relevantFiles ?? []).join(", ") || "inspect the repository first"}`, `Acceptance criteria:\n${task.acceptanceCriteria.map((criterion) => `- ${criterion.id}: ${criterion.description}`).join("\n")}`, `Repository conventions:\n${task.planningContext?.repositoryConventions.join("\n") || "follow existing repository conventions"}`, `Allowed capabilities: ${(nodeToRun.capabilities ?? ["read"]).join(", ")}`, `Execution placement: ${nodeToRun.executionPlacement?.kind ?? "local"}`, `Current workspace revision: ${task.workspaceRevision ?? "unknown"}`, `Operator guidance: ${task.guidance.slice(-8).map((item) => `${item.appliesTo}: ${item.text}`).join("\n") || "none"}`, "The task snapshot, workspace roots, permissions, target, and budget are immutable. Do not broaden scope or access secrets.", "Return a structured report of work performed, files changed, checks run, evidence, usage, and any blocker."].join("\n");
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
  return task.acceptanceCriteria.filter((criterion) => criterion.blocking).every((criterion) => criterion.status === "passed" && evidenceForCriterion(task, criterion).some((evidence) => evidence.authoritative && evidence.passed && !evidence.stale && (!task.workspaceRevision || !evidence.testedRevision || evidence.testedRevision === task.workspaceRevision)));
}

export function taskSnapshotPayload(task: AutonomousTask): Record<string, unknown> {
  return structuredClone(task) as unknown as Record<string, unknown>;
}
