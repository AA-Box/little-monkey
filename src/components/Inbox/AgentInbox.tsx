import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import {
  Archive,
  ArchiveRestore,
  ExternalLink,
  Filter,
  ListTodo,
  Pause,
  Play,
  RefreshCw,
  RotateCcw,
  Search,
  Square,
  X,
} from "lucide-react";

import { useT } from "../../lib/i18n";
import {
  decideRunPermission,
  loadRunEvents,
  requestRunCancellation,
  type PermissionDecision,
  type RiskLevel,
} from "../../lib/runProtocol";
import {
  daemonCancel,
  daemonPause,
  daemonResume,
  daemonRetry,
  daemonStatus,
  isDaemonManagedRun,
  type DaemonStatus,
} from "../../lib/daemonClient";
import { runRecipeNow } from "../../lib/recipeRunner";
import {
  buildAutomationInboxItems,
  buildChatApprovalInboxItems,
  buildRunInboxItems,
  buildSideTaskInboxItems,
  deriveRunEnrichment,
  filterInboxItems,
  inboxFilterOptions,
  mergeInboxItems,
  needsApprovalCount,
  sortInboxItems,
  EMPTY_INBOX_FILTERS,
  type CostBucket,
  type InboxFilters,
  type InboxItem,
  type InboxStatus,
  type RunEnrichment,
} from "../../lib/inbox";
import { initializeRunStore, useRunStore } from "../../store/runStore";
import { useAutomationsStore } from "../../store/automationsStore";
import { usePermissionStore } from "../../store/permissionStore";
import { useMcpStore } from "../../store/mcpStore";
import { useRecipeStore } from "../../store/recipeStore";
import {
  buildMcpResultSideTaskSeed,
  useSideTaskStore,
  type SideTaskRecord,
} from "../../store/sideTaskStore";
import { useSessionStore } from "../../store/sessionStore";
import { Button, IconButton, StatusPill, Tabs, type PillTone } from "../ui";
import { SideTaskDetail } from "../SideTasks";

interface AgentInboxProps {
  onClose: () => void;
  /** Deep-links to the existing Run Center's raw event log for a run — the
   * Inbox's own detail pane renders a structured timeline, but power users
   * auditing something unusual can still drop to the raw ledger dump. */
  onOpenRunCenter: (runId: string) => void;
}

const STATUS_TABS: Array<InboxStatus | "all"> = [
  "all",
  "active",
  "waiting",
  "failed",
  "scheduled",
  "completed",
  "cancelled",
  "archived",
];

const STATUS_TONE: Record<InboxStatus, PillTone> = {
  active: "neutral",
  waiting: "warning",
  failed: "danger",
  completed: "success",
  cancelled: "neutral",
  archived: "neutral",
  scheduled: "neutral",
};

const RISK_TONE: Record<RiskLevel, PillTone> = {
  low: "success",
  medium: "warning",
  high: "danger",
};

const COST_BUCKETS: CostBucket[] = ["unknown", "free", "under_0_50", "under_2", "2_plus"];

function formatTime(value: number): string {
  if (!value) return "—";
  return new Intl.DateTimeFormat(undefined, { dateStyle: "medium", timeStyle: "short" }).format(value);
}

function formatCost(costMicros: number | null): string | null {
  if (costMicros == null) return null;
  return (costMicros / 1_000_000).toFixed(2);
}

/**
 * Non-terminal run statuses whose events are worth eagerly fetching — these
 * are exactly the rows that can answer "what needs me right now", so their
 * cost/risk/connector/approval data shouldn't wait on the user clicking in.
 * Terminal runs are enriched lazily (see `ensureEnrichment`'s callers): on
 * selection, or in bulk once a cost/risk/connector filter is applied.
 */
function isHighPriorityRun(item: InboxItem): boolean {
  return item.sourceKind === "run" && (item.status === "active" || item.status === "waiting");
}

function InboxRow({ item, selected, onSelect }: { item: InboxItem; selected: boolean; onSelect: () => void }) {
  const { t } = useT();
  const cost = formatCost(item.costMicros);
  return (
    <button
      type="button"
      onClick={onSelect}
      aria-current={selected ? "true" : undefined}
      className={`w-full border-b border-border px-3 py-3 text-left transition-colors focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-inset focus-visible:ring-accent ${
        selected ? "bg-surface-2" : "hover:bg-surface-2/60"
      }`}
    >
      <div className="flex items-start justify-between gap-2">
        <span className="min-w-0 flex-1 truncate text-sm font-medium text-foreground">{item.title}</span>
        <div className="flex shrink-0 items-center gap-1">
          {item.needsApproval && <StatusPill tone="warning">{t("AgentInbox.needsApproval")}</StatusPill>}
          <StatusPill tone={STATUS_TONE[item.status]}>{t(`AgentInbox.status.${item.status}`)}</StatusPill>
        </div>
      </div>
      <p className="mt-1 truncate text-xs text-muted">{item.subtitle}</p>
      <div className="mt-2 flex flex-wrap items-center gap-2 text-[11px] text-faint">
        <span className="rounded bg-surface-2 px-1.5 py-0.5">{t(`AgentInbox.source.${item.sourceKind}`)}</span>
        {item.workspaceLabel && <span className="truncate">{item.workspaceLabel}</span>}
        {item.riskLevel && <StatusPill tone={RISK_TONE[item.riskLevel]}>{t(`AgentInbox.risk.${item.riskLevel}`)}</StatusPill>}
        {cost && <span>${cost}</span>}
        <time className="ml-auto shrink-0" dateTime={new Date(item.updatedAtMs || item.createdAtMs).toISOString()}>
          {formatTime(item.updatedAtMs || item.createdAtMs)}
        </time>
      </div>
    </button>
  );
}

