import { useEffect, useMemo, useState } from "react";
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
import { cancelSubagentRun } from "../../lib/subagent";
import { formatCompactTokens, formatElapsed } from "../../lib/taskFormat";
import { extractChildToolCalls } from "../Chat/SubagentRow";
import { ToolCallRow } from "../Chat/MessageList";

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

function shellStatusLabel(task: BackgroundShellTask): string {
  switch (task.status) {
    case "running":
      return "Running";
    case "exited":
      return "Exited";
    case "killed":
      return "Stopped";
    case "error":
      return task.exit_code !== null ? `Exit ${task.exit_code}` : "Failed";
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

function subagentStatusLabel(status: SubagentStatus): string {
  switch (status) {
    case "running":
      return "Running";
    case "done":
      return "Completed";
    case "error":
      return "Failed";
    default:
      return "Cancelled";
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
          <IconButton size="sm" aria-label={`Stop "${task.command}"`} onClick={() => void useBackgroundShellStore.getState().kill(task.id)}>
            <Square size={12} />
          </IconButton>
        ) : (
          <StatusPill tone={shellStatusTone(task.status)}>{shellStatusLabel(task)}</StatusPill>
        )}
      </div>
      <div className="mt-1 flex items-center gap-2 text-xs">
        <span className="text-muted">Shell</span>
        <span className="truncate text-faint">{folderName(task.cwd)}</span>
        <span className="shrink-0 text-faint">{elapsed}</span>
        {running && <Loader2 size={11} className="shrink-0 animate-spin text-warning" />}
      </div>
      <div className="mt-0.5 flex flex-wrap items-center gap-x-3 gap-y-0.5 text-xs text-faint">
        {!running && task.exit_code !== null && <span>exit {task.exit_code}</span>}
        {tail.length > 0 && (
          <button type="button" onClick={() => setShowOutput((prev) => !prev)} className="cursor-pointer text-accent hover:underline">
            {showOutput ? "Hide output" : "View output"}
          </button>
        )}
      </div>
      {!showOutput && running && lastLine && <div className="mt-1 truncate font-mono text-[11px] text-faint">{lastLine}</div>}
      {showOutput && (
        <div className="mt-2 border-t border-border pt-2">
          {task.output_truncated && <p className="mb-1 text-[11px] text-faint">Earlier output dropped — showing the retained tail.</p>}
          <pre className="max-h-64 overflow-auto whitespace-pre-wrap break-words rounded-md border border-border bg-background p-2 font-mono text-[11px] text-foreground">
            {tail}
          </pre>
        </div>
      )}
    </div>
  );
}

/**
 * One `task`-tool subagent run — the model's own delegated work. Same card
 * shape as `ShellTaskCard` above so the two kinds read as one list: title,
 * stop square while running, then "Agent · elapsed" and the token/tool-use
 * counts, with the transcript (and final report) expanding inline through
 * the same `ToolCallRow` the inline `SubagentRow` uses.
 */
