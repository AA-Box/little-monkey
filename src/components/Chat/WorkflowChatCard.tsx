import { memo, useEffect, useState } from "react";
import { ChevronRight, Network } from "lucide-react";
import { useShallow } from "zustand/react/shallow";

import { useWorkflowStore, type WorkflowStatus } from "../../store/workflowStore";
import { useSubagentStore, type SubagentStatus } from "../../store/subagentStore";
import { useSessionStore } from "../../store/sessionStore";
import { formatCompactTokens, formatElapsed } from "../../lib/taskFormat";
import { StatusPill, type PillTone } from "../ui";
import { useT } from "../../lib/i18n";
import { dotClass } from "./SubagentGroupCard";
import { resultLooksLikeError } from "./activityTimeline";
import { unwrapUntrustedContent } from "../../lib/untrustedContent";
import { CANCELLED_TOOL_RESULT } from "../../lib/turnEngine";

/** Parsed subset of the `workflow` tool arguments the card can render before
 * (or without) any store entry — the transcript itself is the fallback
 * source after a restart wipes the transient stores. Exported for the
 * DOM-free logic tests, same convention as `SubagentRow.parseTaskArgs`. */
export function parseWorkflowArgs(args: string): { name: string; agentCount: number } {
  try {
    const parsed: unknown = args ? JSON.parse(args) : null;
    if (parsed && typeof parsed === "object") {
      const candidate = parsed as { name?: unknown; phases?: unknown };
      const name = typeof candidate.name === "string" && candidate.name.trim().length > 0 ? candidate.name.trim() : "workflow";
      const agentCount = Array.isArray(candidate.phases)
        ? candidate.phases.reduce<number>((sum, phase) => {
            const agents = (phase as { agents?: unknown })?.agents;
            return sum + (Array.isArray(agents) ? agents.length : 0);
          }, 0)
        : 0;
      return { name, agentCount };
    }
  } catch {
    // fall through to the default below
  }
  return { name: "workflow", agentCount: 0 };
}

/** Resolves the run-level status shown on the card: live store entry wins,
 * then the persisted snapshot, then the tool result alone (a restored
 * transcript with no meta at all) — mirroring `resolveSubagentStatus`.
 * Exported for the logic tests. */
export function resolveWorkflowStatus(
  liveStatus: WorkflowStatus | undefined,
  metaStatus: "done" | "error" | "cancelled" | undefined,
  result: string | undefined,
): WorkflowStatus {
  if (liveStatus) return liveStatus;
  if (metaStatus) return metaStatus;
  if (result === undefined) return "running";
  if (unwrapUntrustedContent(result) === CANCELLED_TOOL_RESULT) return "cancelled";
  if (resultLooksLikeError(result)) return "error";
  return "done";
}

function pillTone(status: WorkflowStatus): PillTone {
  switch (status) {
    case "running":
      return "warning";
    case "done":
      return "success";
    case "error":
      return "danger";
    default:
      return "neutral";
  }
}

function statusKey(status: WorkflowStatus): string {
  switch (status) {
    case "running":
      return "BackgroundTasksPanel.agentStatusRunning";
    case "done":
      return "BackgroundTasksPanel.agentStatusCompleted";
    case "error":
      return "BackgroundTasksPanel.agentStatusFailed";
    default:
      return "BackgroundTasksPanel.agentStatusCancelled";
  }
}

/**
 * A `workflow` tool call's transcript card — name, "Workflow · N agents ·
 * elapsed", and one status dot per agent, Claude-Code-desktop-style.
 * Clicking opens the Background-tasks drawer (where `WorkflowRunCard` shows
 * the per-phase detail); without an `onOpenPanel` host the card is a plain
 * status readout, nothing to expand inline.
 */
