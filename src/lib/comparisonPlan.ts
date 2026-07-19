import { invoke, isTauri } from "@tauri-apps/api/core";

import type { ModelTargetSnapshot } from "./modelTargets";

export interface SystemMemoryInfo {
  totalBytes: number;
  availableBytes: number;
}

export type ComparisonExecutionMode = "concurrent" | "local_sequential";

export interface ComparisonExecutionBranchPlan {
  /** Filled after comparison sessions are created and before any branch is
   * launched. Older drafts may temporarily carry null, but persisted plans
   * are bound to a concrete branch session. */
  sessionId: string | null;
  targetKey: string;
  mode: "concurrent" | "queued";
  queuePosition: number | null;
  estimatedResidentBytes: number | null;
}

/** Persisted before a comparison launches so queued labels and reload
 * behavior describe the decision that actually governed the run. */
export interface ComparisonExecutionPlan {
  version: 1;
  mode: ComparisonExecutionMode;
  strategy: "concurrent" | "memory_queue";
  localTargetKeys: string[];
  branches: ComparisonExecutionBranchPlan[];
  estimatedLocalBytes: number | null;
  availableMemoryBytes: number | null;
  budgetMemoryBytes: number | null;
  reason: "within_budget" | "memory_pressure" | "memory_unknown" | "remote_only";
  /** Exact names returned by Ollama /api/ps before the comparison started.
   * Null means the snapshot was unavailable, in which case cleanup is
   * deliberately skipped rather than risking a user-owned resident model. */
  residentOllamaModels: string[] | null;
  cleanupWarnings: string[];
}

const SAFE_AVAILABLE_MEMORY_FRACTION = 0.8;

function isNonNegativeFinite(value: unknown): value is number {
  return typeof value === "number" && Number.isFinite(value) && value >= 0;
}

export function isLocalExecutionTarget(target: ModelTargetSnapshot): boolean {
  return target.kind === "local" || (target.kind === "ollama" && target.isCloud !== true);
}

/** Reads physical-memory availability from the desktop backend. Failure is
 * deliberately represented as unknown: the planner then serializes local
 * branches conservatively rather than pretending they are safe. */
export async function loadSystemMemoryInfo(): Promise<SystemMemoryInfo | null> {
  if (!isTauri()) return null;
  try {
    const raw = await invoke<{ totalBytes?: unknown; availableBytes?: unknown }>("system_memory_info");
    if (!isNonNegativeFinite(raw.totalBytes) || !isNonNegativeFinite(raw.availableBytes)) return null;
    if (raw.totalBytes === 0 || raw.availableBytes > raw.totalBytes) return null;
    return { totalBytes: raw.totalBytes, availableBytes: raw.availableBytes };
  } catch {
    return null;
  }
}

/** Chooses concurrency before any branch starts. Provider and cloud-Ollama
 * requests always remain concurrent; only local-execution targets are put
 * behind a sequential chain when their conservative resident estimate
 * cannot fit inside 80% of currently available physical memory. */
export function buildComparisonExecutionPlan(
  targets: readonly ModelTargetSnapshot[],
  memory: SystemMemoryInfo | null,
): ComparisonExecutionPlan {
  const localTargets = targets.filter(isLocalExecutionTarget);
  const localTargetKeys = localTargets.map((target) => target.key);
  if (localTargets.length <= 1) {
    const estimated = localTargets.reduce((sum, target) => sum + (target.estimatedMemoryBytes ?? 0), 0);
    return {
      version: 1,
      mode: "concurrent",
      strategy: "concurrent",
      localTargetKeys,
      branches: targets.map((target) => ({
        sessionId: null,
        targetKey: target.key,
        mode: "concurrent",
        queuePosition: null,
        estimatedResidentBytes:
          typeof target.estimatedMemoryBytes === "number" ? target.estimatedMemoryBytes : null,
      })),
      estimatedLocalBytes: localTargets.some((target) => target.estimatedMemoryBytes === undefined)
        ? null
        : estimated,
      availableMemoryBytes: memory?.availableBytes ?? null,
      budgetMemoryBytes: memory ? Math.floor(memory.availableBytes * SAFE_AVAILABLE_MEMORY_FRACTION) : null,
      reason: localTargets.length === 0 ? "remote_only" : "within_budget",
      residentOllamaModels: null,
      cleanupWarnings: [],
    };
  }

  const estimatesKnown = localTargets.every(
    (target) => typeof target.estimatedMemoryBytes === "number" && target.estimatedMemoryBytes > 0,
  );
  const estimatedLocalBytes = estimatesKnown
    ? localTargets.reduce((sum, target) => sum + (target.estimatedMemoryBytes ?? 0), 0)
    : null;
  const budgetMemoryBytes = memory ? Math.floor(memory.availableBytes * SAFE_AVAILABLE_MEMORY_FRACTION) : null;
  const memoryUnknown = estimatedLocalBytes === null || budgetMemoryBytes === null;
  const exceedsBudget =
    estimatedLocalBytes !== null && budgetMemoryBytes !== null && estimatedLocalBytes > budgetMemoryBytes;

  const mode: ComparisonExecutionMode = memoryUnknown || exceedsBudget ? "local_sequential" : "concurrent";
  let queuePosition = 0;
  return {
    version: 1,
    mode,
    strategy: mode === "local_sequential" ? "memory_queue" : "concurrent",
    localTargetKeys,
    branches: targets.map((target) => {
      const queued = mode === "local_sequential" && isLocalExecutionTarget(target);
      return {
        sessionId: null,
        targetKey: target.key,
        mode: queued ? "queued" : "concurrent",
        queuePosition: queued ? queuePosition++ : null,
        estimatedResidentBytes:
          typeof target.estimatedMemoryBytes === "number" ? target.estimatedMemoryBytes : null,
      };
    }),
    estimatedLocalBytes,
    availableMemoryBytes: memory?.availableBytes ?? null,
    budgetMemoryBytes,
    reason: memoryUnknown ? "memory_unknown" : exceedsBudget ? "memory_pressure" : "within_budget",
    residentOllamaModels: null,
    cleanupWarnings: [],
  };
}