function AgentTaskCard({ run }: { run: SubagentRun }) {
  const running = run.status === "running";
  const [showTranscript, setShowTranscript] = useState(false);
  useLiveTick(running);

  const childToolCalls = extractChildToolCalls(run.liveMessages);
  const report = [...run.liveMessages].reverse().find((message) => message.role === "assistant" && !message.tool_calls);
  const reportText = typeof report?.content === "string" ? report.content : null;

  return (
    <div className="rounded-xl border border-border bg-surface-2 p-3">
      <div className="flex items-start gap-2">
        <span className="min-w-0 flex-1 text-sm font-medium leading-snug text-foreground">{run.description}</span>
        {running ? (
          <IconButton size="sm" aria-label={`Stop "${run.description}"`} onClick={() => cancelSubagentRun(run.cancelId)}>
            <Square size={12} />
          </IconButton>
        ) : (
          run.status !== "done" && <StatusPill tone={subagentStatusTone(run.status)}>{subagentStatusLabel(run.status)}</StatusPill>
        )}
      </div>
      <div className="mt-1 flex items-center gap-2 text-xs">
        <span className="text-muted">Agent</span>
        <span className="text-faint">{formatElapsed((run.finishedAt ?? Date.now()) - run.startedAt)}</span>
        {running && <Loader2 size={11} className="shrink-0 animate-spin text-warning" />}
      </div>
      <div className="mt-0.5 flex flex-wrap items-center gap-x-3 gap-y-0.5 text-xs text-faint">
        {run.usage && <span>{formatCompactTokens(run.usage.totalTokens)} tokens</span>}
        <span>
          {run.toolCallCount} tool use{run.toolCallCount === 1 ? "" : "s"}
        </span>
        {(childToolCalls.length > 0 || reportText) && (
          <button type="button" onClick={() => setShowTranscript((prev) => !prev)} className="cursor-pointer text-accent hover:underline">
            {showTranscript ? "Hide transcript" : "View transcript"}
          </button>
        )}
      </div>
      {running && run.lastActivity && <div className="mt-1 truncate font-mono text-[11px] text-faint">{run.lastActivity}</div>}
      {showTranscript && (
        <div className="mt-2 space-y-1.5 border-t border-border pt-2">
          {childToolCalls.map((row) => (
            <ToolCallRow key={row.key} name={row.name} args={row.args} result={row.result} />
          ))}
          {!running && reportText && (
            <p className="whitespace-pre-wrap rounded-md border border-border bg-background p-2 text-xs text-foreground">{reportText}</p>
          )}
        </div>
      )}
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

  const runningAgents = subagentRuns.filter((run) => run.status === "running");
  const finishedAgents = subagentRuns.filter((run) => run.status !== "running");

  type Entry = { kind: "shell"; at: number; task: BackgroundShellTask } | { kind: "agent"; at: number; run: SubagentRun };
  const byNewest = (a: Entry, b: Entry) => b.at - a.at;
  const runningEntries: Entry[] = [
    ...runningShell.map((task): Entry => ({ kind: "shell", at: task.started_at_ms, task })),
    ...runningAgents.map((run): Entry => ({ kind: "agent", at: run.startedAt, run })),
  ].sort(byNewest);
  const finishedEntries: Entry[] = [
    ...finishedShell.map((task): Entry => ({ kind: "shell", at: task.finished_at_ms ?? task.started_at_ms, task })),
    ...finishedAgents.map((run): Entry => ({ kind: "agent", at: run.finishedAt ?? run.startedAt, run })),
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
    ) : (
      <AgentTaskCard key={`agent-${entry.run.taskId}`} run={entry.run} />
    );

  return (
    <aside className="flex h-full min-h-0 w-full flex-col bg-surface">
      <div className="flex h-11 shrink-0 items-center justify-between border-b border-border px-3">
        <span className="flex items-center gap-1.5 text-[11px] font-semibold uppercase tracking-wider text-faint">
          Background tasks
          {runningCount > 0 && <StatusPill tone="warning">{runningCount}</StatusPill>}
        </span>
        {onClose && (
          <IconButton size="sm" onClick={onClose} aria-label="Close background tasks panel">
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
            <p className="text-xs text-faint">Background commands and agent runs appear here</p>
          </div>
        )}

        {runningEntries.length > 0 && <div className="text-[11px] font-medium uppercase tracking-wider text-faint">Running</div>}
        {runningEntries.map(renderEntry)}

        {finishedCount > 0 && (
          <div className="mt-1 flex items-center justify-between">
            <button
              type="button"
              onClick={() => setFinishedOpen((prev) => !prev)}
              className="flex cursor-pointer items-center gap-1.5 text-[11px] font-medium uppercase tracking-wider text-faint transition-colors duration-150 hover:text-foreground"
            >
              Finished
              <span>{finishedCount}</span>
              <ChevronRight size={12} className={`transition-transform duration-150 ${finishedOpen ? "rotate-90" : ""}`} />
            </button>
            <Button variant="ghost" size="sm" onClick={clearFinished}>
              Clear
            </Button>
          </div>
        )}
        {finishedOpen && finishedEntries.map(renderEntry)}
      </div>
    </aside>
  );
}

export default BackgroundTasksPanel;