function ToolCallRow({
  call,
  t,
  onStartSideTask,
}: {
  call: RunEnrichment["toolCalls"][number];
  t: ReturnType<typeof useT>["t"];
  onStartSideTask?: () => void;
}) {
  const tone: PillTone = call.outcome === "succeeded" ? "success" : call.outcome === "failed" || call.outcome === "denied" ? "danger" : call.outcome === "cancelled" ? "neutral" : "warning";
  return (
    <li className="rounded-md border border-border bg-surface px-3 py-2 text-xs">
      <div className="flex items-center justify-between gap-3">
        <div className="min-w-0">
          <p className="truncate font-mono text-foreground">{call.toolName}</p>
          <p className="mt-0.5 text-[11px] text-faint">
            {call.connectorId ? `${call.connectorId} · ` : ""}
            {call.mutation ? "mutates" : "read-only"}
            {call.durationMs != null ? ` · ${call.durationMs}ms` : ""}
          </p>
        </div>
        <div className="flex shrink-0 items-center gap-1.5">
          {onStartSideTask && (
            <Button size="sm" variant="ghost" onClick={onStartSideTask} title="Review this MCP result in a side task">
              <ListTodo size={12} /> Side task
            </Button>
          )}
          <StatusPill tone={tone}>{call.outcome ? t(`AgentInbox.detail.outcome.${call.outcome}`) : t("AgentInbox.detail.pending")}</StatusPill>
        </div>
      </div>
      {call.outputExcerpt && (
        <pre className="mt-2 max-h-36 overflow-auto whitespace-pre-wrap break-words rounded bg-surface-2 p-2 font-mono text-[11px] text-muted">
          {call.outputExcerpt}
        </pre>
      )}
    </li>
  );
}

function ApprovalRow({ approval, t }: { approval: RunEnrichment["approvals"][number]; t: ReturnType<typeof useT>["t"] }) {
  const tone: PillTone = approval.decision === "deny" ? "danger" : approval.decision ? "success" : "warning";
  return (
    <li className="rounded-md border border-border bg-surface px-3 py-2 text-xs">
      <div className="flex items-start justify-between gap-3">
        <p className="font-medium text-foreground">{approval.toolName}</p>
        {approval.riskLevel && <StatusPill tone={RISK_TONE[approval.riskLevel]}>{t(`AgentInbox.risk.${approval.riskLevel}`)}</StatusPill>}
      </div>
      <p className="mt-1 whitespace-pre-wrap break-all text-[11px] text-muted">{approval.detail}</p>
      <p className="mt-1 text-[11px] text-faint">
        {t("AgentInbox.detail.decision")}: <StatusPill tone={tone}>{approval.decision ?? t("AgentInbox.detail.pending")}</StatusPill>
      </p>
    </li>
  );
}

interface DetailPaneProps {
  item: InboxItem;
  enrichment: RunEnrichment | null;
  enrichmentLoading: boolean;
  enrichmentError: string | null;
  daemonManagedRunIds: string[];
  automationEntries: ReturnType<typeof useAutomationsStore.getState>["entries"];
  chatApprovalQueue: ReturnType<typeof usePermissionStore.getState>["queue"];
  sideTasks: readonly SideTaskRecord[];
  isHeadOfChatQueue: boolean;
  actionBusy: string | null;
  onDecideRunApproval: (requestId: string, operationSha256: string, decision: PermissionDecision) => void;
  onDecideChatApproval: (allow: boolean, remember: boolean) => void;
  onCancelRun: () => void;
  onDaemonControl: (action: "pause" | "resume" | "retry") => void;
  onArchive: () => void;
  onUnarchive: () => void;
  onRunAutomationNow: () => void;
  onToggleAutomation: () => void;
  onOpenRunCenter: (runId: string) => void;
  onRevealSideTask: () => void;
  onStartSideTaskFromMcpResult: (call: RunEnrichment["toolCalls"][number]) => void;
}

const TERMINAL_RUN_STATUSES = new Set(["completed", "failed", "cancelled", "archived"]);

