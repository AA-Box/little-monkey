/**
 * Agent Inbox aggregation (ROADMAP.md "Agent Inbox and Run Dashboard",
 * Phase 1): pure, store-free functions that normalize every existing
 * run/task/approval source into one `InboxItem[]` list, plus the
 * filter/sort logic the Inbox panel applies to it.
 *
 * Deliberately has no Tauri/zustand imports — every function here is a
 * plain data transform so the aggregation and filtering logic (the part
 * most worth getting right) can be unit tested without mocking the whole
 * app shell. `components/Inbox/AgentInbox.tsx` is the only caller and owns
 * all the store wiring + async enrichment fetching.
 *
 * Today's real sources: the durable run ledger (`runProtocol.ts` — covers
 * interactive chat turns, daemon/background runs, workflow runs, browser
 * runs, crew/comparison runs, and anything tagged `kind: "scheduled"`),
 * cron automations (`automationsStore.ts`), and the live chat-turn
 * permission queue (`permissionStore.ts`). Side tasks
 * (ROADMAP.md "Side Tasks", also Phase 1/Next) have no store yet on this
 * branch — `buildSideTaskInboxItems` is the plug-in slot: it returns `[]`
 * today and documents the shape a future side-task store should hand back.
 */
import type {
  ClientKind,
  PermissionDecision,
  RiskLevel,
  RunEventEnvelopeWire,
  RunKind,
  RunRecord,
  RunStatus,
  UsageSnapshotWire,
} from "./runProtocol";
import type { AutomationEntry, AutomationRunStatus } from "../store/automationsStore";
import type { PermissionRequest } from "../store/permissionStore";

// ---------------------------------------------------------------------------
// Core item shape
// ---------------------------------------------------------------------------

export type InboxSourceKind = "run" | "automation" | "chat_approval" | "side_task";

/** Buckets named after ROADMAP.md's own list ("active, waiting, failed,
 * completed, archived, and scheduled"), plus "cancelled" — a real, distinct
 * `RunStatus` the roadmap's prose doesn't call out by name but that would
 * otherwise get silently folded into "failed" (wrong) or "completed"
 * (worse). */
export type InboxStatus = "active" | "waiting" | "failed" | "completed" | "cancelled" | "archived" | "scheduled";

export type CostBucket = "unknown" | "free" | "under_0_50" | "under_2" | "2_plus";

/** One tool call inside a run's timeline — `tool_proposed` joined with its
 * `tool_started`/`tool_finished` counterparts by `tool_call_id`. */
export interface ToolCallSummary {
  toolCallId: string;
  toolName: string;
  mutation: boolean;
  connectorId: string | null;
  started: boolean;
  outcome: "succeeded" | "failed" | "denied" | "cancelled" | null;
  durationMs: number | null;
  occurredAtMs: number;
}

/** One approval round-trip — `permission_requested` joined with its
 * `permission_decided` (if any) by `request_id`. */
export interface ApprovalSummary {
  requestId: string;
  toolName: string;
  detail: string;
  riskLevel: RiskLevel | null;
  riskReason: string | null;
  requestedAtMs: number;
  expiresAtMs: number;
  operationSha256: string;
  decision: PermissionDecision | null;
  decidedAtMs: number | null;
}

export interface MutationSummary {
  mutationId: string;
  kind: string;
  summary: string;
  preparedAtMs: number;
  confirmedAtMs: number | null;
  confirmationRef: string | null;
}

export interface ArtifactSummary {
  artifactId: string;
  kind: string;
  name: string;
  mediaType: string;
  sizeBytes: number;
  contentSha256: string;
  occurredAtMs: number;
}

export interface VerificationSummary {
  verificationId: string;
  name: string;
  passed: boolean;
  summary: string;
  durationMs: number;
  occurredAtMs: number;
}

/** Everything derived from a run's event stream — only available once the
 * caller has fetched that run's events (see `deriveRunEnrichment`). Rows
 * whose events haven't been loaded yet carry `null` here rather than a
 * fabricated zero/empty value, so "no connector used" and "don't know yet"
 * never look the same in a filter. */
export interface RunEnrichment {
  costMicros: number | null;
  usage: UsageSnapshotWire | null;
  connectors: string[];
  pendingApproval: ApprovalSummary | null;
  approvals: ApprovalSummary[];
  toolCalls: ToolCallSummary[];
  mutations: MutationSummary[];
  artifacts: ArtifactSummary[];
  verifications: VerificationSummary[];
}

