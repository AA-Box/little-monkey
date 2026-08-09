import { memo, useEffect, useId, useState } from "react";
import { Bot, ChevronRight } from "lucide-react";
import { useShallow } from "zustand/react/shallow";

import { useSubagentStore, type SubagentStatus } from "../../store/subagentStore";
import { useSessionStore } from "../../store/sessionStore";
import { formatCompactTokens, formatElapsed } from "../../lib/taskFormat";
import { StatusPill, type PillTone } from "../ui";
import { useT } from "../../lib/i18n";
import SubagentRow, { resolveSubagentStatus, statusLabelKey } from "./SubagentRow";

/** One `task` tool_call of the group — the same triple `buildTimeline` hands
 * a standalone `SubagentRow` (see `MessageList.tsx`'s `kind: "subagent"`
 * items; a group item carries an array of these instead). */
export interface SubagentGroupTask {
  taskId: string;
  args: string;
  result?: string;
}

/** The pill tone the whole card shows: any run still going wins, then any
 * failure, then all-cancelled, else success — exported for the same
 * DOM-free logic tests `SubagentRow`'s helpers get (see that module's top
 * comment for why JSX itself isn't unit-tested here). */
export function resolveGroupStatus(statuses: SubagentStatus[]): SubagentStatus {
  if (statuses.some((status) => status === "running")) return "running";
  if (statuses.some((status) => status === "error")) return "error";
  if (statuses.every((status) => status === "cancelled")) return "cancelled";
  return "done";
}

/** Exported for the Background-tasks drawer's `AgentGroupCard`, which shows
 * the same per-agent status dots this card's collapsed header does. */
export function dotClass(status: SubagentStatus): string {
  switch (status) {
    case "running":
      return "animate-pulse bg-accent";
    case "done":
      return "bg-success";
    case "error":
      return "bg-danger";
    default:
      return "bg-faint";
  }
}

/**
 * Renders an assistant message's PARALLEL `task` calls (two or more — a lone
 * one stays a plain `SubagentRow`) as one collapsed progress card: agent
 * count, a ticking elapsed time, the group's summed token count, and one
 * status dot per agent — expanding to the individual `SubagentRow`s.
 * Claude-Code-desktop-style: the transcript shows one card doing the work of
 * N spinners, and the per-agent detail is one click away.
 */
const SubagentGroupCard = memo(function SubagentGroupCard({
  sessionId,
  tasks,
  onOpenPanel,
}: {
  sessionId: string;
  tasks: SubagentGroupTask[];
  /** When provided, clicking the card opens the Background-tasks drawer
   * (where `AgentGroupCard` shows this round's per-agent table) instead of
   * expanding inline — Claude-Code-desktop-style. Hosts without the
   * right-sidebar region omit it and keep the inline expansion. */
  onOpenPanel?: () => void;
}) {
  const { t } = useT();
  const [open, setOpen] = useState(false);
  const detailsId = useId();
  // One subscription for the whole group — a fresh array per snapshot, so
  // `useShallow` keeps it from re-render-looping, same as the drawer's list
  // selectors.
  const liveRuns = useSubagentStore(useShallow((state) => tasks.map((task) => state.runs[task.taskId])));
  // Post-restart stats source (see ChatSession.subagentRunMeta) — live
  // entries win while they exist; both carry the same
  // startedAt/finishedAt/usage subset read below.
  const metaMap = useSessionStore((state) => state.sessions.find((s) => s.id === sessionId)?.subagentRunMeta);
  const statRuns = tasks.map((task, index) => liveRuns[index] ?? metaMap?.[task.taskId]);

  const statuses = tasks.map((task, index) => resolveSubagentStatus(liveRuns[index]?.status, task.result));
  const groupStatus = resolveGroupStatus(statuses);
  const running = groupStatus === "running";

  // Elapsed = first dispatch → last finish (or now while running).
  // End times come from dispatched runs only — a member with no entry at
  // all never ran (e.g. cancelled while queued) and contributes no timing,
  // same filter `startTimes` applies. Without this, one missing entry would
  // keep `settledAt` null forever and a finished group's elapsed label
  // would keep growing.
  const startTimes = statRuns.filter(Boolean).map((run) => run!.startedAt);
  const endTimes = statRuns.filter(Boolean).map((run) => run!.finishedAt);
  const startedAt = startTimes.length > 0 ? Math.min(...startTimes) : null;
  const settledAt = !running && endTimes.length > 0 && endTimes.every((time) => time !== undefined) ? Math.max(...(endTimes as number[])) : null;

  // A 1s tick, active only while the group is live — the elapsed label and
  // summed tokens re-render from it without any store churn.
  const [, setTick] = useState(0);
  useEffect(() => {
    if (!running) return;
    const interval = setInterval(() => setTick((value) => value + 1), 1000);
    return () => clearInterval(interval);
  }, [running]);

  const totalTokens = statRuns.reduce((sum, run) => sum + (run?.usage?.totalTokens ?? 0), 0);
  const tone: PillTone = groupStatus === "running" ? "warning" : groupStatus === "error" ? "danger" : groupStatus === "cancelled" ? "neutral" : "success";

  return (
    <div className="flex justify-start">
      <div className="max-w-[85%] min-w-0 overflow-hidden rounded-md border border-border bg-surface-2">
        <button
          type="button"
          aria-expanded={onOpenPanel ? undefined : open}
          aria-controls={onOpenPanel ? undefined : detailsId}
          onClick={() => (onOpenPanel ? onOpenPanel() : setOpen((prev) => !prev))}
          className="flex min-h-11 w-full cursor-pointer items-center gap-2 px-3 py-1.5 text-left text-xs text-muted transition-colors duration-150 hover:text-foreground focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-accent focus-visible:ring-inset motion-reduce:transition-none"
        >
          <ChevronRight
            size={12}
            className={`shrink-0 text-faint transition-transform duration-150 motion-reduce:transition-none ${open && !onOpenPanel ? "rotate-90" : ""}`}
          />
          <Bot size={13} className="shrink-0 text-faint" />
          <span className="truncate font-medium text-foreground">{t("SubagentGroupCard.title", { count: tasks.length })}</span>
          <StatusPill tone={tone}>{t(statusLabelKey(groupStatus))}</StatusPill>
          {startedAt !== null && (
            <span className="shrink-0 font-mono text-[10px] text-faint">{formatElapsed((settledAt ?? Date.now()) - startedAt)}</span>
          )}
          {totalTokens > 0 && (
            <span className="shrink-0 font-mono text-[10px] text-faint">
              {t("SubagentGroupCard.tokenUsage", { count: formatCompactTokens(totalTokens) })}
            </span>
          )}
          <span className="ml-auto flex shrink-0 items-center gap-1" aria-label={t("SubagentGroupCard.progressAriaLabel")}>
            {statuses.map((status, index) => (
              <span key={tasks[index].taskId} className={`h-1.5 w-1.5 rounded-full ${dotClass(status)}`} />
            ))}
          </span>
        </button>
        {open && !onOpenPanel && (
          <div id={detailsId} className="space-y-2 border-t border-border bg-background px-3 py-2">
            {tasks.map((task) => (
              <SubagentRow key={task.taskId} sessionId={sessionId} taskId={task.taskId} args={task.args} result={task.result} />
            ))}
          </div>
        )}
      </div>
    </div>
  );
});

export default SubagentGroupCard;
