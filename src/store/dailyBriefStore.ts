import { create } from "zustand";

import { useRunStore } from "./runStore";
import { useAutomationsStore, type AutomationEntry, type AutomationRunStatus } from "./automationsStore";
import { usePermissionStore, type PermissionRequest } from "./permissionStore";
import { useMcpStore } from "./mcpStore";
import { useRuntimeHubStore } from "./runtimeHubStore";
import type { RiskLevel, RunRecord, RunStatus } from "../lib/runProtocol";
import type { HardwareSnapshot, M3RuntimeCapability, M3StorageStatus } from "../lib/runtimeHubClient";

/**
 * Daily Brief and Command Center (ROADMAP.md Phase 6): a purely additive,
 * read-only aggregation of state the app already tracks elsewhere — the run
 * ledger (`runStore.ts`), cron automations (`automationsStore.ts`), the live
 * chat-turn approval queue (`permissionStore.ts`), configured MCP connectors
 * (`mcpStore.ts`), and the local model/runtime health snapshot
 * (`runtimeHubStore.ts`). It introduces no new Rust command and no new
 * network/connector call of its own: `refresh()` below only calls each
 * source's own already-existing `refresh()` method and then re-derives this
 * store's fields from whatever ends up in those stores' state.
 *
 * The derivation functions exported here are deliberately pure and
 * store-free (no zustand/Tauri imports), mirroring `lib/inbox.ts`'s own
 * split — the aggregation logic worth getting right is unit-testable
 * without mocking the app shell or a Tauri runtime.
 */

// ---------------------------------------------------------------------------
// Section shapes
// ---------------------------------------------------------------------------

export interface PendingApprovalBrief {
  id: string;
  kind: "chat_approval" | "run";
  title: string;
  detail: string;
  riskLevel: RiskLevel | null;
  requestedAtMs: number;
  runId: string | null;
  approvalRequestId: string | null;
}

/** Run statuses this brief treats as "still going" — everything short of a
 * terminal state, minus `waiting_for_permission` (that one is surfaced under
 * Pending approvals instead, so a run never quietly appears in both lists
 * without an approval action to take). */
const RUNNING_RUN_STATUSES = new Set<RunStatus>(["queued", "running", "cancelling", "paused"]);

export interface RunningTaskBrief {
  id: string;
  runId: string;
  title: string;
  subtitle: string;
  status: RunStatus;
  updatedAtMs: number;
}

export interface FailedScheduledJobBrief {
  id: string;
  automationEntryId: string;
  recipeName: string;
  cron: string;
  lastRunAt: number;
  lastStatus: Extract<AutomationRunStatus, "error" | "denied">;
}

export interface RecentlyCompletedBrief {
  id: string;
  runId: string;
  title: string;
  updatedAtMs: number;
}

export interface StaleTaskBrief {
  id: string;
  runId: string;
  title: string;
  status: RunStatus;
  updatedAtMs: number;
  staleForMs: number;
}

/** Placeholder shape for a future connector-sourced highlight — deliberately
 * unused today (see `buildConnectorHighlights`'s doc comment) but kept as a
 * named export so a later change that wires real content in has a type to
 * fill rather than inventing one under time pressure. */
export interface ConnectorHighlight {
  connectorId: string;
  label: string;
  summary: string;
}

export interface RuntimeNodeSummary {
  runtimeId: string;
  label: string;
  kind: string;
  canInfer: boolean;
}

export interface RuntimeHealthBrief {
  /** False until Runtime Hub's overview has loaded at least once — lets the
   * panel show "no data yet" instead of a fabricated all-zero summary. */
  hasData: boolean;
  nodes: RuntimeNodeSummary[];
  inferenceReadyCount: number;
  storageUsedBytes: number | null;
  storageQuotaBytes: number | null;
  overviewError: string | null;
  lanError: string | null;
}

export interface DailyBriefData {
  pendingApprovals: PendingApprovalBrief[];
  running: RunningTaskBrief[];
  failedScheduledJobs: FailedScheduledJobBrief[];
  recentlyCompleted: RecentlyCompletedBrief[];
  staleTasks: StaleTaskBrief[];
  connectorHighlights: ConnectorHighlight[];
  runtimeHealth: RuntimeHealthBrief;
}