export interface InboxItem {
  id: string;
  sourceKind: InboxSourceKind;
  status: InboxStatus;
  title: string;
  subtitle: string;
  createdAtMs: number;
  updatedAtMs: number;
  workspaceId: string | null;
  workspaceLabel: string | null;
  /** A real, dynamically-derived field per source: a run's `RunKind`, an
   * automation's literal `"scheduled"`, or a chat approval's `"interactive"`
   * (chat-turn tool calls are always interactive runs) — never a hardcoded
   * enum the backend can't actually populate. */
  sourceTrigger: string;
  /** Who/what submitted the run (`ClientKind`) — `null` for non-run items. */
  submittedBy: ClientKind | null;
  model: string | null;
  /** `null` until enrichment has run for this item; `[]` once loaded with no
   * connector calls observed. */
  connectors: string[] | null;
  costMicros: number | null;
  riskLevel: RiskLevel | null;
  needsApproval: boolean;
  runId: string | null;
  automationEntryId: string | null;
  approvalRequestId: string | null;
  archivedAtMs: number | null;
  daemonManaged: boolean;
  /** Next cron occurrence, epoch ms — only ever set for `sourceKind ===
   * "automation"` items, computed by the caller via the real `cron_next`
   * Tauri command (see `AgentInbox.tsx`). `null` for every other source and
   * for an automation whose next occurrence hasn't been fetched yet. */
  nextRunAtMs: number | null;
}

const STATUS_PRIORITY: Record<InboxStatus, number> = {
  waiting: 0,
  active: 1,
  failed: 2,
  scheduled: 3,
  completed: 4,
  cancelled: 5,
  archived: 6,
};

const ACTIVE_RUN_STATUSES = new Set<RunStatus>(["queued", "running", "cancelling"]);
const WAITING_RUN_STATUSES = new Set<RunStatus>(["waiting_for_permission", "paused"]);
const FAILED_RUN_STATUSES = new Set<RunStatus>(["failed", "needs_reconciliation"]);

export function runStatusToInboxStatus(run: RunRecord): InboxStatus {
  if (run.archivedAtMs != null) return "archived";
  if (ACTIVE_RUN_STATUSES.has(run.status)) return "active";
  if (WAITING_RUN_STATUSES.has(run.status)) return "waiting";
  if (FAILED_RUN_STATUSES.has(run.status)) return "failed";
  if (run.status === "cancelled") return "cancelled";
  return "completed"; // succeeded
}

// ---------------------------------------------------------------------------
// Connector id derivation (best-effort — see mcpTools.ts's own doc comment:
// a composite `mcp__<server>__<tool>` name is NOT reliably reversible after
// sanitization/dedup, so a parsed id is only trusted when it matches a
// currently-configured server id exactly. `mcp:<server>:<tool>` — the
// permission-request identifier `mcp.rs::mcp_call_tool` builds straight from
// the real server id — needs no such guard.)
// ---------------------------------------------------------------------------

export function connectorIdFromToolName(toolName: string, knownServerIds: readonly string[]): string | null {
  if (toolName.startsWith("mcp:")) {
    const id = toolName.slice(4).split(":")[0];
    return id || null;
  }
  if (toolName.startsWith("mcp__")) {
    const rest = toolName.slice(5);
    const separator = rest.indexOf("__");
    const candidate = separator === -1 ? rest : rest.slice(0, separator);
    return knownServerIds.includes(candidate) ? candidate : null;
  }
  return null;
}

export function costBucketOf(costMicros: number | null): CostBucket {
  if (costMicros == null) return "unknown";
  if (costMicros <= 0) return "free";
  if (costMicros < 500_000) return "under_0_50";
  if (costMicros < 2_000_000) return "under_2";
  return "2_plus";
}

// ---------------------------------------------------------------------------
// Run event enrichment
// ---------------------------------------------------------------------------

/** Walks one run's full event stream once, deriving everything the Inbox's
 * list row + detail timeline need. Mirrors `RunCenter.tsx`'s
 * `pendingApprovals` (kept event-shape-compatible on purpose) but also
 * collects tool calls, mutations, artifacts, verifications, connectors, and
 * the latest cost/usage snapshot. */
