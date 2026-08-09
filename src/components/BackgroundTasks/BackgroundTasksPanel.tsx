import { Fragment, useEffect, useMemo, useState } from "react";
import { useShallow } from "zustand/react/shallow";
import { ChevronRight, Loader2, Square, TerminalSquare, X } from "lucide-react";

import { Button, IconButton, StatusPill, type PillTone } from "../ui";
import {
  selectFinishedShellTasks,
  selectRunningShellTaskCount,
  selectRunningShellTasks,
  useBackgroundShellStore,
  type BackgroundShellStatus,
  type BackgroundShellTask,
} from "../../store/backgroundShellStore";
import {
  selectRunningSubagentCount,
  selectSubagentRunList,
  useSubagentStore,
  type SubagentRun,
  type SubagentStatus,
} from "../../store/subagentStore";
import { useSessionStore } from "../../store/sessionStore";
import { useT } from "../../lib/i18n";
import { cancelSubagentRun } from "../../lib/subagent";
import { formatCompactTokens, formatElapsed } from "../../lib/taskFormat";
import { textContent, type ChatMessage } from "../../lib/llamaClient";
import { ToolCallRow } from "../Chat/MessageList";
import { dotClass, resolveGroupStatus } from "../Chat/SubagentGroupCard";

/**
 * Background tasks: work the APP is doing on its own behalf while the user
 * keeps talking to the main chat — background shell commands the agent
 * started with `run_shell`'s `run_in_background` (`backgroundShellStore.ts`),
 * and model-spawned `task` subagent runs (`subagentStore.ts`).
 *
 * Deliberately NOT side tasks. A side task is a second CONVERSATION the user
 * opened on purpose and can keep talking to — it lives in its own pane with
 * its own composer (`SideTasks/SideTaskPane.tsx`). Everything here is
 * headless: it has a status, an output tail or a transcript, and a stop
 * button, and there is nothing to say to it. Keeping the two apart is the
 * whole point of this file existing separately from that one.
 */

type Translate = ReturnType<typeof useT>["t"];

function shellStatusTone(status: BackgroundShellStatus): PillTone {
  switch (status) {
    case "running":
      return "warning";
    case "exited":
      return "success";
    case "error":
      return "danger";
    default:
      return "neutral";
  }
}

function shellStatusLabel(t: Translate, task: BackgroundShellTask): string {
  switch (task.status) {
    case "running":
      return t("BackgroundTasksPanel.shellStatusRunning");
    case "exited":
      return t("BackgroundTasksPanel.shellStatusExited");
    case "killed":
      return t("BackgroundTasksPanel.shellStatusStopped");
    case "error":
      return task.exit_code !== null
        ? t("BackgroundTasksPanel.shellStatusExitCode", { code: task.exit_code })
        : t("BackgroundTasksPanel.shellStatusFailed");
    default:
      return task.status;
  }
}