function DetailPane(props: DetailPaneProps) {
  const { t } = useT();
  const { item } = props;

  if (item.sourceKind === "chat_approval") {
    const request = props.chatApprovalQueue.find((r) => r.id === item.approvalRequestId) ?? null;
    return (
      <div className="mx-auto max-w-3xl space-y-4 p-5">
        <h2 className="text-lg font-semibold text-foreground">{item.title}</h2>
        <p className="whitespace-pre-wrap break-all rounded-md border border-border bg-surface p-3 font-mono text-xs text-muted">
          {item.subtitle}
        </p>
        {item.riskLevel && (
          <StatusPill tone={RISK_TONE[item.riskLevel]}>{t(`AgentInbox.risk.${item.riskLevel}`)}</StatusPill>
        )}
        {!request ? null : !props.isHeadOfChatQueue ? (
          <p className="rounded-md border border-warning/40 bg-warning-soft px-3 py-2 text-xs text-warning">
            {t("AgentInbox.queuedBehindAnother")}
          </p>
        ) : (
          <div className="flex flex-wrap gap-2">
            <Button size="sm" variant="primary" disabled={props.actionBusy !== null} onClick={() => props.onDecideChatApproval(true, false)}>
              {t("AgentInbox.allowOnce")}
            </Button>
            {request.tool !== "run_shell" && (
              <Button size="sm" disabled={props.actionBusy !== null} onClick={() => props.onDecideChatApproval(true, true)}>
                {t("AgentInbox.allowForSession")}
              </Button>
            )}
            <Button size="sm" variant="danger" disabled={props.actionBusy !== null} onClick={() => props.onDecideChatApproval(false, false)}>
              {t("AgentInbox.deny")}
            </Button>
          </div>
        )}
      </div>
    );
  }

  if (item.sourceKind === "automation") {
    const entry = props.automationEntries.find((e) => e.id === item.automationEntryId) ?? null;
    return (
      <div className="mx-auto max-w-3xl space-y-4 p-5">
        <h2 className="text-lg font-semibold text-foreground">{item.title}</h2>
        <p className="text-sm text-muted">{item.subtitle}</p>
        {item.nextRunAtMs != null && (
          <p className="text-xs text-faint">{t("AgentInbox.nextRun")}: {formatTime(item.nextRunAtMs)}</p>
        )}
        <div className="flex flex-wrap gap-2">
          <Button size="sm" variant="primary" disabled={props.actionBusy !== null} onClick={props.onRunAutomationNow}>
            <Play size={12} /> {t("AgentInbox.runNow")}
          </Button>
          {entry && (
            <Button size="sm" disabled={props.actionBusy !== null} onClick={props.onToggleAutomation}>
              {entry.enabled ? t("AgentInbox.disable") : t("AgentInbox.enable")}
            </Button>
          )}
        </div>
      </div>
    );
  }

  if (item.sourceKind === "side_task") {
    const task = props.sideTasks.find((candidate) => candidate.id === item.sideTaskId) ?? null;
    if (!task) {
      return <p className="p-6 text-sm text-faint">This side task is no longer available.</p>;
    }
    return (
      <div className="mx-auto max-w-4xl p-5">
        <div className="mb-2 flex justify-end">
          <Button size="sm" variant="primary" onClick={props.onRevealSideTask}>
            <ExternalLink size={12} /> Open in Side Tasks
          </Button>
        </div>
        <SideTaskDetail task={task} />
      </div>
    );
  }

  // sourceKind === "run"
  const daemonManaged = item.runId ? isDaemonManagedRun(item.runId, props.daemonManagedRunIds) : false;
  const isTerminal = TERMINAL_RUN_STATUSES.has(item.status);
  const enrichment = props.enrichment;
  const usage = enrichment?.usage;
  const cost = formatCost(item.costMicros);

  return (
    <div className="mx-auto max-w-4xl space-y-5 p-5">
      <div className="flex items-start justify-between gap-4">
        <div className="min-w-0">
          <h2 className="text-lg font-semibold text-foreground">{item.title}</h2>
          {item.runId && <p className="mt-1 break-all font-mono text-[11px] text-faint">{item.runId}</p>}
        </div>
        <div className="flex shrink-0 flex-wrap items-center justify-end gap-2">
          <StatusPill tone={STATUS_TONE[item.status]}>{t(`AgentInbox.status.${item.status}`)}</StatusPill>
          {item.runId && (
            <IconButton size="sm" aria-label={t("AgentInbox.openRunCenter")} onClick={() => props.onOpenRunCenter(item.runId!)}>
              <ExternalLink size={14} />
            </IconButton>
          )}
        </div>
      </div>

      <dl className="grid grid-cols-2 gap-3 rounded-lg border border-border bg-surface p-3 text-xs sm:grid-cols-4">
        <div><dt className="text-faint">{t("AgentInbox.detail.model")}</dt><dd className="mt-1 truncate font-medium">{item.model ?? "—"}</dd></div>
        <div><dt className="text-faint">{t("AgentInbox.detail.workspace")}</dt><dd className="mt-1 truncate font-medium">{item.workspaceLabel ?? "—"}</dd></div>
        <div><dt className="text-faint">{t("AgentInbox.detail.created")}</dt><dd className="mt-1 font-medium">{formatTime(item.createdAtMs)}</dd></div>
        <div><dt className="text-faint">{t("AgentInbox.detail.updated")}</dt><dd className="mt-1 font-medium">{formatTime(item.updatedAtMs)}</dd></div>
      </dl>

      <div className="flex flex-wrap gap-2">
        {daemonManaged && item.status === "waiting" && (
          <Button size="sm" disabled={props.actionBusy !== null} onClick={() => props.onDaemonControl("resume")}>
            <Play size={12} /> {t("AgentInbox.resume")}
          </Button>
        )}
        {daemonManaged && (item.status === "active" || item.status === "waiting") && (
          <Button size="sm" disabled={props.actionBusy !== null} onClick={() => props.onDaemonControl("pause")}>
            <Pause size={12} /> {t("AgentInbox.pause")}
          </Button>
        )}
        {daemonManaged && (item.status === "failed" || item.status === "cancelled" || item.status === "completed") && (
          <Button size="sm" disabled={props.actionBusy !== null} onClick={() => props.onDaemonControl("retry")}>
            <RotateCcw size={12} /> {t("AgentInbox.retry")}
          </Button>
        )}
        {(item.status === "active" || item.status === "waiting") && (
          <Button variant="danger" size="sm" disabled={props.actionBusy !== null} onClick={props.onCancelRun}>
            <Square size={12} /> {t("AgentInbox.cancel")}
          </Button>
        )}
        {isTerminal && item.status !== "archived" && (
          <Button size="sm" disabled={props.actionBusy !== null} onClick={props.onArchive}>
            <Archive size={12} /> {t("AgentInbox.archive")}
          </Button>
        )}
        {item.status === "archived" && (
          <Button size="sm" disabled={props.actionBusy !== null} onClick={props.onUnarchive}>
            <ArchiveRestore size={12} /> {t("AgentInbox.unarchive")}
          </Button>
        )}
      </div>

      {props.enrichmentLoading && !enrichment && (
        <p className="text-sm text-faint">{t("AgentInbox.detail.loadingEvents")}</p>
      )}
      {props.enrichmentError && (
        <p className="text-sm text-danger">{t("AgentInbox.detail.eventsError", { error: props.enrichmentError })}</p>
      )}

      {enrichment && enrichment.pendingApproval && (
        <section aria-labelledby="inbox-pending-approval-title">
          <h3 id="inbox-pending-approval-title" className="text-sm font-semibold">{t("AgentInbox.needsApproval")}</h3>
          <article className="mt-2 rounded-lg border border-warning/40 bg-warning-soft p-3 text-sm">
            <div className="flex items-start justify-between gap-3">
              <div>
                <p className="font-medium">{enrichment.pendingApproval.toolName}</p>
                <p className="mt-1 text-xs text-muted">{enrichment.pendingApproval.detail}</p>
              </div>
              {enrichment.pendingApproval.riskLevel && (
                <StatusPill tone={RISK_TONE[enrichment.pendingApproval.riskLevel]}>{t(`AgentInbox.risk.${enrichment.pendingApproval.riskLevel}`)}</StatusPill>
              )}
            </div>
            <p className="mt-2 font-mono text-[10px] text-faint" title={enrichment.pendingApproval.operationSha256}>
              {t("AgentInbox.detail.digest")}: {enrichment.pendingApproval.operationSha256.slice(0, 16)}…
            </p>
            <p className="mt-1 text-[11px] text-faint">{t("AgentInbox.detail.expires")}: {formatTime(enrichment.pendingApproval.expiresAtMs)}</p>
            <div className="mt-3 flex flex-wrap gap-2">
              {Date.now() >= enrichment.pendingApproval.expiresAtMs ? (
                <Button size="sm" disabled={props.actionBusy !== null} onClick={() => props.onDecideRunApproval(enrichment.pendingApproval!.requestId, enrichment.pendingApproval!.operationSha256, "expired")}>
                  {t("AgentInbox.markExpired")}
                </Button>
              ) : (
                <>
                  <Button size="sm" variant="primary" disabled={props.actionBusy !== null} onClick={() => props.onDecideRunApproval(enrichment.pendingApproval!.requestId, enrichment.pendingApproval!.operationSha256, "allow_once")}>
                    {t("AgentInbox.allowOnce")}
                  </Button>
                  <Button size="sm" disabled={props.actionBusy !== null} onClick={() => props.onDecideRunApproval(enrichment.pendingApproval!.requestId, enrichment.pendingApproval!.operationSha256, "allow_for_run")}>
                    {t("AgentInbox.allowForRun")}
                  </Button>
                  <Button size="sm" variant="danger" disabled={props.actionBusy !== null} onClick={() => props.onDecideRunApproval(enrichment.pendingApproval!.requestId, enrichment.pendingApproval!.operationSha256, "deny")}>
                    {t("AgentInbox.deny")}
                  </Button>
                </>
              )}
            </div>
          </article>
        </section>
      )}

      {usage && (
        <section>
          <h3 className="text-sm font-semibold">{t("AgentInbox.detail.usage")}</h3>
          <p className="mt-1 text-xs text-muted">
            {t("AgentInbox.detail.usageTokens", { input: usage.input_tokens, output: usage.output_tokens, calls: usage.model_calls })}
            {cost ? ` · ${t("AgentInbox.detail.usageCost", { amount: cost })}` : ""}
          </p>
        </section>
      )}

      {enrichment && (
        <section aria-labelledby="inbox-connectors-title">
          <h3 id="inbox-connectors-title" className="text-sm font-semibold">{t("AgentInbox.detail.connectors")}</h3>
          {enrichment.connectors.length === 0 ? (
            <p className="mt-1 text-xs text-faint">{t("AgentInbox.detail.noConnectors")}</p>
          ) : (
            <div className="mt-1 flex flex-wrap gap-1.5">
              {enrichment.connectors.map((connector) => (
                <StatusPill key={connector} tone="neutral">{connector}</StatusPill>
              ))}
            </div>
          )}
        </section>
      )}

      {enrichment && (
        <section aria-labelledby="inbox-tools-title">
          <h3 id="inbox-tools-title" className="text-sm font-semibold">{t("AgentInbox.detail.tools")}</h3>
          {enrichment.toolCalls.length === 0 ? (
            <p className="mt-1 text-xs text-faint">{t("AgentInbox.detail.noTools")}</p>
          ) : (
            <ul className="mt-2 space-y-1.5">
              {enrichment.toolCalls.map((call) => (
                <ToolCallRow
                  key={call.toolCallId}
                  call={call}
                  t={t}
                  onStartSideTask={call.connectorId && call.outputExcerpt?.trim()
                    ? () => props.onStartSideTaskFromMcpResult(call)
                    : undefined}
                />
              ))}
            </ul>
          )}
        </section>
      )}

      {enrichment && (
        <section aria-labelledby="inbox-approvals-title">
          <h3 id="inbox-approvals-title" className="text-sm font-semibold">{t("AgentInbox.detail.approvals")}</h3>
          {enrichment.approvals.length === 0 ? (
            <p className="mt-1 text-xs text-faint">{t("AgentInbox.detail.noApprovals")}</p>
          ) : (
            <ul className="mt-2 space-y-1.5">
              {enrichment.approvals.map((approval) => <ApprovalRow key={approval.requestId} approval={approval} t={t} />)}
            </ul>
          )}
        </section>
      )}

      {enrichment && (
        <section aria-labelledby="inbox-mutations-title">
          <h3 id="inbox-mutations-title" className="text-sm font-semibold">{t("AgentInbox.detail.mutations")}</h3>
          {enrichment.mutations.length === 0 && enrichment.artifacts.length === 0 ? (
            <p className="mt-1 text-xs text-faint">{t("AgentInbox.detail.noMutations")}</p>
          ) : (
            <ul className="mt-2 space-y-1.5">
              {enrichment.mutations.map((mutation) => (
                <li key={mutation.mutationId} className="rounded-md border border-border bg-surface px-3 py-2 text-xs">
                  <p className="font-medium text-foreground">{mutation.kind}</p>
                  <p className="mt-0.5 text-[11px] text-muted">{mutation.summary}</p>
                  <p className="mt-1 text-[11px] text-faint">
                    {mutation.confirmedAtMs != null ? t("AgentInbox.detail.mutationConfirmed") : t("AgentInbox.detail.mutationPending")}
                  </p>
                </li>
              ))}
              {enrichment.artifacts.map((artifact) => (
                <li key={artifact.artifactId} className="rounded-md border border-border bg-surface px-3 py-2 text-xs">
                  <p className="truncate font-medium text-foreground">{artifact.name}</p>
                  <p className="mt-0.5 text-[11px] text-faint">{artifact.kind} · {artifact.mediaType} · {artifact.sizeBytes.toLocaleString()} bytes</p>
                </li>
              ))}
            </ul>
          )}
        </section>
      )}

      {enrichment && (
        <section aria-labelledby="inbox-verification-title">
          <h3 id="inbox-verification-title" className="text-sm font-semibold">{t("AgentInbox.detail.verification")}</h3>
          {enrichment.verifications.length === 0 ? (
            <p className="mt-1 text-xs text-faint">{t("AgentInbox.detail.noVerification")}</p>
          ) : (
            <ul className="mt-2 space-y-1.5">
              {enrichment.verifications.map((verification) => (
                <li key={verification.verificationId} className="flex items-center justify-between gap-3 rounded-md border border-border bg-surface px-3 py-2 text-xs">
                  <div className="min-w-0">
                    <p className="truncate font-medium text-foreground">{verification.name}</p>
                    <p className="mt-0.5 truncate text-[11px] text-faint">{verification.summary}</p>
                  </div>
                  <StatusPill tone={verification.passed ? "success" : "danger"}>
                    {verification.passed ? t("AgentInbox.detail.outcome.succeeded") : t("AgentInbox.detail.outcome.failed")}
                  </StatusPill>
                </li>
              ))}
            </ul>
          )}
        </section>
      )}
    </div>
  );
}