export function deriveRunEnrichment(
  events: readonly RunEventEnvelopeWire[],
  knownServerIds: readonly string[],
): RunEnrichment {
  const toolCallsById = new Map<string, ToolCallSummary>();
  const toolOrder: string[] = [];
  const approvalsById = new Map<string, ApprovalSummary>();
  const approvalOrder: string[] = [];
  const mutationsById = new Map<string, MutationSummary>();
  const mutationOrder: string[] = [];
  const artifacts: ArtifactSummary[] = [];
  const verifications: VerificationSummary[] = [];
  const connectors = new Set<string>();
  let usage: UsageSnapshotWire | null = null;

  for (const envelope of events) {
    const event = envelope.event;
    switch (event.type) {
      case "tool_proposed": {
        const connectorId = connectorIdFromToolName(event.payload.tool_name, knownServerIds);
        if (connectorId) connectors.add(connectorId);
        toolCallsById.set(event.payload.tool_call_id, {
          toolCallId: event.payload.tool_call_id,
          toolName: event.payload.tool_name,
          mutation: event.payload.mutation,
          connectorId,
          started: false,
          outcome: null,
          durationMs: null,
          occurredAtMs: envelope.occurred_at_ms,
        });
        toolOrder.push(event.payload.tool_call_id);
        break;
      }
      case "tool_started": {
        const existing = toolCallsById.get(event.payload.tool_call_id);
        if (existing) existing.started = true;
        break;
      }
      case "tool_finished": {
        const existing = toolCallsById.get(event.payload.tool_call_id);
        if (existing) {
          existing.outcome = event.payload.outcome;
          existing.durationMs = event.payload.duration_ms;
        }
        break;
      }
      case "permission_requested": {
        approvalsById.set(event.payload.request_id, {
          requestId: event.payload.request_id,
          toolName: event.payload.tool_name,
          detail: event.payload.detail,
          riskLevel: event.payload.risk_level,
          riskReason: event.payload.risk_reason,
          requestedAtMs: envelope.occurred_at_ms,
          expiresAtMs: event.payload.expires_at_ms,
          operationSha256: event.payload.operation_sha256,
          decision: null,
          decidedAtMs: null,
        });
        approvalOrder.push(event.payload.request_id);
        const connectorId = connectorIdFromToolName(event.payload.tool_name, knownServerIds);
        if (connectorId) connectors.add(connectorId);
        break;
      }
      case "permission_decided": {
        const existing = approvalsById.get(event.payload.request_id);
        if (existing) {
          existing.decision = event.payload.decision;
          existing.decidedAtMs = envelope.occurred_at_ms;
        }
        break;
      }
      case "external_mutation_prepared": {
        mutationsById.set(event.payload.mutation_id, {
          mutationId: event.payload.mutation_id,
          kind: event.payload.kind,
          summary: event.payload.summary,
          preparedAtMs: envelope.occurred_at_ms,
          confirmedAtMs: null,
          confirmationRef: null,
        });
        mutationOrder.push(event.payload.mutation_id);
        break;
      }
      case "external_mutation_confirmed": {
        const existing = mutationsById.get(event.payload.mutation_id);
        if (existing) {
          existing.confirmedAtMs = envelope.occurred_at_ms;
          existing.confirmationRef = event.payload.confirmation_ref;
        }
        break;
      }
      case "artifact_added": {
        artifacts.push({
          artifactId: event.payload.artifact_id,
          kind: event.payload.kind,
          name: event.payload.name,
          mediaType: event.payload.media_type,
          sizeBytes: event.payload.size_bytes,
          contentSha256: event.payload.content_sha256,
          occurredAtMs: envelope.occurred_at_ms,
        });
        break;
      }
      case "verification_finished": {
        verifications.push({
          verificationId: event.payload.verification_id,
          name: event.payload.name,
          passed: event.payload.passed,
          summary: event.payload.summary,
          durationMs: event.payload.duration_ms,
          occurredAtMs: envelope.occurred_at_ms,
        });
        break;
      }
      case "usage_recorded": {
        usage = event.payload.usage;
        break;
      }
      case "completed": {
        usage = event.payload.usage;
        break;
      }
      default:
        break;
    }
  }

  const approvals = approvalOrder.map((id) => approvalsById.get(id)!);
  const pendingApproval = approvals.find((approval) => approval.decision === null) ?? null;

  return {
    costMicros: usage?.cost_micros ?? null,
    usage,
    connectors: [...connectors].sort(),
    pendingApproval,
    approvals,
    toolCalls: toolOrder.map((id) => toolCallsById.get(id)!),
    mutations: mutationOrder.map((id) => mutationsById.get(id)!),
    artifacts,
    verifications,
  };
}

// ---------------------------------------------------------------------------
// Item builders
// ---------------------------------------------------------------------------

function workspaceOf(run: RunRecord): { id: string | null; label: string | null } {
  const workspace = run.spec.workspace;
  if (!workspace) return { id: null, label: null };
  const primary = workspace.roots.find((root) => root.root_id === workspace.primary_root_id);
  return { id: workspace.workspace_id, label: primary?.canonical_path ?? workspace.workspace_id };
}