const WorkflowChatCard = memo(function WorkflowChatCard({
  sessionId,
  runId,
  args,
  result,
  onOpenPanel,
}: {
  sessionId: string;
  runId: string;
  args: string;
  result?: string;
  onOpenPanel?: () => void;
}) {
  const { t } = useT();
  const live = useWorkflowStore((state) => state.runs[runId]);
  const meta = useSessionStore((state) => state.sessions.find((s) => s.id === sessionId)?.workflowRunMeta?.[runId]);
  const shape = live ?? meta;
  const parsed = parseWorkflowArgs(args);

  const name = shape?.name ?? parsed.name;
  const taskIds = shape ? shape.phases.flatMap((phase) => phase.agents.map((agent) => agent.taskId)) : [];
  const agentCount = taskIds.length > 0 ? taskIds.length : parsed.agentCount;
  const status = resolveWorkflowStatus(live?.status, meta?.status, result);
  const running = status === "running";

  const liveAgents = useSubagentStore(useShallow((state) => taskIds.map((taskId) => state.runs[taskId])));
  const agentMeta = useSessionStore((state) => state.sessions.find((s) => s.id === sessionId)?.subagentRunMeta);
  const dotStatuses: SubagentStatus[] = taskIds.map((taskId, index) => {
    const agent = liveAgents[index] ?? agentMeta?.[taskId];
    // No entry at all = not dispatched yet (a later phase while running) or
    // lost to a restart (terminal run). While running that's a queued agent
    // — the neutral `dotClass` dot ("cancelled" tone) reads as pending;
    // terminal runs inherit the run's own outcome.
    if (!agent) return running ? "cancelled" : status;
    return agent.status;
  });
  const totalTokens = taskIds.reduce((sum, taskId, index) => {
    const agent = liveAgents[index] ?? agentMeta?.[taskId];
    return sum + (agent?.usage?.totalTokens ?? 0);
  }, 0);

  const startedAt = shape?.startedAt;
  const finishedAt = live?.finishedAt ?? meta?.finishedAt;

  const [, setTick] = useState(0);
  useEffect(() => {
    if (!running) return;
    const interval = setInterval(() => setTick((value) => value + 1), 1000);
    return () => clearInterval(interval);
  }, [running]);

  return (
    <div className="flex justify-start">
      <button
        type="button"
        onClick={onOpenPanel}
        disabled={!onOpenPanel}
        className="max-w-[85%] min-w-64 cursor-pointer rounded-md border border-border bg-surface-2 px-3 py-2 text-left transition-colors duration-150 hover:border-border-strong focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-accent disabled:cursor-default motion-reduce:transition-none"
      >
        <span className="flex items-center gap-2">
          <Network size={13} className="shrink-0 text-faint" aria-hidden />
          <span className="min-w-0 flex-1 truncate text-[13px] font-medium text-foreground">{name}</span>
          <StatusPill tone={pillTone(status)}>{t(statusKey(status))}</StatusPill>
          {onOpenPanel && <ChevronRight size={13} className="shrink-0 text-faint" aria-hidden />}
        </span>
        <span className="mt-1 flex items-center gap-2 text-xs text-muted">
          <span>{t("BackgroundTasksPanel.workflowKindLabel")}</span>
          {agentCount > 0 && <span className="text-faint">{t("SubagentGroupCard.title", { count: agentCount })}</span>}
          {startedAt !== undefined && (
            <span className="font-mono text-[10px] text-faint">{formatElapsed((running ? Date.now() : (finishedAt ?? Date.now())) - startedAt)}</span>
          )}
          {totalTokens > 0 && (
            <span className="font-mono text-[10px] text-faint">{t("SubagentGroupCard.tokenUsage", { count: formatCompactTokens(totalTokens) })}</span>
          )}
        </span>
        {dotStatuses.length > 0 && (
          <span className="mt-1.5 flex items-center gap-1" aria-label={t("SubagentGroupCard.progressAriaLabel")}>
            {dotStatuses.map((dotStatus, index) => (
              <span key={taskIds[index]} className={`h-1.5 w-1.5 rounded-full ${dotClass(dotStatus)}`} />
            ))}
          </span>
        )}
      </button>
    </div>
  );
});

export default WorkflowChatCard;