/** How long a still-running/waiting task can go without an update before
 * this brief calls it "stale" — a judgment call specific to this panel
 * (nothing else in the app defines "stale"), not a value mirrored from
 * elsewhere. */
export const STALE_THRESHOLD_MS = 24 * 60 * 60 * 1000;

/** How many rows each of the "recently completed" section shows — enough to
 * be useful without turning the brief into a second Run Center. */
const RECENTLY_COMPLETED_LIMIT = 5;

function runTitle(run: RunRecord): string {
  return run.spec.task || run.spec.run_id;
}

// ---------------------------------------------------------------------------
// Pending approvals
// ---------------------------------------------------------------------------

/** Chat-turn tool-call approvals (from `permissionStore`'s live queue) plus
 * any run parked in `waiting_for_permission` (from `runStore`) — the two
 * places this app can currently be blocked on the user's decision. Neither
 * source needs enrichment/event-walking: both already carry everything shown
 * here on the object the store already has in memory. */
export function buildPendingApprovals(
  permissionQueue: readonly PermissionRequest[],
  runs: readonly RunRecord[],
  nowMs: number = Date.now(),
): PendingApprovalBrief[] {
  const chatApprovals: PendingApprovalBrief[] = permissionQueue.map((request) => ({
    id: `chat-approval:${request.id}`,
    kind: "chat_approval",
    title: request.agent_label ? `${request.tool} (${request.agent_label})` : request.tool,
    detail: request.detail.split("\n", 1)[0] ?? request.detail,
    riskLevel: request.risk_level ?? null,
    // `PermissionRequest` carries no timestamp of its own (see
    // `permissionStore.ts`) — `lib/inbox.ts`'s `buildChatApprovalInboxItem`
    // makes the same approximation for the same reason.
    requestedAtMs: nowMs,
    runId: null,
    approvalRequestId: request.id,
  }));

  const runApprovals: PendingApprovalBrief[] = runs
    .filter((run) => run.archivedAtMs == null && run.status === "waiting_for_permission")
    .map((run) => ({
      id: `run-approval:${run.spec.run_id}`,
      kind: "run",
      title: runTitle(run),
      detail: run.spec.target.label,
      riskLevel: null,
      requestedAtMs: run.updatedAtMs,
      runId: run.spec.run_id,
      approvalRequestId: null,
    }));

  return [...chatApprovals, ...runApprovals].sort((a, b) => b.requestedAtMs - a.requestedAtMs);
}

// ---------------------------------------------------------------------------
// Running agents/tasks
// ---------------------------------------------------------------------------

export function buildRunningTasks(runs: readonly RunRecord[]): RunningTaskBrief[] {
  return runs
    .filter((run) => run.archivedAtMs == null && RUNNING_RUN_STATUSES.has(run.status))
    .map((run) => ({
      id: `run:${run.spec.run_id}`,
      runId: run.spec.run_id,
      title: runTitle(run),
      subtitle: run.spec.target.label,
      status: run.status,
      updatedAtMs: run.updatedAtMs,
    }))
    .sort((a, b) => b.updatedAtMs - a.updatedAtMs);
}

// ---------------------------------------------------------------------------
// Failed scheduled jobs
// ---------------------------------------------------------------------------

function isFailedStatus(status: AutomationRunStatus | undefined): status is "error" | "denied" {
  return status === "error" || status === "denied";
}

/** Only entries still `enabled` — a disabled automation will never run
 * again, so surfacing its old failure as something to act on today would be
 * misleading (mirrors `lib/inbox.ts`'s `buildAutomationInboxItems`, which
 * applies the same filter for the same reason). */
export function buildFailedScheduledJobs(entries: readonly AutomationEntry[]): FailedScheduledJobBrief[] {
  return entries
    .filter((entry) => entry.enabled && isFailedStatus(entry.lastStatus) && entry.lastRunAt != null)
    .map((entry) => ({
      id: `automation:${entry.id}`,
      automationEntryId: entry.id,
      recipeName: entry.recipeName,
      cron: entry.cron,
      lastRunAt: entry.lastRunAt!,
      lastStatus: entry.lastStatus as "error" | "denied",
    }))
    .sort((a, b) => b.lastRunAt - a.lastRunAt);
}