function modelLabelOf(run: RunRecord): string {
  return run.spec.target.label;
}

export function buildRunInboxItem(
  run: RunRecord,
  enrichment: RunEnrichment | null,
  daemonManagedRunIds: readonly string[],
): InboxItem {
  const status = runStatusToInboxStatus(run);
  const workspace = workspaceOf(run);
  const needsApproval = status === "waiting" && run.status === "waiting_for_permission";
  return {
    id: `run:${run.spec.run_id}`,
    sourceKind: "run",
    status,
    title: run.spec.task || run.spec.run_id,
    subtitle: run.spec.target.label,
    createdAtMs: run.spec.created_at_ms,
    updatedAtMs: run.updatedAtMs,
    workspaceId: workspace.id,
    workspaceLabel: workspace.label,
    sourceTrigger: run.spec.kind,
    submittedBy: run.spec.submitted_by.kind,
    model: modelLabelOf(run),
    connectors: enrichment?.connectors ?? null,
    costMicros: enrichment?.costMicros ?? null,
    riskLevel: enrichment?.pendingApproval?.riskLevel ?? null,
    needsApproval,
    runId: run.spec.run_id,
    automationEntryId: null,
    approvalRequestId: enrichment?.pendingApproval?.requestId ?? null,
    archivedAtMs: run.archivedAtMs,
    daemonManaged: daemonManagedRunIds.includes(run.spec.run_id),
    nextRunAtMs: null,
  };
}

export function buildRunInboxItems(
  runs: readonly RunRecord[],
  enrichmentByRunId: ReadonlyMap<string, RunEnrichment>,
  daemonManagedRunIds: readonly string[],
): InboxItem[] {
  return runs.map((run) => buildRunInboxItem(run, enrichmentByRunId.get(run.spec.run_id) ?? null, daemonManagedRunIds));
}

const AUTOMATION_RUN_STATUS_TO_SUBTITLE: Record<AutomationRunStatus, string> = {
  ok: "last run succeeded",
  error: "last run failed",
  denied: "last run was denied",
};

export function buildAutomationInboxItem(entry: AutomationEntry, nextRunAtMs: number | null): InboxItem {
  const parts = [`cron ${entry.cron}`];
  if (!entry.enabled) parts.push("disabled");
  if (entry.lastStatus) parts.push(AUTOMATION_RUN_STATUS_TO_SUBTITLE[entry.lastStatus]);
  return {
    id: `automation:${entry.id}`,
    sourceKind: "automation",
    status: "scheduled",
    title: entry.recipeName,
    subtitle: parts.join(" · "),
    createdAtMs: entry.lastRunAt ?? 0,
    updatedAtMs: entry.lastRunAt ?? 0,
    workspaceId: null,
    workspaceLabel: null,
    sourceTrigger: "scheduled",
    submittedBy: "scheduler",
    model: null,
    connectors: [],
    costMicros: null,
    riskLevel: null,
    needsApproval: false,
    runId: null,
    automationEntryId: entry.id,
    approvalRequestId: null,
    archivedAtMs: null,
    daemonManaged: false,
    nextRunAtMs,
  };
}

/** `nextRunAtMs` is keyed by `AutomationEntry.id` — the caller (`AgentInbox.tsx`)
 * fetches it via the real `cron_next` Tauri command per entry and passes the
 * results in; entries with no cached value yet get `null`, never a
 * fabricated estimate. */
export function buildAutomationInboxItems(
  entries: readonly AutomationEntry[],
  nextRunAtMsByEntryId: ReadonlyMap<string, number | null> = new Map(),
): InboxItem[] {
  return entries
    .filter((entry) => entry.enabled)
    .map((entry) => buildAutomationInboxItem(entry, nextRunAtMsByEntryId.get(entry.id) ?? null));
}

export function buildChatApprovalInboxItem(request: PermissionRequest, knownServerIds: readonly string[]): InboxItem {
  const now = Date.now();
  const connectorId = connectorIdFromToolName(request.tool, knownServerIds);
  return {
    id: `chat-approval:${request.id}`,
    sourceKind: "chat_approval",
    status: "waiting",
    title: request.agent_label ? `${request.tool} (${request.agent_label})` : request.tool,
    subtitle: request.detail.split("\n", 1)[0] ?? request.detail,
    createdAtMs: now,
    updatedAtMs: now,
    workspaceId: null,
    workspaceLabel: null,
    sourceTrigger: "interactive",
    submittedBy: "desktop",
    model: null,
    connectors: connectorId ? [connectorId] : [],
    costMicros: null,
    riskLevel: request.risk_level ?? null,
    needsApproval: true,
    runId: null,
    automationEntryId: null,
    approvalRequestId: request.id,
    archivedAtMs: null,
    daemonManaged: false,
    nextRunAtMs: null,
  };
}