/** Binds the target-oriented draft plan to the sessions created for those
 * targets and records the safe cleanup baseline. The plan is then immutable
 * input for the full run and every retry. */
export function finalizeComparisonExecutionPlan(
  plan: ComparisonExecutionPlan,
  sessionIds: readonly string[],
  residentOllamaModels: readonly string[] | null,
): ComparisonExecutionPlan {
  if (sessionIds.length !== plan.branches.length) {
    throw new Error("Comparison branch/session count does not match its execution plan.");
  }
  return {
    ...plan,
    branches: plan.branches.map((branch, index) => ({ ...branch, sessionId: sessionIds[index] })),
    residentOllamaModels:
      residentOllamaModels === null ? null : [...new Set(residentOllamaModels.filter((name) => name.trim().length > 0))],
    cleanupWarnings: [...plan.cleanupWarnings],
  };
}

interface OllamaRunningModelWire {
  name?: unknown;
}

/** Snapshots Ollama residency. A failed/invalid response returns null so the
 * caller never guesses ownership and therefore never unloads anything. */
export async function loadResidentOllamaModels(): Promise<string[] | null> {
  if (!isTauri()) return null;
  try {
    const raw = await invoke<unknown>("ollama_list_running_models");
    if (!Array.isArray(raw)) return null;
    const names: string[] = [];
    for (const item of raw) {
      if (!item || typeof item !== "object") return null;
      const name = (item as OllamaRunningModelWire).name;
      if (typeof name !== "string" || name.trim().length === 0) return null;
      names.push(name);
    }
    return [...new Set(names)];
  } catch {
    return null;
  }
}

export async function unloadComparisonOllamaModel(model: string): Promise<void> {
  if (!isTauri()) return;
  await invoke("ollama_unload_model", { model });
}

export function isComparisonExecutionPlan(value: unknown): value is ComparisonExecutionPlan {
  if (!value || typeof value !== "object") return false;
  const candidate = value as Partial<ComparisonExecutionPlan>;
  const nullableNumber = (entry: unknown) => entry === null || isNonNegativeFinite(entry);
  return (
    candidate.version === 1 &&
    (candidate.mode === "concurrent" || candidate.mode === "local_sequential") &&
    (candidate.strategy === "concurrent" || candidate.strategy === "memory_queue") &&
    ((candidate.mode === "concurrent" && candidate.strategy === "concurrent") ||
      (candidate.mode === "local_sequential" && candidate.strategy === "memory_queue")) &&
    Array.isArray(candidate.localTargetKeys) &&
    candidate.localTargetKeys.every((key) => typeof key === "string") &&
    Array.isArray(candidate.branches) &&
    candidate.branches.every((branch) => {
      if (!branch || typeof branch !== "object") return false;
      const item = branch as Partial<ComparisonExecutionBranchPlan>;
      return (
        (item.sessionId === null || typeof item.sessionId === "string") &&
        typeof item.targetKey === "string" &&
        (item.mode === "concurrent" || item.mode === "queued") &&
        (item.queuePosition === null ||
          (Number.isInteger(item.queuePosition) && (item.queuePosition as number) >= 0)) &&
        nullableNumber(item.estimatedResidentBytes) &&
        ((item.mode === "queued" && item.queuePosition !== null) ||
          (item.mode === "concurrent" && item.queuePosition === null))
      );
    }) &&
    nullableNumber(candidate.estimatedLocalBytes) &&
    nullableNumber(candidate.availableMemoryBytes) &&
    nullableNumber(candidate.budgetMemoryBytes) &&
    (candidate.reason === "within_budget" ||
      candidate.reason === "memory_pressure" ||
      candidate.reason === "memory_unknown" ||
      candidate.reason === "remote_only") &&
    (candidate.residentOllamaModels === null ||
      (Array.isArray(candidate.residentOllamaModels) &&
        candidate.residentOllamaModels.every((name) => typeof name === "string" && name.trim().length > 0))) &&
    Array.isArray(candidate.cleanupWarnings) &&
    candidate.cleanupWarnings.every((warning) => typeof warning === "string")
  );
}