// ---------------------------------------------------------------------------
// Recently completed
// ---------------------------------------------------------------------------

export function buildRecentlyCompleted(
  runs: readonly RunRecord[],
  limit: number = RECENTLY_COMPLETED_LIMIT,
): RecentlyCompletedBrief[] {
  return runs
    .filter((run) => run.archivedAtMs == null && run.status === "succeeded")
    .sort((a, b) => b.updatedAtMs - a.updatedAtMs)
    .slice(0, limit)
    .map((run) => ({
      id: `run:${run.spec.run_id}`,
      runId: run.spec.run_id,
      title: runTitle(run),
      updatedAtMs: run.updatedAtMs,
    }));
}

// ---------------------------------------------------------------------------
// Stale tasks
// ---------------------------------------------------------------------------

/** Anything still in flight (running, paused, or itself waiting on an
 * approval) that hasn't moved in over `STALE_THRESHOLD_MS` — the run may
 * also appear under Pending approvals or Running agents/tasks; that overlap
 * is intentional, the two sections answer different questions ("what needs
 * a decision" / "what's stuck") about the same run. */
export function buildStaleTasks(
  runs: readonly RunRecord[],
  nowMs: number = Date.now(),
  thresholdMs: number = STALE_THRESHOLD_MS,
): StaleTaskBrief[] {
  const inFlight = new Set<RunStatus>([...RUNNING_RUN_STATUSES, "waiting_for_permission"]);
  return runs
    .filter((run) => run.archivedAtMs == null && inFlight.has(run.status))
    .map((run) => ({ run, staleForMs: nowMs - run.updatedAtMs }))
    .filter(({ staleForMs }) => staleForMs >= thresholdMs)
    .sort((a, b) => b.staleForMs - a.staleForMs)
    .map(({ run, staleForMs }) => ({
      id: `run:${run.spec.run_id}`,
      runId: run.spec.run_id,
      title: runTitle(run),
      status: run.status,
      updatedAtMs: run.updatedAtMs,
      staleForMs,
    }));
}

// ---------------------------------------------------------------------------
// Connector-sourced highlights
// ---------------------------------------------------------------------------

/**
 * Deliberately always returns `[]` today. `mcpStore.ts` only caches a
 * connected server's *tool schema list* (`McpServerInfo.tools`), never any
 * content a tool call returned — there is no already-fetched "what happened
 * over there" data for any connector to surface as a highlight. Producing
 * one here would mean querying the connector live from a passive brief
 * refresh, which the roadmap's acceptance line ("no connector is queried
 * unless connected and enabled for the brief") is written to rule out, not
 * merely gate. `DailyBriefPanel` already omits this section whenever the
 * list is empty, so this is the correct behavior until some connector
 * exposes cached content this store can read instead of query — not a
 * placeholder to fill in blindly later.
 */
export function buildConnectorHighlights(): ConnectorHighlight[] {
  return [];
}

// ---------------------------------------------------------------------------
// Homelab / runtime node health
// ---------------------------------------------------------------------------

export function buildRuntimeHealth(input: {
  loaded: boolean;
  runtimes: readonly M3RuntimeCapability[];
  hardware: HardwareSnapshot | null;
  storage: M3StorageStatus | null;
  errors: Readonly<Record<string, string>>;
}): RuntimeHealthBrief {
  const nodes: RuntimeNodeSummary[] = input.runtimes.map((runtime) => ({
    runtimeId: runtime.descriptor.runtimeId,
    label: runtime.descriptor.label,
    kind: runtime.descriptor.kind,
    canInfer: runtime.canInfer,
  }));

  return {
    hasData: input.loaded,
    nodes,
    inferenceReadyCount: nodes.filter((node) => node.canInfer).length,
    storageUsedBytes: input.storage?.usedBytes ?? null,
    storageQuotaBytes: input.storage?.quotaBytes ?? null,
    overviewError: input.errors.overview ?? null,
    lanError: input.errors["lan-refresh"] ?? null,
  };
}