export function buildChatApprovalInboxItems(queue: readonly PermissionRequest[], knownServerIds: readonly string[]): InboxItem[] {
  return queue.map((request) => buildChatApprovalInboxItem(request, knownServerIds));
}

/**
 * Plug-in slot for ROADMAP.md's "Side Tasks" item, which has no store on
 * this branch yet (its feature branch hasn't merged into develop). Once it
 * ships, replace this stub's body with a call into that store's list of
 * tasks, mapped into `InboxItem`s the same way the builders above do — the
 * rest of the Inbox (filtering, sorting, actions) needs no changes since it
 * already treats `"side_task"` as a first-class `InboxSourceKind`.
 */
export function buildSideTaskInboxItems(): InboxItem[] {
  return [];
}

// ---------------------------------------------------------------------------
// Merge, filter, sort
// ---------------------------------------------------------------------------

export function mergeInboxItems(...groups: InboxItem[][]): InboxItem[] {
  return groups.flat();
}

export function sortInboxItems(items: readonly InboxItem[]): InboxItem[] {
  return [...items].sort((a, b) => {
    const priorityDiff = STATUS_PRIORITY[a.status] - STATUS_PRIORITY[b.status];
    if (priorityDiff !== 0) return priorityDiff;
    return b.updatedAtMs - a.updatedAtMs;
  });
}

export interface InboxFilters {
  search: string;
  workspaceId: string | null;
  sourceTrigger: string | null;
  model: string | null;
  connector: string | null;
  status: InboxStatus | null;
  costBucket: CostBucket | null;
  riskLevel: RiskLevel | "unknown" | null;
}

export const EMPTY_INBOX_FILTERS: InboxFilters = {
  search: "",
  workspaceId: null,
  sourceTrigger: null,
  model: null,
  connector: null,
  status: null,
  costBucket: null,
  riskLevel: null,
};

function matchesSearch(item: InboxItem, query: string): boolean {
  if (!query.trim()) return true;
  const needle = query.trim().toLowerCase();
  return item.title.toLowerCase().includes(needle) || item.subtitle.toLowerCase().includes(needle);
}

export function filterInboxItems(items: readonly InboxItem[], filters: InboxFilters): InboxItem[] {
  return items.filter((item) => {
    if (!matchesSearch(item, filters.search)) return false;
    if (filters.workspaceId && item.workspaceId !== filters.workspaceId) return false;
    if (filters.sourceTrigger && item.sourceTrigger !== filters.sourceTrigger) return false;
    if (filters.model && item.model !== filters.model) return false;
    if (filters.connector && !(item.connectors ?? []).includes(filters.connector)) return false;
    if (filters.status && item.status !== filters.status) return false;
    if (filters.costBucket && costBucketOf(item.costMicros) !== filters.costBucket) return false;
    if (filters.riskLevel) {
      const actual = item.riskLevel ?? "unknown";
      if (actual !== filters.riskLevel) return false;
    }
    return true;
  });
}

export interface InboxFilterOptions {
  workspaces: Array<{ id: string; label: string }>;
  sourceTriggers: string[];
  models: string[];
  connectors: string[];
}

/** Filter dropdown options are always derived from what's actually present
 * in the currently loaded items — never a hardcoded full enum — so a fresh
 * install with only interactive runs shows only "Interactive", not nine
 * empty categories the backend has never populated. */
export function inboxFilterOptions(items: readonly InboxItem[]): InboxFilterOptions {
  const workspaces = new Map<string, string>();
  const sourceTriggers = new Set<string>();
  const models = new Set<string>();
  const connectors = new Set<string>();

  for (const item of items) {
    if (item.workspaceId && item.workspaceLabel) workspaces.set(item.workspaceId, item.workspaceLabel);
    sourceTriggers.add(item.sourceTrigger);
    if (item.model) models.add(item.model);
    for (const connector of item.connectors ?? []) connectors.add(connector);
  }

  return {
    workspaces: [...workspaces.entries()].map(([id, label]) => ({ id, label })).sort((a, b) => a.label.localeCompare(b.label)),
    sourceTriggers: [...sourceTriggers].sort(),
    models: [...models].sort(),
    connectors: [...connectors].sort(),
  };
}

export function needsApprovalCount(items: readonly InboxItem[]): number {
  return items.filter((item) => item.needsApproval).length;
}

export type RunKindForDisplay = RunKind;
