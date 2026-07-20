import { create } from "zustand";
import { invoke } from "@tauri-apps/api/core";

import { useRunStore } from "./runStore";
import { useAutomationsStore, type AutomationEntry, type AutomationRunStatus } from "./automationsStore";
import { usePermissionStore, type PermissionRequest } from "./permissionStore";
import { useMcpStore, type McpServerInfo } from "./mcpStore";
import { useRuntimeHubStore } from "./runtimeHubStore";
import type { RiskLevel, RunRecord, RunStatus } from "../lib/runProtocol";
import type { HardwareSnapshot, M3RuntimeCapability, M3StorageStatus } from "../lib/runtimeHubClient";
import { formatMcpCallToolResult, type McpCallToolResult } from "../lib/mcpTools";
import { neutralizeModelControlTokens } from "../lib/untrustedContent";

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
export interface DailyBriefConnectorSource {
  id: string;
  serverId: string;
  toolName: string;
  label: string;
  arguments: Record<string, unknown>;
  enabled: boolean;
}

export interface ConnectorHighlight {
  id: string;
  connectorId: string;
  label: string;
  summary: string;
  toolName: string;
  fetchedAtMs: number;
  status: "ok" | "error";
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

const CONNECTOR_SOURCE_STORAGE_KEY = "little-monkey-daily-brief-connectors-v1";
const CONNECTOR_SUMMARY_CHARS = 800;
const READ_VERB = /(?:^|[_.:/-])(get|list|search|read|fetch|query|lookup|status|health|mentions|calendar|inbox|checks)(?:$|[_.:/-])/i;
const WRITE_VERB = /(?:^|[_.:/-])(create|add|send|post|write|update|delete|remove|merge|close|approve|deploy|publish|mutate)(?:$|[_.:/-])/i;

/** Daily Brief may only poll obviously read-oriented tools. Unknown names
 * fail closed; enabling a source never becomes a generic connector grant. */
export function isReadOnlyBriefTool(toolName: string): boolean {
  return READ_VERB.test(toolName) && !WRITE_VERB.test(toolName);
}

function sanitizeConnectorSource(value: unknown): DailyBriefConnectorSource | null {
  if (!value || typeof value !== "object" || Array.isArray(value)) return null;
  const source = value as Partial<DailyBriefConnectorSource>;
  if (
    typeof source.id !== "string" || !source.id
    || typeof source.serverId !== "string" || !source.serverId
    || typeof source.toolName !== "string" || !isReadOnlyBriefTool(source.toolName)
    || typeof source.label !== "string" || !source.label.trim()
    || !source.arguments || typeof source.arguments !== "object" || Array.isArray(source.arguments)
    || typeof source.enabled !== "boolean"
  ) return null;
  return {
    id: source.id.slice(0, 120),
    serverId: source.serverId.slice(0, 120),
    toolName: source.toolName.slice(0, 200),
    label: source.label.trim().slice(0, 160),
    arguments: structuredClone(source.arguments as Record<string, unknown>),
    enabled: source.enabled,
  };
}

export function loadDailyBriefConnectorSources(): DailyBriefConnectorSource[] {
  try {
    const raw = localStorage.getItem(CONNECTOR_SOURCE_STORAGE_KEY);
    const parsed: unknown = raw ? JSON.parse(raw) : [];
    if (!Array.isArray(parsed)) return [];
    return parsed
      .slice(0, 32)
      .map(sanitizeConnectorSource)
      .filter((source): source is DailyBriefConnectorSource => source !== null);
  } catch {
    return [];
  }
}

function persistDailyBriefConnectorSources(sources: readonly DailyBriefConnectorSource[]): void {
  try {
    localStorage.setItem(CONNECTOR_SOURCE_STORAGE_KEY, JSON.stringify(sources));
  } catch {
    // The brief still works for this session when storage is unavailable.
  }
}

function boundedConnectorSummary(value: string): string {
  const normalized = neutralizeModelControlTokens(value).replace(/\s+/g, " ").trim();
  if (!normalized) return "The connector returned no text content.";
  return normalized.length <= CONNECTOR_SUMMARY_CHARS
    ? normalized
    : `${normalized.slice(0, CONNECTOR_SUMMARY_CHARS)}…`;
}

export type DailyBriefMcpCaller = (
  serverId: string,
  toolName: string,
  args: Record<string, unknown>,
  turnId: string,
  toolCallId: string,
) => Promise<McpCallToolResult>;

const invokeBriefMcpTool: DailyBriefMcpCaller = (serverId, toolName, args, turnId, toolCallId) =>
  invoke<McpCallToolResult>("mcp_call_tool", {
    server_id: serverId,
    tool_name: toolName,
    arguments: args,
    turn_id: turnId,
    tool_call_id: toolCallId,
  });

/** Queries only sources that the user enabled and whose server is both
 * enabled and connected. Every configured tool is rechecked against the live
 * schema/allowlist and the read-only name policy before invocation. */
export async function queryConnectorHighlights(
  sources: readonly DailyBriefConnectorSource[],
  servers: readonly McpServerInfo[],
  call: DailyBriefMcpCaller = invokeBriefMcpTool,
  nowMs: number = Date.now(),
): Promise<ConnectorHighlight[]> {
  const highlights: ConnectorHighlight[] = [];
  for (const source of sources) {
    if (!source.enabled || !isReadOnlyBriefTool(source.toolName)) continue;
    const server = servers.find((candidate) => candidate.id === source.serverId);
    if (!server || !server.enabled || server.status !== "connected") continue;
    if (server.toolAllowlist && !server.toolAllowlist.includes(source.toolName)) continue;
    if (!server.tools.some((tool) => tool.name === source.toolName)) continue;

    try {
      const result = await call(
        source.serverId,
        source.toolName,
        structuredClone(source.arguments),
        `daily-brief:${source.id}:${crypto.randomUUID()}`,
        `daily-brief-call:${crypto.randomUUID()}`,
      );
      const formatted = formatMcpCallToolResult(result);
      highlights.push({
        id: source.id,
        connectorId: source.serverId,
        label: source.label,
        summary: boundedConnectorSummary(formatted),
        toolName: source.toolName,
        fetchedAtMs: nowMs,
        status: result.isError ? "error" : "ok",
      });
    } catch (error) {
      highlights.push({
        id: source.id,
        connectorId: source.serverId,
        label: source.label,
        summary: boundedConnectorSummary(errorMessage(error)),
        toolName: source.toolName,
        fetchedAtMs: nowMs,
        status: "error",
      });
    }
  }
  return highlights;
}

/** Stable newest-first presentation helper used by aggregate tests and by
 * persisted/cached callers that already have connector evidence. */
export function buildConnectorHighlights(
  highlights: readonly ConnectorHighlight[] = [],
): ConnectorHighlight[] {
  return [...highlights].sort((left, right) => right.fetchedAtMs - left.fetchedAtMs);
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
  connectorSources: DailyBriefConnectorSource[];
  loading: boolean;
  /** Set only when re-deriving from already-loaded store state fails
   * unexpectedly (it shouldn't — every input is a plain, already-validated
   * field) — refreshing each source's own data is best-effort below and
   * never surfaces here, since a single source failing to refresh must not
   * block the rest of the brief from rendering. */
  error: string | null;
  lastRefreshedAtMs: number | null;
  saveConnectorSource: (source: Omit<DailyBriefConnectorSource, "id"> & { id?: string }) => void;
  removeConnectorSource: (id: string) => void;
  setConnectorSourceEnabled: (id: string, enabled: boolean) => void;
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
  connectorSources: loadDailyBriefConnectorSources(),
  loading: false,
  error: null,
  lastRefreshedAtMs: null,

  saveConnectorSource: (candidate) => {
    const source = sanitizeConnectorSource({
      ...candidate,
      id: candidate.id || crypto.randomUUID(),
    });
    if (!source) throw new Error("Daily Brief connector source is invalid or is not a read-only tool.");
    set((state) => {
      const connectorSources = state.connectorSources.some((entry) => entry.id === source.id)
        ? state.connectorSources.map((entry) => entry.id === source.id ? source : entry)
        : [...state.connectorSources, source];
      persistDailyBriefConnectorSources(connectorSources);
      return { connectorSources };
    });
  },

  removeConnectorSource: (id) => {
    set((state) => {
      const connectorSources = state.connectorSources.filter((source) => source.id !== id);
      persistDailyBriefConnectorSources(connectorSources);
      return {
        connectorSources,
        connectorHighlights: state.connectorHighlights.filter((highlight) => highlight.id !== id),
      };
    });
  },

  setConnectorSourceEnabled: (id, enabled) => {
    set((state) => {
      const connectorSources = state.connectorSources.map((source) =>
        source.id === id ? { ...source, enabled } : source
      );
      persistDailyBriefConnectorSources(connectorSources);
      return {
        connectorSources,
        ...(!enabled
          ? { connectorHighlights: state.connectorHighlights.filter((highlight) => highlight.id !== id) }
          : {}),
      };
    });
  },

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
    const connectorHighlights = await queryConnectorHighlights(
      useDailyBriefStore.getState().connectorSources,
      useMcpStore.getState().servers,
    );
    set({
      loading: false,
      lastRefreshedAtMs: Date.now(),
      error: firstFailure ? errorMessage(firstFailure.reason) : null,
      ...deriveFromStores(),
      connectorHighlights: buildConnectorHighlights(connectorHighlights),
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