function subagentStatusTone(status: SubagentStatus): PillTone {
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

function subagentStatusLabel(t: Translate, status: SubagentStatus): string {
  switch (status) {
    case "running":
      return t("BackgroundTasksPanel.agentStatusRunning");
    case "done":
      return t("BackgroundTasksPanel.agentStatusCompleted");
    case "error":
      return t("BackgroundTasksPanel.agentStatusFailed");
    default:
      return t("BackgroundTasksPanel.agentStatusCancelled");
  }
}

/** Ticks once a second while `active`, so an elapsed label counts up live
 * without every card owning its own interval bookkeeping. */
function useLiveTick(active: boolean): void {
  const [, setTick] = useState(0);
  useEffect(() => {
    if (!active) return;
    const interval = setInterval(() => setTick((value) => value + 1), 1000);
    return () => clearInterval(interval);
  }, [active]);
}

function folderName(path: string): string {
  return path.split(/[\\/]/).filter(Boolean).pop() ?? path;
}

/**
 * One background shell command. Shows the exact command line that is running
 * (never a paraphrase — the user must be able to tell what process this is),
 * its live elapsed time, and its output tail on demand. The square stops it
 * through the same `background_shell_kill` the model's `shell_kill` tool
 * calls, so a user stop and an agent stop are the same operation.
 */
function ShellTaskCard({ task }: { task: BackgroundShellTask }) {
  const { t } = useT();
  const running = task.status === "running";
  const [showOutput, setShowOutput] = useState(false);
  useLiveTick(running);

  const elapsed = formatElapsed((task.finished_at_ms ?? Date.now()) - task.started_at_ms);
  const tail = task.output.trimEnd();
  const lastLine = tail.length > 0 ? tail.slice(tail.lastIndexOf("\n") + 1) : "";

  return (
    <div className="rounded-xl border border-border bg-surface-2 p-3">
      <div className="flex items-start gap-2">
        <TerminalSquare size={14} className="mt-0.5 shrink-0 text-faint" />
        <span className="min-w-0 flex-1 break-words font-mono text-xs leading-snug text-foreground">{task.command}</span>
        {running ? (
          <IconButton
            size="sm"
            aria-label={t("BackgroundTasksPanel.stopAriaLabel", { name: task.command })}
            onClick={() => void useBackgroundShellStore.getState().kill(task.id)}
          >
            <Square size={12} />
          </IconButton>
        ) : (
          <StatusPill tone={shellStatusTone(task.status)}>{shellStatusLabel(t, task)}</StatusPill>
        )}
      </div>
      <div className="mt-1 flex items-center gap-2 text-xs">
        <span className="text-muted">{t("BackgroundTasksPanel.shellKindLabel")}</span>
        <span className="truncate text-faint">{folderName(task.cwd)}</span>
        <span className="shrink-0 text-faint">{elapsed}</span>
        {running && <Loader2 size={11} className="shrink-0 animate-spin text-warning" />}
      </div>
      <div className="mt-0.5 flex flex-wrap items-center gap-x-3 gap-y-0.5 text-xs text-faint">
        {!running && task.exit_code !== null && <span>{t("BackgroundTasksPanel.exitCode", { code: task.exit_code })}</span>}
        {tail.length > 0 && (
          <button type="button" onClick={() => setShowOutput((prev) => !prev)} className="cursor-pointer text-accent hover:underline">
            {showOutput ? t("BackgroundTasksPanel.hideOutput") : t("BackgroundTasksPanel.viewOutput")}
          </button>
        )}
      </div>
      {!showOutput && running && lastLine && <div className="mt-1 truncate font-mono text-[11px] text-faint">{lastLine}</div>}
      {showOutput && (
        <div className="mt-2 border-t border-border pt-2">
          {task.output_truncated && <p className="mb-1 text-[11px] text-faint">{t("BackgroundTasksPanel.outputTruncated")}</p>}
          <pre className="max-h-64 overflow-auto whitespace-pre-wrap break-words rounded-md border border-border bg-background p-2 font-mono text-[11px] text-foreground">
            {tail}
          </pre>
        </div>
      )}
    </div>
  );
}

/** One subagent transcript entry, in conversation order: the seed prompt,
 * an assistant text segment, or a tool call with its result folded in from
 * the matching `tool` message. */
type TranscriptRow =
  | { key: string; kind: "prompt" | "text"; text: string }
  | { key: string; kind: "tool"; name: string; args: string; result?: string };

/** Flattens a child run's `liveMessages` into the FULL exchange — prompt,
 * every assistant text segment, every tool call — not just the tool calls:
 * the transcript must read as the whole conversation, so the final report is
 * simply the last text row rather than a special case appended at the end. */
function buildTranscriptRows(messages: ChatMessage[]): TranscriptRow[] {
  const resultByCallId = new Map<string, string>();
  for (const message of messages) {
    if (message.role === "tool" && message.tool_call_id) resultByCallId.set(message.tool_call_id, textContent(message.content));
  }
  const rows: TranscriptRow[] = [];
  messages.forEach((message, index) => {
    if (message.role === "user") {
      const text = textContent(message.content).trim();
      if (text) rows.push({ key: `prompt-${index}`, kind: "prompt", text });
      return;
    }
    if (message.role !== "assistant") return;
    const text = textContent(message.content).trim();
    if (text) rows.push({ key: `text-${index}`, kind: "text", text });
    (message.tool_calls ?? []).forEach((toolCall, callIndex) => {
      rows.push({
        // Index-qualified: provider-fallback ids (`call_0`, llamaClient.ts)
        // can repeat across iterations within one child transcript.
        key: `tool-${index}-${callIndex}-${toolCall.id}`,
        kind: "tool",
        name: toolCall.function.name,
        args: toolCall.function.arguments,
        result: resultByCallId.get(toolCall.id),
      });
    });
  });
  return rows;
}

/** The transcript body shared by `AgentTaskCard` and `AgentGroupCard`'s
 * expanded rows — one element per `buildTranscriptRows` entry. */
function TranscriptRowsView({ rows }: { rows: TranscriptRow[] }) {
  return (
    <>
      {rows.map((row) =>
        row.kind === "tool" ? (
          <ToolCallRow key={row.key} name={row.name} args={row.args} result={row.result} />
        ) : (
          <p
            key={row.key}
            className={`whitespace-pre-wrap rounded-md border border-border bg-background p-2 ${
              row.kind === "prompt" ? "font-mono text-[11px] text-muted" : "text-xs text-foreground"
            }`}
          >
            {row.text}
          </p>
        ),
      )}
    </>
  );
}

/**
 * One `task`-tool subagent run — the model's own delegated work. Same card
 * shape as `ShellTaskCard` above so the two kinds read as one list: title,
 * stop square while running, then "Agent · elapsed" and the token/tool-use
 * counts. The token count renders from the first second (0 until the child's
 * first iteration reports usage) and the transcript expands inline as the
 * full exchange via `buildTranscriptRows` — both available while the run is
 * still going, not only after it finishes.
 */
function AgentTaskCard({ run }: { run: SubagentRun }) {
  const { t } = useT();
  const running = run.status === "running";
  const [showTranscript, setShowTranscript] = useState(false);
  useLiveTick(running);

  const transcriptRows = buildTranscriptRows(run.liveMessages);

  return (
    <div className="rounded-xl border border-border bg-surface-2 p-3">
      <div className="flex items-start gap-2">
        <span className="min-w-0 flex-1 text-sm font-medium leading-snug text-foreground">{run.description}</span>
        {running ? (
          <IconButton
            size="sm"
            aria-label={t("BackgroundTasksPanel.stopAriaLabel", { name: run.description })}
            onClick={() => cancelSubagentRun(run.cancelId)}
          >
            <Square size={12} />
          </IconButton>
        ) : (
          <StatusPill tone={subagentStatusTone(run.status)}>{subagentStatusLabel(t, run.status)}</StatusPill>
        )}
      </div>
      <div className="mt-1 flex items-center gap-2 text-xs">
        <span className="text-muted">{t("BackgroundTasksPanel.agentKindLabel")}</span>
        <span className="text-faint">{formatElapsed((run.finishedAt ?? Date.now()) - run.startedAt)}</span>
        {running && <Loader2 size={11} className="shrink-0 animate-spin text-warning" />}
      </div>
      <div className="mt-0.5 flex flex-wrap items-center gap-x-3 gap-y-0.5 text-xs text-faint">
        <span>{t("BackgroundTasksPanel.tokenUsage", { count: formatCompactTokens(run.usage?.totalTokens ?? 0) })}</span>
        <span>
          {run.toolCallCount === 1
            ? t("BackgroundTasksPanel.toolCallCountOne")
            : t("BackgroundTasksPanel.toolCallCountMany", { count: run.toolCallCount })}
        </span>
        {transcriptRows.length > 0 && (
          <button type="button" onClick={() => setShowTranscript((prev) => !prev)} className="cursor-pointer text-accent hover:underline">
            {showTranscript ? t("BackgroundTasksPanel.hideTranscript") : t("BackgroundTasksPanel.viewTranscript")}
          </button>
        )}
      </div>
      {running && run.lastActivity && <div className="mt-1 truncate font-mono text-[11px] text-faint">{run.lastActivity}</div>}
      {showTranscript && (
        <div className="mt-2 space-y-1.5 border-t border-border pt-2">
          <TranscriptRowsView rows={transcriptRows} />
        </div>
      )}
    </div>
  );
}

/** One agent-list entry after grouping: a lone run, or one parallel round's
 * same-`groupId` runs (see `SubagentRun.groupId`). */
export type AgentPanelEntry = { kind: "single"; run: SubagentRun } | { kind: "group"; groupId: string; runs: SubagentRun[] };

/** Groups same-`groupId` runs (one parallel round's `task` calls) into one
 * entry, preserving the input's order by each group's first appearance. A
 * group with only one member left (e.g. after Clear dropped the others)
 * renders as a plain card. Exported for the DOM-free logic tests — see
 * `SubagentRow.test.ts`'s top comment for the convention. */
export function groupAgentRuns(runs: SubagentRun[]): AgentPanelEntry[] {
  const members = new Map<string, SubagentRun[]>();
  const entries: AgentPanelEntry[] = [];
  for (const run of runs) {
    if (!run.groupId) {
      entries.push({ kind: "single", run });
      continue;
    }
    const existing = members.get(run.groupId);
    if (existing) {
      existing.push(run);
      continue;
    }
    const group: SubagentRun[] = [run];
    members.set(run.groupId, group);
    entries.push({ kind: "group", groupId: run.groupId, runs: group });
  }
  return entries.map((entry) => (entry.kind === "group" && entry.runs.length === 1 ? { kind: "single", run: entry.runs[0] } : entry));
}

/** True when any member is still in flight — the whole group card stays in
 * the Running section until its last member settles. */
export function agentEntryRunning(entry: AgentPanelEntry): boolean {
  return entry.kind === "group" ? entry.runs.some((run) => run.status === "running") : entry.run.status === "running";
}

/**
 * One parallel round's `task` runs as a single card — the panel counterpart
 * of the chat's `SubagentGroupCard`: header with agent count, per-agent
 * status dots, group elapsed and summed tokens, then a compact per-agent
 * table (agent / tokens / tool uses / time) whose rows expand to that run's
 * full transcript. The stop square cancels every still-running member.
 */
function AgentGroupCard({ runs }: { runs: SubagentRun[] }) {
  const { t } = useT();
  const groupStatus = resolveGroupStatus(runs.map((run) => run.status));
  const running = groupStatus === "running";
  const [openTaskId, setOpenTaskId] = useState<string | null>(null);
  useLiveTick(running);

  const startedAt = Math.min(...runs.map((run) => run.startedAt));
  const endTimes = runs.map((run) => run.finishedAt);
  const settledAt = !running && endTimes.every((time) => time !== undefined) ? Math.max(...(endTimes as number[])) : null;
  const totalTokens = runs.reduce((sum, run) => sum + (run.usage?.totalTokens ?? 0), 0);

  const stopAll = () => {
    for (const run of runs) {
      if (run.status === "running" && run.cancelId) cancelSubagentRun(run.cancelId);
    }
  };

  return (
    <div className="rounded-xl border border-border bg-surface-2 p-3">
      <div className="flex items-center gap-2">
        <span className="min-w-0 flex-1 truncate text-sm font-medium leading-snug text-foreground">
          {t("BackgroundTasksPanel.agentGroupTitle", { count: runs.length })}
        </span>
        <span className="flex shrink-0 items-center gap-1" aria-hidden>
          {runs.map((run) => (
            <span key={run.taskId} className={`h-1.5 w-1.5 rounded-full ${dotClass(run.status)}`} />
          ))}
        </span>
        {running ? (
          <IconButton size="sm" aria-label={t("BackgroundTasksPanel.stopAllAriaLabel")} onClick={stopAll}>
            <Square size={12} />
          </IconButton>
        ) : (
          <StatusPill tone={subagentStatusTone(groupStatus)}>{subagentStatusLabel(t, groupStatus)}</StatusPill>
        )}
      </div>
      <div className="mt-1 flex items-center gap-2 text-xs">
        <span className="text-muted">{t("BackgroundTasksPanel.agentKindLabel")}</span>
        <span className="text-faint">{formatElapsed((settledAt ?? Date.now()) - startedAt)}</span>
        <span className="text-faint">{t("BackgroundTasksPanel.tokenUsage", { count: formatCompactTokens(totalTokens) })}</span>
        {running && <Loader2 size={11} className="shrink-0 animate-spin text-warning" />}
      </div>
      <table className="mt-2 w-full table-fixed border-collapse text-xs">
        <thead>
          <tr className="text-left text-[11px] uppercase tracking-wider text-faint">
            <th scope="col" className="w-1/2 py-1 pr-2 font-medium">{t("BackgroundTasksPanel.tableAgentHeader")}</th>
            <th scope="col" className="py-1 pr-2 text-right font-medium">{t("BackgroundTasksPanel.tableTokensHeader")}</th>
            <th scope="col" className="py-1 pr-2 text-right font-medium">{t("BackgroundTasksPanel.tableToolsHeader")}</th>
            <th scope="col" className="py-1 text-right font-medium">{t("BackgroundTasksPanel.tableTimeHeader")}</th>
          </tr>
        </thead>
        <tbody>
          {runs.map((run) => {
            const open = openTaskId === run.taskId;
            return (
              <Fragment key={run.taskId}>
                <tr className="border-t border-border">
                  <td className="py-1 pr-2">
                    <button
                      type="button"
                      aria-expanded={open}
                      onClick={() => setOpenTaskId(open ? null : run.taskId)}
                      className="flex w-full min-w-0 cursor-pointer items-center gap-1.5 text-left text-foreground transition-colors duration-150 hover:text-accent focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-accent motion-reduce:transition-none"
                    >
                      <span className={`h-1.5 w-1.5 shrink-0 rounded-full ${dotClass(run.status)}`} aria-hidden />
                      <span className="min-w-0 truncate">{run.description}</span>
                    </button>
                  </td>
                  <td className="py-1 pr-2 text-right font-mono text-muted">{formatCompactTokens(run.usage?.totalTokens ?? 0)}</td>
                  <td className="py-1 pr-2 text-right font-mono text-muted">{run.toolCallCount}</td>
                  <td className="py-1 text-right font-mono text-muted">{formatElapsed((run.finishedAt ?? Date.now()) - run.startedAt)}</td>
                </tr>
                {open && (
                  <tr>
                    <td colSpan={4} className="pb-2">
                      <div className="space-y-1.5 border-t border-border pt-2">
                        <TranscriptRowsView rows={buildTranscriptRows(run.liveMessages)} />
                      </div>
                    </td>
                  </tr>
                )}
              </Fragment>
            );
          })}
        </tbody>
      </table>
    </div>
  );
}

export interface BackgroundTasksPanelProps {
  /** The chat session whose persisted subagent-run stats are restored into
   * the Finished section after a restart (see `ChatSession.subagentRunMeta`). */
  sessionId: string;
  /** Closes the hosting right-sidebar tab. */
  onClose?: () => void;
}

export function BackgroundTasksPanel({ sessionId, onClose }: BackgroundTasksPanelProps) {
  const { t } = useT();
  // Both selectors build a fresh array per call — `useShallow` keeps the
  // uncached snapshots from re-render-looping (React's "getSnapshot should be
  // cached" guard), same as the other array-selector consumers.
  const runningShell = useBackgroundShellStore(useShallow(selectRunningShellTasks));
  const finishedShell = useBackgroundShellStore(useShallow(selectFinishedShellTasks));
  const shellError = useBackgroundShellStore((state) => state.error);
  const liveSubagentRuns = useSubagentStore(useShallow(selectSubagentRunList));

  // Rust owns the processes, so the panel asks for the current truth on mount
  // rather than assuming this window started everything it can see.
  useEffect(() => {
    void useBackgroundShellStore.getState().initialize();
  }, []);

  // Finished runs persisted with the ACTIVE session (see
  // `ChatSession.subagentRunMeta`) — what keeps the Finished section
  // populated after a restart wipes the transient store. Field-level
  // subscriptions (not the whole session) so streaming message updates don't
  // re-render the panel; both references only change on `setSubagentRun`.
  const persistedMeta = useSessionStore((state) => state.sessions.find((s) => s.id === sessionId)?.subagentRunMeta);
  const persistedTranscripts = useSessionStore((state) => state.sessions.find((s) => s.id === sessionId)?.subagentRuns);
  const subagentRuns = useMemo(() => {
    const liveIds = new Set(liveSubagentRuns.map((run) => run.taskId));
    const restored: SubagentRun[] = Object.entries(persistedMeta ?? {})
      .filter(([taskId]) => !liveIds.has(taskId))
      .map(([taskId, meta]) => ({
        sessionId,
        taskId,
        // Empty cancelId: a restored run is terminal, Stop stays disabled and
        // `cancelSubagentRun("")` would be a no-op regardless.
        cancelId: "",
        groupId: meta.groupId,
        description: meta.description,
        profile: meta.profile,
        status: meta.status,
        startedAt: meta.startedAt,
        finishedAt: meta.finishedAt,
        lastActivity: "",
        toolCallCount: meta.toolCallCount,
        usage: meta.usage,
        liveMessages: persistedTranscripts?.[taskId] ?? [],
      }));
    return [...liveSubagentRuns, ...restored].sort((a, b) => b.startedAt - a.startedAt);
  }, [liveSubagentRuns, persistedMeta, persistedTranscripts, sessionId]);

  const runningCount = useBackgroundShellStore(selectRunningShellTaskCount) + useSubagentStore(selectRunningSubagentCount);
  const [finishedOpen, setFinishedOpen] = useState(false);

  // One parallel round's runs collapse into a single group entry — the
  // group stays in Running until its LAST member settles (see
  // `agentEntryRunning`), so a half-finished round never splits across the
  // two sections.
  const agentEntries = groupAgentRuns(subagentRuns);
  const runningAgentEntries = agentEntries.filter(agentEntryRunning);
  const finishedAgentEntries = agentEntries.filter((entry) => !agentEntryRunning(entry));

  type Entry =
    | { kind: "shell"; at: number; task: BackgroundShellTask }
    | { kind: "agent"; at: number; run: SubagentRun }
    | { kind: "agentGroup"; at: number; groupId: string; runs: SubagentRun[] };
  const byNewest = (a: Entry, b: Entry) => b.at - a.at;
  const toAgentEntry = (entry: AgentPanelEntry, finished: boolean): Entry =>
    entry.kind === "group"
      ? {
          kind: "agentGroup",
          at: Math.max(...entry.runs.map((run) => (finished ? (run.finishedAt ?? run.startedAt) : run.startedAt))),
          groupId: entry.groupId,
          runs: entry.runs,
        }
      : { kind: "agent", at: finished ? (entry.run.finishedAt ?? entry.run.startedAt) : entry.run.startedAt, run: entry.run };
  const runningEntries: Entry[] = [
    ...runningShell.map((task): Entry => ({ kind: "shell", at: task.started_at_ms, task })),
    ...runningAgentEntries.map((entry) => toAgentEntry(entry, false)),
  ].sort(byNewest);
  const finishedEntries: Entry[] = [
    ...finishedShell.map((task): Entry => ({ kind: "shell", at: task.finished_at_ms ?? task.started_at_ms, task })),
    ...finishedAgentEntries.map((entry) => toAgentEntry(entry, true)),
  ].sort(byNewest);
  const finishedCount = finishedEntries.length;
  const hasAnyTask = runningEntries.length > 0 || finishedCount > 0;

  // "Clear" empties the Finished list without stopping anything: finished
  // shell entries drop out of the Rust registry, finished agent entries drop
  // from the transient store and the session's persisted stats — their
  // transcripts stay in `ChatSession.subagentRuns` for the inline rows.
  const clearFinished = () => {
    void useBackgroundShellStore.getState().clearFinished();
    useSubagentStore.getState().clearFinished();
    useSessionStore.getState().clearSubagentRunMeta(sessionId);
  };

  const renderEntry = (entry: Entry) =>
    entry.kind === "shell" ? (
      <ShellTaskCard key={`shell-${entry.task.id}`} task={entry.task} />
    ) : entry.kind === "agentGroup" ? (
      <AgentGroupCard key={`group-${entry.groupId}`} runs={entry.runs} />
    ) : (
      <AgentTaskCard key={`agent-${entry.run.taskId}`} run={entry.run} />
    );

  return (
    <aside className="flex h-full min-h-0 w-full flex-col bg-surface">
      <div className="flex h-11 shrink-0 items-center justify-between border-b border-border px-3">
        <span className="flex items-center gap-1.5 text-[11px] font-semibold uppercase tracking-wider text-faint">
          {t("BackgroundTasksPanel.title")}
          {runningCount > 0 && <StatusPill tone="warning">{runningCount}</StatusPill>}
        </span>
        {onClose && (
          <IconButton size="sm" onClick={onClose} aria-label={t("BackgroundTasksPanel.closeAriaLabel")}>
            <X size={16} />
          </IconButton>
        )}
      </div>

      <div className="flex min-h-0 flex-1 flex-col gap-2 overflow-y-auto p-3">
        {shellError && (
          <p className="rounded-md border border-danger bg-danger-soft p-2 text-xs text-danger">{shellError}</p>
        )}

        {!hasAnyTask && (
          <div className="flex flex-1 flex-col items-center justify-center py-12 text-center">
            <p className="text-xs text-faint">{t("BackgroundTasksPanel.emptyState")}</p>
          </div>
        )}

        {runningEntries.length > 0 && (
          <div className="text-[11px] font-medium uppercase tracking-wider text-faint">{t("BackgroundTasksPanel.runningHeading")}</div>
        )}
        {runningEntries.map(renderEntry)}

        {finishedCount > 0 && (
          <div className="mt-1 flex items-center justify-between">
            <button
              type="button"
              onClick={() => setFinishedOpen((prev) => !prev)}
              className="flex cursor-pointer items-center gap-1.5 text-[11px] font-medium uppercase tracking-wider text-faint transition-colors duration-150 hover:text-foreground"
            >
              {t("BackgroundTasksPanel.finishedHeading")}
              <span>{finishedCount}</span>
              <ChevronRight size={12} className={`transition-transform duration-150 ${finishedOpen ? "rotate-90" : ""}`} />
            </button>
            <Button variant="ghost" size="sm" onClick={clearFinished}>
              {t("BackgroundTasksPanel.clearButton")}
            </Button>
          </div>
        )}
        {finishedOpen && finishedEntries.map(renderEntry)}
      </div>
    </aside>
  );
}

export default BackgroundTasksPanel;