// ---------------------------------------------------------------------------
// Aggregate
// ---------------------------------------------------------------------------

export function buildDailyBrief(input: {
  runs: readonly RunRecord[];
  automationEntries: readonly AutomationEntry[];
  permissionQueue: readonly PermissionRequest[];
  runtimeHub: {
    loaded: boolean;
    runtimes: readonly M3RuntimeCapability[];
    hardware: HardwareSnapshot | null;
    storage: M3StorageStatus | null;
    errors: Readonly<Record<string, string>>;
  };
  nowMs?: number;
}): DailyBriefData {
  const nowMs = input.nowMs ?? Date.now();
  return {
    pendingApprovals: buildPendingApprovals(input.permissionQueue, input.runs, nowMs),
    running: buildRunningTasks(input.runs),
    failedScheduledJobs: buildFailedScheduledJobs(input.automationEntries),
    recentlyCompleted: buildRecentlyCompleted(input.runs),
    staleTasks: buildStaleTasks(input.runs, nowMs),
    connectorHighlights: buildConnectorHighlights(),
    runtimeHealth: buildRuntimeHealth(input.runtimeHub),
  };
}

// ---------------------------------------------------------------------------
// Store
// ---------------------------------------------------------------------------

const EMPTY_BRIEF: DailyBriefData = {
  pendingApprovals: [],
  running: [],
  failedScheduledJobs: [],
  recentlyCompleted: [],
  staleTasks: [],
  connectorHighlights: [],
  runtimeHealth: {
    hasData: false,
    nodes: [],
    inferenceReadyCount: 0,
    storageUsedBytes: null,
    storageQuotaBytes: null,
    overviewError: null,
    lanError: null,
  },
};

interface DailyBriefStoreState extends DailyBriefData {
  loading: boolean;
  /** Set only when re-deriving from already-loaded store state fails
   * unexpectedly (it shouldn't — every input is a plain, already-validated
   * field) — refreshing each source's own data is best-effort below and
   * never surfaces here, since a single source failing to refresh must not
   * block the rest of the brief from rendering. */
  error: string | null;
  lastRefreshedAtMs: number | null;
  /** Recomputes every section from whatever is currently in the other
   * stores' state, then asks each source to refresh itself (best-effort,
   * independently) and recomputes again — so the panel shows something
   * immediately on open and stays current without inventing any new
   * network/connector call of its own. */
  refresh: () => Promise<void>;
}

function deriveFromStores(): DailyBriefData {
  const { runs } = useRunStore.getState();
  const { entries: automationEntries } = useAutomationsStore.getState();
  const { queue: permissionQueue } = usePermissionStore.getState();
  const { loaded, runtimes, hardware, storage, errors } = useRuntimeHubStore.getState();
  return buildDailyBrief({
    runs,
    automationEntries,
    permissionQueue,
    runtimeHub: { loaded, runtimes, hardware, storage, errors },
  });
}

export const useDailyBriefStore = create<DailyBriefStoreState>((set) => ({
  ...EMPTY_BRIEF,
  loading: false,
  error: null,
  lastRefreshedAtMs: null,

  refresh: async () => {
    set({ loading: true, error: null, ...deriveFromStores() });
    // Each source's own refresh is independent and best-effort: a connector
    // (mcpStore) or the runtime hub being unreachable must not prevent the
    // run/automation/approval sections above from being current.
    const results = await Promise.allSettled([
      useRunStore.getState().refresh(),
      useMcpStore.getState().refresh(),
      useRuntimeHubStore.getState().refresh(),
    ]);
    const firstFailure = results.find((result): result is PromiseRejectedResult => result.status === "rejected");
    set({
      loading: false,
      lastRefreshedAtMs: Date.now(),
      error: firstFailure ? errorMessage(firstFailure.reason) : null,
      ...deriveFromStores(),
    });
  },
}));

function errorMessage(error: unknown): string {
  if (error instanceof Error) return error.message;
  if (typeof error === "string") return error;
  try {
    return JSON.stringify(error);
  } catch {
    return String(error);
  }
}