export function AgentInbox({ onClose, onOpenRunCenter }: AgentInboxProps) {
  const { t } = useT();
  const runs = useRunStore((state) => state.runs);
  const runsLoading = useRunStore((state) => state.loading);
  const runsShowArchived = useRunStore((state) => state.showArchived);
  const setRunsShowArchived = useRunStore((state) => state.setShowArchived);
  const refreshRuns = useRunStore((state) => state.refresh);
  const archiveRunAction = useRunStore((state) => state.archiveRun);
  const unarchiveRunAction = useRunStore((state) => state.unarchiveRun);

  const automationEntries = useAutomationsStore((state) => state.entries);

  const chatApprovalQueue = usePermissionStore((state) => state.queue);
  const respondToChatApproval = usePermissionStore((state) => state.respond);

  const mcpServers = useMcpStore((state) => state.servers);
  const knownServerIds = useMemo(() => mcpServers.map((server) => server.id), [mcpServers]);

  const recipes = useRecipeStore((state) => state.recipes);

  const sideTaskById = useSideTaskStore((state) => state.tasks);
  const sideTaskOrder = useSideTaskStore((state) => state.order);
  const sideTasks = useMemo(
    () => sideTaskOrder.map((id) => sideTaskById[id]).filter((task): task is SideTaskRecord => Boolean(task)),
    [sideTaskById, sideTaskOrder],
  );
  const activeChatSessionId = useSessionStore((state) => state.activeSessionId);

  const [selectedId, setSelectedId] = useState<string | null>(null);
  const [statusTab, setStatusTab] = useState<InboxStatus | "all">("all");
  const [needsApprovalOnly, setNeedsApprovalOnly] = useState(false);
  const [filtersOpen, setFiltersOpen] = useState(false);
  const [filters, setFilters] = useState<InboxFilters>(EMPTY_INBOX_FILTERS);
  const [actionBusy, setActionBusy] = useState<string | null>(null);
  const [actionError, setActionError] = useState<string | null>(null);
  const [daemon, setDaemon] = useState<DaemonStatus | null>(null);

  const enrichmentCache = useRef(new Map<string, { updatedAtMs: number; data: RunEnrichment }>());
  const enrichmentInFlight = useRef(new Set<string>());
  const enrichmentErrors = useRef(new Map<string, string>());
  const [enrichmentVersion, setEnrichmentVersion] = useState(0);
  const nextRunCache = useRef(new Map<string, number | null>());
  const [nextRunVersion, setNextRunVersion] = useState(0);

  useEffect(() => {
    void initializeRunStore();
  }, []);

  // The Inbox's whole point is showing archived runs too (its own "Archived"
  // tab) — flip the shared run-list toggle on once so `runs` actually
  // contains them. Reused rather than duplicated: Run Center reads the same
  // toggle, so this is a shared, persisted preference, not Inbox-only state.
  useEffect(() => {
    if (!runsShowArchived) void setRunsShowArchived(true);
  }, [runsShowArchived, setRunsShowArchived]);

  const refreshDaemon = useCallback(() => {
    void daemonStatus().then(setDaemon).catch(() => setDaemon(null));
  }, []);

  useEffect(() => {
    refreshDaemon();
    const interval = window.setInterval(refreshDaemon, 15_000);
    return () => window.clearInterval(interval);
  }, [refreshDaemon]);

  const requestEnrichment = useCallback((runId: string, updatedAtMs: number) => {
    const cached = enrichmentCache.current.get(runId);
    if (cached && cached.updatedAtMs === updatedAtMs) return;
    if (enrichmentInFlight.current.has(runId)) return;
    enrichmentInFlight.current.add(runId);
    loadRunEvents(runId)
      .then((events) => {
        enrichmentCache.current.set(runId, { updatedAtMs, data: deriveRunEnrichment(events, knownServerIds) });
        enrichmentErrors.current.delete(runId);
      })
      .catch((error: unknown) => {
        enrichmentErrors.current.set(runId, error instanceof Error ? error.message : String(error));
      })
      .finally(() => {
        enrichmentInFlight.current.delete(runId);
        setEnrichmentVersion((value) => value + 1);
      });
  }, [knownServerIds]);

  // Base normalization pass — doesn't depend on enrichment, so it's cheap to
  // recompute whenever the underlying stores change.
  const baseItems = useMemo(() => {
    const daemonManagedRunIds = daemon?.managedRunIds ?? [];
    const enrichmentByRunId = new Map<string, RunEnrichment>();
    for (const run of runs) {
      const cached = enrichmentCache.current.get(run.spec.run_id);
      if (cached) enrichmentByRunId.set(run.spec.run_id, cached.data);
    }
    const runItems = buildRunInboxItems(runs, enrichmentByRunId, daemonManagedRunIds);
    const automationItems = buildAutomationInboxItems(automationEntries, nextRunCache.current);
    const chatItems = buildChatApprovalInboxItems(chatApprovalQueue, knownServerIds);
    const sideTaskItems = buildSideTaskInboxItems(sideTasks);
    return sortInboxItems(mergeInboxItems(runItems, automationItems, chatItems, sideTaskItems));
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [runs, automationEntries, chatApprovalQueue, sideTasks, knownServerIds, daemon, enrichmentVersion, nextRunVersion]);

  // Eagerly enrich active/waiting runs — the rows most likely to answer
  // "what needs me right now".
  useEffect(() => {
    for (const item of baseItems) {
      if (isHighPriorityRun(item) && item.runId) {
        const run = runs.find((r) => r.spec.run_id === item.runId);
        if (run) requestEnrichment(run.spec.run_id, run.updatedAtMs);
      }
    }
  }, [baseItems, runs, requestEnrichment]);

  // Fetch each enabled automation's next occurrence once (cron expressions
  // rarely change) — real data from the same `cron_next` command the
  // Scheduled Tasks panel already relies on.
  useEffect(() => {
    for (const entry of automationEntries) {
      if (!entry.enabled || nextRunCache.current.has(entry.id)) continue;
      nextRunCache.current.set(entry.id, null); // claim immediately, avoid duplicate in-flight requests
      invoke<number[]>("cron_next", { expr: entry.cron, n: 1 })
        .then((occurrences) => {
          nextRunCache.current.set(entry.id, occurrences[0] ?? null);
          setNextRunVersion((value) => value + 1);
        })
        .catch(() => {
          // Leave it at null — an invalid cron expression is already
          // surfaced elsewhere (Scheduled Tasks panel's own validation).
        });
    }
  }, [automationEntries]);

  const filterOptions = useMemo(() => inboxFilterOptions(baseItems), [baseItems]);

  // Applying a cost/risk/connector filter needs real per-run data to mean
  // anything — trigger a bounded bulk enrichment pass over whatever the
  // *other* filters already narrowed things down to, so the filter doesn't
  // just silently hide every not-yet-loaded row.
  useEffect(() => {
    if (!filters.costBucket && !filters.riskLevel && !filters.connector) return;
    const candidates = filterInboxItems(baseItems, { ...filters, costBucket: null, riskLevel: null, connector: null })
      .filter((item) => item.sourceKind === "run" && item.runId)
      .slice(0, 80);
    for (const item of candidates) {
      const run = runs.find((r) => r.spec.run_id === item.runId);
      if (run) requestEnrichment(run.spec.run_id, run.updatedAtMs);
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [filters.costBucket, filters.riskLevel, filters.connector, baseItems, runs, requestEnrichment]);

  const tabFilters = useMemo<InboxFilters>(() => ({
    ...filters,
    status: statusTab === "all" ? null : statusTab,
  }), [filters, statusTab]);

  const visibleItems = useMemo(() => {
    let items = filterInboxItems(baseItems, tabFilters);
    if (statusTab === "all") items = items.filter((item) => item.status !== "archived");
    if (needsApprovalOnly) items = items.filter((item) => item.needsApproval);
    return items;
  }, [baseItems, tabFilters, statusTab, needsApprovalOnly]);

  const totalNeedsApproval = useMemo(() => needsApprovalCount(baseItems), [baseItems]);

  useEffect(() => {
    if (selectedId && visibleItems.some((item) => item.id === selectedId)) return;
    setSelectedId(visibleItems[0]?.id ?? null);
  }, [visibleItems, selectedId]);

  const selectedItem = visibleItems.find((item) => item.id === selectedId) ?? null;
  const selectedRun = selectedItem?.runId ? runs.find((r) => r.spec.run_id === selectedItem.runId) ?? null : null;
  const selectedSideTask = selectedItem?.sideTaskId ? sideTaskById[selectedItem.sideTaskId] ?? null : null;
  useEffect(() => {
    if (selectedRun) requestEnrichment(selectedRun.spec.run_id, selectedRun.updatedAtMs);
  }, [selectedRun, requestEnrichment]);
  const selectedEnrichment = selectedRun ? enrichmentCache.current.get(selectedRun.spec.run_id)?.data ?? null : null;
  const selectedEnrichmentError = selectedRun ? enrichmentErrors.current.get(selectedRun.spec.run_id) ?? null : null;
  const selectedEnrichmentLoading = selectedRun ? enrichmentInFlight.current.has(selectedRun.spec.run_id) : false;

  const runAction = useCallback(async (key: string, action: () => Promise<void>) => {
    setActionBusy(key);
    setActionError(null);
    try {
      await action();
      refreshDaemon();
    } catch (error) {
      setActionError(error instanceof Error ? error.message : String(error));
    } finally {
      setActionBusy(null);
    }
  }, [refreshDaemon]);

  const handleDecideRunApproval = useCallback((requestId: string, operationSha256: string, decision: PermissionDecision) => {
    if (!selectedRun) return;
    const runId = selectedRun.spec.run_id;
    void runAction(`decide:${requestId}`, async () => {
      await decideRunPermission(runId, requestId, operationSha256, decision);
      // Drop the stale cache entry and let `refreshRun` update the shared
      // `runs` list with the run's new `updatedAtMs`; the eager-enrichment
      // effect (keyed on that real timestamp, not a synthetic one) then
      // re-fetches events on its own. Passing `Date.now()` here instead would
      // desync the cache key from what that effect expects forever, causing
      // a refetch loop every time `baseItems` recomputes.
      enrichmentCache.current.delete(runId);
      await useRunStore.getState().refreshRun(runId);
    });
  }, [selectedRun, runAction]);

  const handleDecideChatApproval = useCallback((allow: boolean, remember: boolean) => {
    void runAction("chat-approval", () => respondToChatApproval(allow, remember));
  }, [runAction, respondToChatApproval]);

  const handleCancelRun = useCallback(() => {
    if (!selectedRun) return;
    const runId = selectedRun.spec.run_id;
    const managed = isDaemonManagedRun(runId, daemon?.managedRunIds ?? []);
    void runAction("cancel", async () => {
      if (managed) await daemonCancel(runId, t("AgentInbox.cancelReason"));
      else await requestRunCancellation(runId, t("AgentInbox.cancelReason"));
      await useRunStore.getState().refreshRun(runId);
    });
  }, [selectedRun, daemon, runAction, t]);

  const handleDaemonControl = useCallback((action: "pause" | "resume" | "retry") => {
    if (!selectedRun) return;
    const runId = selectedRun.spec.run_id;
    if (action === "retry") {
      const hasMutationBoundary = selectedItem?.status === "failed" || (selectedEnrichment?.mutations.some((m) => m.confirmedAtMs == null) ?? false);
      if (hasMutationBoundary && !window.confirm(t("AgentInbox.retryMutationWarning"))) return;
      void runAction("retry", async () => {
        await daemonRetry(runId, hasMutationBoundary);
        await refreshRuns();
      });
      return;
    }
    void runAction(action, async () => {
      await (action === "pause" ? daemonPause(runId) : daemonResume(runId));
      await useRunStore.getState().refreshRun(runId);
    });
  }, [selectedRun, selectedItem, selectedEnrichment, runAction, refreshRuns, t]);

  const handleArchive = useCallback(() => {
    if (!selectedRun) return;
    void runAction("archive", () => archiveRunAction(selectedRun.spec.run_id));
  }, [selectedRun, runAction, archiveRunAction]);

  const handleUnarchive = useCallback(() => {
    if (!selectedRun) return;
    void runAction("unarchive", () => unarchiveRunAction(selectedRun.spec.run_id));
  }, [selectedRun, runAction, unarchiveRunAction]);

  const handleRunAutomationNow = useCallback(() => {
    if (!selectedItem?.automationEntryId) return;
    const entry = automationEntries.find((e) => e.id === selectedItem.automationEntryId);
    if (!entry) return;
    const recipe = recipes.find((candidate) => candidate.recipe?.name === entry.recipeName)?.recipe;
    if (!recipe) {
      setActionError(`Recipe '${entry.recipeName}' is no longer available.`);
      return;
    }
    void runAction("run-now", () => runRecipeNow(recipe).then(() => undefined));
  }, [selectedItem, automationEntries, recipes, runAction]);

  const handleToggleAutomation = useCallback(() => {
    if (!selectedItem?.automationEntryId) return;
    const entry = automationEntries.find((e) => e.id === selectedItem.automationEntryId);
    if (!entry) return;
    useAutomationsStore.getState().updateEntry(entry.id, { enabled: !entry.enabled });
  }, [selectedItem, automationEntries]);

  const handleRevealSideTask = useCallback(() => {
    if (!selectedSideTask) return;
    const store = useSideTaskStore.getState();
    store.selectTask(selectedSideTask.id);
    store.revealPanel();
  }, [selectedSideTask]);

  const handleStartSideTaskFromMcpResult = useCallback((call: RunEnrichment["toolCalls"][number]) => {
    if (!call.connectorId || !call.outputExcerpt?.trim()) return;
    useSideTaskStore.getState().openComposer(buildMcpResultSideTaskSeed({
      sessionId: activeChatSessionId,
      serverId: call.connectorId,
      toolName: call.toolName,
      output: call.outputExcerpt,
    }));
  }, [activeChatSessionId]);

  const isHeadOfChatQueue = chatApprovalQueue[0]?.id === selectedItem?.approvalRequestId;

  const activeFilterCount = [filters.workspaceId, filters.sourceTrigger, filters.model, filters.connector, filters.costBucket, filters.riskLevel].filter(Boolean).length;

  return (
    <section className="flex min-h-0 flex-1 flex-col" aria-labelledby="agent-inbox-title">
      <header className="flex shrink-0 flex-col gap-2 border-b border-border px-4 py-3">
        <div className="flex items-center justify-between gap-3">
          <div className="min-w-0">
            <h1 id="agent-inbox-title" className="text-base font-semibold text-foreground">{t("AgentInbox.title")}</h1>
            <p className="truncate text-xs text-muted">{t("AgentInbox.subtitle")}</p>
          </div>
          <div className="flex shrink-0 items-center gap-1">
            <IconButton size="sm" onClick={() => { refreshDaemon(); void refreshRuns(); }} aria-label={t("AgentInbox.refresh")}>
              <RefreshCw size={15} className={runsLoading ? "animate-spin" : ""} />
            </IconButton>
            <IconButton size="sm" onClick={onClose} aria-label={t("AgentInbox.close")}>
              <X size={16} />
            </IconButton>
          </div>
        </div>
        {daemon && (
          <p className="text-xs text-faint">
            {daemon.serviceRunning
              ? t("AgentInbox.daemonSummary", { active: daemon.active, waitingApproval: daemon.waitingApproval, queued: daemon.queued, paused: daemon.paused })
              : t("AgentInbox.daemonOffline")}
          </p>
        )}
      </header>

      {(actionError) && (
        <div role="alert" className="flex items-start justify-between gap-3 border-b border-danger/30 bg-danger-soft px-4 py-2 text-xs text-danger">
          <span>{t("AgentInbox.actionError", { error: actionError })}</span>
          <button type="button" className="underline" onClick={() => setActionError(null)}>{t("AgentInbox.dismiss")}</button>
        </div>
      )}

      <div className="flex shrink-0 flex-wrap items-center justify-between gap-2 border-b border-border px-3 py-2">
        <Tabs
          tabs={STATUS_TABS.map((id) => ({ id, label: id === "all" ? t("AgentInbox.tab.all") : t(`AgentInbox.tab.${id}`) }))}
          active={statusTab}
          onChange={(id) => setStatusTab(id as InboxStatus | "all")}
        />
        <div className="flex items-center gap-2">
          <label className="flex items-center gap-1.5 text-xs text-muted">
            <input type="checkbox" checked={needsApprovalOnly} onChange={(e) => setNeedsApprovalOnly(e.target.checked)} />
            {t("AgentInbox.needsApprovalOnly")} ({totalNeedsApproval})
          </label>
          <Button size="sm" variant={filtersOpen ? "primary" : "secondary"} onClick={() => setFiltersOpen((v) => !v)}>
            <Filter size={13} /> {t("AgentInbox.filters")}{activeFilterCount > 0 ? ` (${activeFilterCount})` : ""}
          </Button>
        </div>
      </div>

      <div className="flex shrink-0 items-center gap-2 border-b border-border px-3 py-2">
        <Search size={14} className="shrink-0 text-faint" />
        <input
          type="text"
          value={filters.search}
          onChange={(e) => setFilters((f) => ({ ...f, search: e.target.value }))}
          placeholder={t("AgentInbox.searchPlaceholder")}
          className="w-full bg-transparent text-sm text-foreground placeholder:text-faint focus:outline-none"
        />
      </div>

      {filtersOpen && (
        <div className="flex shrink-0 flex-wrap items-center gap-2 border-b border-border bg-surface px-3 py-2">
          <select
            value={filters.workspaceId ?? ""}
            onChange={(e) => setFilters((f) => ({ ...f, workspaceId: e.target.value || null }))}
            className="rounded-md border border-border bg-background px-2 py-1 text-xs text-foreground"
            aria-label={t("AgentInbox.filterWorkspace")}
          >
            <option value="">{t("AgentInbox.filterWorkspace")}: {t("AgentInbox.filterAny")}</option>
            {filterOptions.workspaces.map((w) => <option key={w.id} value={w.id}>{w.label}</option>)}
          </select>
          <select
            value={filters.sourceTrigger ?? ""}
            onChange={(e) => setFilters((f) => ({ ...f, sourceTrigger: e.target.value || null }))}
            className="rounded-md border border-border bg-background px-2 py-1 text-xs text-foreground"
            aria-label={t("AgentInbox.filterSourceTrigger")}
          >
            <option value="">{t("AgentInbox.filterSourceTrigger")}: {t("AgentInbox.filterAny")}</option>
            {filterOptions.sourceTriggers.map((v) => <option key={v} value={v}>{v}</option>)}
          </select>
          <select
            value={filters.model ?? ""}
            onChange={(e) => setFilters((f) => ({ ...f, model: e.target.value || null }))}
            className="rounded-md border border-border bg-background px-2 py-1 text-xs text-foreground"
            aria-label={t("AgentInbox.filterModel")}
          >
            <option value="">{t("AgentInbox.filterModel")}: {t("AgentInbox.filterAny")}</option>
            {filterOptions.models.map((v) => <option key={v} value={v}>{v}</option>)}
          </select>
          <select
            value={filters.connector ?? ""}
            onChange={(e) => setFilters((f) => ({ ...f, connector: e.target.value || null }))}
            className="rounded-md border border-border bg-background px-2 py-1 text-xs text-foreground"
            aria-label={t("AgentInbox.filterConnector")}
          >
            <option value="">{t("AgentInbox.filterConnector")}: {t("AgentInbox.filterAny")}</option>
            {filterOptions.connectors.map((v) => <option key={v} value={v}>{v}</option>)}
          </select>
          <select
            value={filters.costBucket ?? ""}
            onChange={(e) => setFilters((f) => ({ ...f, costBucket: (e.target.value || null) as CostBucket | null }))}
            className="rounded-md border border-border bg-background px-2 py-1 text-xs text-foreground"
            aria-label={t("AgentInbox.filterCost")}
          >
            <option value="">{t("AgentInbox.filterCost")}: {t("AgentInbox.filterAny")}</option>
            {COST_BUCKETS.map((v) => <option key={v} value={v}>{t(`AgentInbox.cost.${v}`)}</option>)}
          </select>
          <select
            value={filters.riskLevel ?? ""}
            onChange={(e) => setFilters((f) => ({ ...f, riskLevel: (e.target.value || null) as InboxFilters["riskLevel"] }))}
            className="rounded-md border border-border bg-background px-2 py-1 text-xs text-foreground"
            aria-label={t("AgentInbox.filterRisk")}
          >
            <option value="">{t("AgentInbox.filterRisk")}: {t("AgentInbox.filterAny")}</option>
            <option value="unknown">{t("AgentInbox.risk.unknown")}</option>
            <option value="low">{t("AgentInbox.risk.low")}</option>
            <option value="medium">{t("AgentInbox.risk.medium")}</option>
            <option value="high">{t("AgentInbox.risk.high")}</option>
          </select>
          {activeFilterCount > 0 && (
            <Button size="sm" variant="ghost" onClick={() => setFilters(EMPTY_INBOX_FILTERS)}>{t("AgentInbox.clearFilters")}</Button>
          )}
        </div>
      )}

      <div className="flex min-h-0 flex-1">
        <nav className="flex w-80 shrink-0 flex-col overflow-y-auto border-r border-border bg-surface [overscroll-behavior:contain]" aria-label={t("AgentInbox.title")}>
          {runsLoading && visibleItems.length === 0 ? (
            <p className="p-4 text-sm text-faint">{t("AgentInbox.loading")}</p>
          ) : visibleItems.length === 0 ? (
            <div className="p-5 text-center">
              <p className="mt-2 text-sm font-medium">{t("AgentInbox.emptyTitle")}</p>
              <p className="mt-1 text-xs text-muted">{t("AgentInbox.emptyDescription")}</p>
            </div>
          ) : visibleItems.map((item) => (
            <InboxRow key={item.id} item={item} selected={item.id === selectedId} onSelect={() => setSelectedId(item.id)} />
          ))}
        </nav>

        <div className="min-w-0 flex-1 overflow-y-auto [overscroll-behavior:contain]">
          {!selectedItem ? (
            <p className="p-6 text-sm text-faint">{t("AgentInbox.selectHint")}</p>
          ) : (
            <DetailPane
              item={selectedItem}
              enrichment={selectedEnrichment}
              enrichmentLoading={selectedEnrichmentLoading}
              enrichmentError={selectedEnrichmentError}
              daemonManagedRunIds={daemon?.managedRunIds ?? []}
              automationEntries={automationEntries}
              chatApprovalQueue={chatApprovalQueue}
              sideTasks={sideTasks}
              isHeadOfChatQueue={isHeadOfChatQueue}
              actionBusy={actionBusy}
              onDecideRunApproval={handleDecideRunApproval}
              onDecideChatApproval={handleDecideChatApproval}
              onCancelRun={handleCancelRun}
              onDaemonControl={handleDaemonControl}
              onArchive={handleArchive}
              onUnarchive={handleUnarchive}
              onRunAutomationNow={handleRunAutomationNow}
              onToggleAutomation={handleToggleAutomation}
              onOpenRunCenter={onOpenRunCenter}
              onRevealSideTask={handleRevealSideTask}
              onStartSideTaskFromMcpResult={handleStartSideTaskFromMcpResult}
            />
          )}
        </div>
      </div>
    </section>
  );
}

export default AgentInbox;
