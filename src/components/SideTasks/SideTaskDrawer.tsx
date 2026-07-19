import { useEffect, useMemo, useState } from "react";
import { useShallow } from "zustand/react/shallow";
import {
  Archive,
  ArchiveRestore,
  CheckCircle2,
  ChevronRight,
  CircleAlert,
  CircleDot,
  ExternalLink,
  ListTree,
  Loader2,
  Pause,
  Play,
  PanelRight,
  PanelRightClose,
  Plus,
  RotateCcw,
  Square,
  Upload,
  X,
  XCircle,
} from "lucide-react";

import { Button, IconButton, StatusPill, type PillTone } from "../ui";
import {
  selectArchivedSideTasks,
  selectRunningSideTaskCount,
  selectVisibleSideTasks,
  useSideTaskStore,
  type SideTaskRecord,
  type SideTaskStatus,
  type SideTaskToolOutcome,
} from "../../store/sideTaskStore";
import {
  selectRunningSubagentCount,
  selectSubagentRunList,
  useSubagentStore,
  type SubagentRun,
  type SubagentStatus,
} from "../../store/subagentStore";
import { useSessionStore } from "../../store/sessionStore";
import { usePermissionStore } from "../../store/permissionStore";
import {
  cancelSideTask,
  openSideTaskAsFullChat,
  pauseSideTask,
  promoteSideTask,
  resumeSideTask,
  retrySideTask,
} from "../../lib/sideTaskRunner";
import { cancelSubagentRun } from "../../lib/subagent";
import { formatCompactTokens, formatElapsed } from "../../lib/taskFormat";
import { extractChildToolCalls } from "../Chat/SubagentRow";
import { ToolCallRow } from "../Chat/MessageList";
import { SideTaskComposer } from "./SideTaskComposer";

function statusTone(status: SideTaskStatus): PillTone {
  switch (status) {
    case "running":
    case "queued":
      return "warning";
    case "paused":
      return "neutral";
    case "completed":
      return "success";
    case "error":
      return "danger";
    case "cancelled":
      return "neutral";
    default:
      return "neutral";
  }
}

function statusLabel(status: SideTaskStatus): string {
  switch (status) {
    case "queued":
      return "Queued";
    case "running":
      return "Running";
    case "paused":
      return "Paused";
    case "completed":
      return "Completed";
    case "error":
      return "Failed";
    case "cancelled":
      return "Cancelled";
    default:
      return status;
  }
}

function toolOutcomeIcon(outcome: SideTaskToolOutcome) {
  switch (outcome) {
    case "pending":
      return <Loader2 size={12} className="shrink-0 animate-spin text-faint" />;
    case "succeeded":
      return <CheckCircle2 size={12} className="shrink-0 text-success" />;
    case "failed":
      return <XCircle size={12} className="shrink-0 text-danger" />;
    case "denied":
      return <CircleAlert size={12} className="shrink-0 text-warning" />;
    case "cancelled":
      return <Square size={12} className="shrink-0 text-faint" />;
    default:
      return <CircleDot size={12} className="shrink-0 text-faint" />;
  }
}

function formatRelativeTime(ms: number): string {
  const diff = Date.now() - ms;
  if (diff < 5_000) return "just now";
  if (diff < 60_000) return `${Math.floor(diff / 1000)}s ago`;
  if (diff < 3_600_000) return `${Math.floor(diff / 60_000)}m ago`;
  if (diff < 86_400_000) return `${Math.floor(diff / 3_600_000)}h ago`;
  return new Date(ms).toLocaleDateString();
}

function sourceKindLabel(kind: SideTaskRecord["source"]["kind"]): string {
  switch (kind) {
    case "chat_message":
      return "Chat message";
    case "selected_files":
      return "Selected files";
    case "terminal_output":
      return "Terminal output";
    case "browser_evidence":
      return "Browser evidence";
    case "mcp_result":
      return "MCP result";
    default:
      return "Manual";
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

/**
 * Full-width card for a `task`-tool subagent run — Claude-Code-desktop-style:
 * title with a square stop button while running, "Agent · elapsed" line,
 * then "tokens · tool uses · View transcript". The transcript (and final
 * report) expands inline, reusing the same `ToolCallRow` the inline
 * `SubagentRow` uses.
 */
function AgentTaskCard({ run }: { run: SubagentRun }) {
  const running = run.status === "running";
  const [showTranscript, setShowTranscript] = useState(false);
  // 1s tick while running so the elapsed label counts up live.
  const [, setTick] = useState(0);
  useEffect(() => {
    if (!running) return;
    const interval = setInterval(() => setTick((value) => value + 1), 1000);
    return () => clearInterval(interval);
  }, [running]);

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
          <button
            type="button"
            onClick={() => setShowTranscript((prev) => !prev)}
            className="cursor-pointer text-accent hover:underline"
          >
            {showTranscript ? "Hide transcript" : "View transcript"}
          </button>
        )}
      </div>
      {running && run.lastActivity && (
        <div className="mt-1 truncate font-mono text-[11px] text-faint">{run.lastActivity}</div>
      )}
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

/**
 * Full-width card for a side task — same Claude-Code card shape as
 * `AgentTaskCard`. The stop square cancels while active; "Details" expands
 * the full `SideTaskDetail` (pause/resume/retry/promote/archive and the
 * task's evidence) inline.
 */
function SideTaskCard({ task, expanded, onToggleDetails }: { task: SideTaskRecord; expanded: boolean; onToggleDetails: () => void }) {
  const active = task.status === "running" || task.status === "queued" || task.status === "paused";
  return (
    <div className="rounded-xl border border-border bg-surface-2 p-3">
      <div className="flex items-start gap-2">
        <span className="min-w-0 flex-1 text-sm font-medium leading-snug text-foreground">{task.title}</span>
        {task.status === "queued" || task.status === "paused" ? (
          <StatusPill tone={statusTone(task.status)}>{statusLabel(task.status)}</StatusPill>
        ) : null}
        {active ? (
          <IconButton size="sm" aria-label={`Stop "${task.title}"`} onClick={() => cancelSideTask(task.id)}>
            <Square size={12} />
          </IconButton>
        ) : (
          task.status !== "completed" && <StatusPill tone={statusTone(task.status)}>{statusLabel(task.status)}</StatusPill>
        )}
      </div>
      <div className="mt-1 flex items-center gap-2 text-xs">
        <span className="text-muted">Side task</span>
        <span className="truncate text-faint">{sourceKindLabel(task.source.kind)}</span>
        <span className="shrink-0 text-faint">{formatRelativeTime(task.updatedAt)}</span>
        {task.status === "running" && <Loader2 size={11} className="shrink-0 animate-spin text-warning" />}
      </div>
      <div className="mt-0.5 flex flex-wrap items-center gap-x-3 gap-y-0.5 text-xs text-faint">
        {task.usage && <span>{formatCompactTokens(task.usage.totalTokens)} tokens</span>}
        <span>
          {task.toolEvidence.length} tool use{task.toolEvidence.length === 1 ? "" : "s"}
        </span>
        <button type="button" onClick={onToggleDetails} className="cursor-pointer text-accent hover:underline">
          {expanded ? "Hide details" : "Details"}
        </button>
      </div>
      {expanded && (
        <div className="mt-2 border-t border-border pt-1">
          <SideTaskDetail task={task} />
        </div>
      )}
    </div>
  );
}

/** Shared side-task lifecycle/evidence view. Agent Inbox reuses this exact
 * component so pause/resume/cancel/retry/archive/promote behavior cannot
 * drift between the two task surfaces. */
export function SideTaskDetail({ task }: { task: SideTaskRecord }) {
  const approvalsWaiting = usePermissionStore(
    (state) => state.queue.filter((request) => request.agent_label === `Side task "${task.title}"`).length,
  );

  const running = task.status === "running";
  const paused = task.status === "paused";
  const terminal = task.status === "completed" || task.status === "error" || task.status === "cancelled";
  const canPause = running;
  const canResume = paused;
  const canCancel = running || paused || task.status === "queued";
  const canRetry = terminal;
  const canPromote = task.status === "completed" && task.finalReport !== null;

  return (
    <div className="flex min-h-0 flex-1 flex-col gap-3 overflow-y-auto p-3">
      <div>
        <div className="flex items-center gap-2">
          <h3 className="min-w-0 flex-1 truncate text-sm font-semibold text-foreground">{task.title}</h3>
          <StatusPill tone={statusTone(task.status)}>{statusLabel(task.status)}</StatusPill>
        </div>
        <p className="mt-1 text-xs text-faint">
          {sourceKindLabel(task.source.kind)} · {task.profile === "code" ? "Can edit files" : "Read-only"} · {task.modelLabel}
        </p>
        {task.retryOf && <p className="mt-0.5 text-[11px] text-faint">Retry of a previous attempt</p>}
      </div>

      <div className="flex flex-wrap gap-1.5">
        <Button variant="secondary" size="sm" onClick={() => canPause && pauseSideTask(task.id)} disabled={!canPause}>
          <Pause size={13} /> Pause
        </Button>
        <Button variant="secondary" size="sm" onClick={() => canResume && resumeSideTask(task.id)} disabled={!canResume}>
          <Play size={13} /> Resume
        </Button>
        <Button variant="danger" size="sm" onClick={() => canCancel && cancelSideTask(task.id)} disabled={!canCancel}>
          <Square size={13} /> Cancel
        </Button>
        <Button
          variant="secondary"
          size="sm"
          onClick={() => {
            if (!canRetry) return;
            const retryId = retrySideTask(task.id);
            if (retryId) useSideTaskStore.getState().selectTask(retryId);
          }}
          disabled={!canRetry}
        >
          <RotateCcw size={13} /> Retry
        </Button>
        {task.archivedAt ? (
          <Button variant="secondary" size="sm" onClick={() => useSideTaskStore.getState().unarchive(task.id)}>
            <ArchiveRestore size={13} /> Unarchive
          </Button>
        ) : (
          <Button variant="secondary" size="sm" onClick={() => useSideTaskStore.getState().archive(task.id)} disabled={!terminal}>
            <Archive size={13} /> Archive
          </Button>
        )}
        <Button variant="secondary" size="sm" onClick={() => openSideTaskAsFullChat(task.id)}>
          <ExternalLink size={13} /> Open as full task
        </Button>
        <Button variant="primary" size="sm" onClick={() => promoteSideTask(task.id)} disabled={!canPromote || task.promotedAt !== null}>
          <Upload size={13} /> {task.promotedAt ? "Promoted" : "Promote to chat"}
        </Button>
      </div>

      {approvalsWaiting > 0 && (
        <div className="flex items-center gap-1.5 rounded-md border border-warning bg-warning-soft px-2.5 py-1.5 text-xs text-warning">
          <CircleAlert size={13} className="shrink-0" />
          {approvalsWaiting === 1 ? "1 approval waiting" : `${approvalsWaiting} approvals waiting`}
        </div>
      )}

      {task.usage && (
        <p className="text-[11px] text-faint">
          {task.usage.totalTokens.toLocaleString("en-US")} tokens · {task.toolEvidence.length} tool call
          {task.toolEvidence.length === 1 ? "" : "s"}
        </p>
      )}

      <div>
        <div className="mb-1 text-xs font-semibold uppercase tracking-wider text-faint">Prompt snapshot</div>
        <p className="whitespace-pre-wrap rounded-md border border-border bg-surface-2 p-2 text-xs text-muted">{task.prompt}</p>
      </div>

      {task.toolEvidence.length > 0 && (
        <div>
          <div className="mb-1 text-xs font-semibold uppercase tracking-wider text-faint">Tools used</div>
          <div className="flex flex-col gap-1">
            {task.toolEvidence.map((evidence) => (
              <div key={evidence.id} className="flex items-start gap-1.5 rounded-md border border-border bg-surface-2 p-2 text-[11px]">
                {toolOutcomeIcon(evidence.outcome)}
                <div className="min-w-0 flex-1">
                  <div className="truncate font-mono text-foreground">{evidence.name}</div>
                  {evidence.argsPreview && <div className="truncate text-faint">{evidence.argsPreview}</div>}
                </div>
              </div>
            ))}
          </div>
        </div>
      )}

      {task.artifacts.length > 0 && (
        <div>
          <div className="mb-1 text-xs font-semibold uppercase tracking-wider text-faint">Artifacts</div>
          <div className="flex flex-col gap-1">
            {task.artifacts.map((artifact) => (
              <div key={artifact.id} className="rounded-md border border-border bg-surface-2 p-2 text-[11px]">
                <div className="flex items-center gap-1.5 font-mono text-foreground">
                  <ListTree size={12} className="shrink-0 text-faint" />
                  <span className="truncate">{artifact.label}</span>
                </div>
              </div>
            ))}
          </div>
        </div>
      )}

      {task.error && (
        <div>
          <div className="mb-1 text-xs font-semibold uppercase tracking-wider text-faint">Error</div>
          <p className="whitespace-pre-wrap rounded-md border border-danger bg-danger-soft p-2 text-xs text-danger">{task.error}</p>
        </div>
      )}

      {task.finalReport && (
        <div>
          <div className="mb-1 text-xs font-semibold uppercase tracking-wider text-faint">Report</div>
          <p className="whitespace-pre-wrap rounded-md border border-border bg-surface-2 p-2 text-xs text-foreground">{task.finalReport}</p>
        </div>
      )}
    </div>
  );
}

export interface SideTaskDrawerProps {
  /** The chat session a manually-started (no seed) side task is attributed
   * to — passed through to `openComposer`'s default seed when the drawer's
   * own "+ New" button is used instead of a message action. */
  sessionId: string;
  /** Renders as a fill-the-parent tab body (the right sidebar's Side-tasks
   * tab): no own width, always the open content layout, no collapse
   * affordance — the hosting region owns sizing and visibility. The
   * hosting region's own fullscreen toggle covers this case, so this
   * component doesn't need one of its own. */
  embedded?: boolean;
  /** Closes the hosting tab. Only meaningful (and only rendered) when
   * `embedded` — the standalone drawer closes via its own collapse toggle
   * instead. */
  onClose?: () => void;
}

/**
 * Collapsible right-side panel that COEXISTS with the main chat (unlike
 * `RunCenter`/`BrowserWorkbench`, which swap out the chat pane entirely) —
 * this is deliberate: ROADMAP.md's "Side Tasks" item asks for work that is
 * "more visible than a hidden tool call" and runs "without blocking the main
 * chat", so the chat has to actually stay on screen next to it. Mirrors
 * `App.tsx`'s own workspace-panel collapse idiom (`w-96` expanded / `w-12`
 * collapsed) for visual consistency with the app's other side panel.
 */
export function SideTaskDrawer({ sessionId, embedded, onClose }: SideTaskDrawerProps) {
  const open = useSideTaskStore((state) => state.drawerOpen);
  const visualOpen = open || Boolean(embedded);
  const toggleDrawer = useSideTaskStore((state) => state.toggleDrawer);
  const composerOpen = useSideTaskStore((state) => state.composerOpen);
  const openComposer = useSideTaskStore((state) => state.openComposer);
  // Both selectors build a fresh array per call — `useShallow` keeps the
  // uncached snapshots from re-render-looping (React's "getSnapshot should
  // be cached" guard), same as the other array-selector consumers.
  const visible = useSideTaskStore(useShallow(selectVisibleSideTasks));
  const archived = useSideTaskStore(useShallow(selectArchivedSideTasks));
  const liveSubagentRuns = useSubagentStore(useShallow(selectSubagentRunList));
  // Finished runs persisted with the ACTIVE session (see
  // `ChatSession.subagentRunMeta`) — what keeps the Finished section
  // populated after a restart wipes the transient store. Field-level
  // subscriptions (not the whole session) so streaming message updates
  // don't re-render the drawer; both references only change on
  // `setSubagentRun`.
  const persistedMeta = useSessionStore((state) => state.sessions.find((s) => s.id === sessionId)?.subagentRunMeta);
  const persistedTranscripts = useSessionStore((state) => state.sessions.find((s) => s.id === sessionId)?.subagentRuns);
  const subagentRuns = useMemo(() => {
    const liveIds = new Set(liveSubagentRuns.map((run) => run.taskId));
    const restored: SubagentRun[] = Object.entries(persistedMeta ?? {})
      .filter(([taskId]) => !liveIds.has(taskId))
      .map(([taskId, meta]) => ({
        sessionId,
        taskId,
        // Empty cancelId: a restored run is terminal, Stop stays disabled
        // and `cancelSubagentRun("")` would be a no-op regardless.
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
  const runningCount = useSideTaskStore(selectRunningSideTaskCount) + useSubagentStore(selectRunningSubagentCount);
  const selectedTaskId = useSideTaskStore((state) => state.selectedTaskId);
  // Which side-task card has its Details section open. Externally-driven
  // selection (the composer's create, retry's auto-select) opens that
  // task's card so the caller's intent stays visible.
  const [expandedSideTaskId, setExpandedSideTaskId] = useState<string | null>(null);
  const [finishedOpen, setFinishedOpen] = useState(false);
  useEffect(() => {
    if (selectedTaskId !== null) setExpandedSideTaskId(selectedTaskId);
  }, [selectedTaskId]);

  const showArchived = useMemo(() => archived.length > 0, [archived]);
  // Running = anything that could still produce work (a paused side task
  // resumes); Finished = terminal, kept until Clear (or archive for side
  // tasks). Both kinds interleave newest-first, Claude-Code-panel style.
  const runningSideTasks = visible.filter((task) => task.status === "running" || task.status === "queued" || task.status === "paused");
  const finishedSideTasks = visible.filter((task) => task.status === "completed" || task.status === "error" || task.status === "cancelled");
  const runningAgents = subagentRuns.filter((run) => run.status === "running");
  const finishedAgents = subagentRuns.filter((run) => run.status !== "running");
  const hasAnyTask = visible.length > 0 || subagentRuns.length > 0;
  const finishedCount = finishedSideTasks.length + finishedAgents.length;

  type DrawerEntry = { kind: "side"; at: number; task: SideTaskRecord } | { kind: "agent"; at: number; run: SubagentRun };
  const byNewest = (a: DrawerEntry, b: DrawerEntry) => b.at - a.at;
  const runningEntries: DrawerEntry[] = [
    ...runningSideTasks.map((task): DrawerEntry => ({ kind: "side", at: task.updatedAt, task })),
    ...runningAgents.map((run): DrawerEntry => ({ kind: "agent", at: run.startedAt, run })),
  ].sort(byNewest);
  const finishedEntries: DrawerEntry[] = [
    ...finishedSideTasks.map((task): DrawerEntry => ({ kind: "side", at: task.updatedAt, task })),
    ...finishedAgents.map((run): DrawerEntry => ({ kind: "agent", at: run.finishedAt ?? run.startedAt, run })),
  ].sort(byNewest);

  // "Clear" empties the Finished list without touching the conversation:
  // terminal side tasks archive (recoverable), finished agent entries drop
  // from the transient store and the session's persisted stats — their
  // transcripts stay in `ChatSession.subagentRuns` for the inline rows.
  const clearFinished = () => {
    for (const task of finishedSideTasks) useSideTaskStore.getState().archive(task.id);
    useSubagentStore.getState().clearFinished();
    useSessionStore.getState().clearSubagentRunMeta(sessionId);
  };

  return (
    <aside
      className={`flex h-full flex-col bg-surface ${
        embedded
          ? "min-h-0 w-full"
          : `shrink-0 border-l border-border transition-[width] duration-200 ${visualOpen ? "w-96" : "w-12"}`
      }`}
    >
      <div className="flex h-11 shrink-0 items-center justify-between border-b border-border px-3">
        {visualOpen && (
          <span className="flex items-center gap-1.5 text-[11px] font-semibold uppercase tracking-wider text-faint">
            Background tasks
            {runningCount > 0 && <StatusPill tone="warning">{runningCount}</StatusPill>}
          </span>
        )}
        <div className={`flex items-center gap-1 ${visualOpen ? "" : "mx-auto"}`}>
          {visualOpen && (
            <IconButton
              size="sm"
              aria-label="Start a new side task"
              onClick={() =>
                openComposer({
                  title: "",
                  prompt: "",
                  profile: "explore",
                  source: { kind: "manual", label: "Manual", excerpt: "" },
                  sessionId,
                })
              }
            >
              <Plus size={16} />
            </IconButton>
          )}
          {!embedded && (
            <IconButton
              size="sm"
              onClick={toggleDrawer}
              aria-label={open ? "Collapse background tasks panel" : "Expand background tasks panel"}
            >
              {open ? <PanelRightClose size={16} /> : <PanelRight size={16} />}
            </IconButton>
          )}
          {embedded && onClose && (
            <IconButton size="sm" onClick={onClose} aria-label="Close background tasks panel">
              <X size={16} />
            </IconButton>
          )}
        </div>
      </div>

      {!visualOpen && runningCount > 0 && (
        <div className="flex justify-center py-2">
          <Loader2 size={16} className="animate-spin text-warning" />
        </div>
      )}

      {visualOpen && (
        <div className="flex min-h-0 flex-1 flex-col">
          {composerOpen && <SideTaskComposer />}

          <div className="flex min-h-0 flex-1 flex-col gap-2 overflow-y-auto p-3">
            {!hasAnyTask && !composerOpen && (
              <div className="flex flex-1 flex-col items-center justify-center py-12 text-center">
                <p className="text-xs text-faint">Background work appears here</p>
              </div>
            )}

            {runningEntries.length > 0 && (
              <div className="text-[11px] font-medium uppercase tracking-wider text-faint">Running</div>
            )}
            {runningEntries.map((entry) =>
              entry.kind === "agent" ? (
                <AgentTaskCard key={`agent-${entry.run.taskId}`} run={entry.run} />
              ) : (
                <SideTaskCard
                  key={`side-${entry.task.id}`}
                  task={entry.task}
                  expanded={expandedSideTaskId === entry.task.id}
                  onToggleDetails={() => setExpandedSideTaskId((prev) => (prev === entry.task.id ? null : entry.task.id))}
                />
              ),
            )}

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
            {finishedOpen &&
              finishedEntries.map((entry) =>
                entry.kind === "agent" ? (
                  <AgentTaskCard key={`agent-${entry.run.taskId}`} run={entry.run} />
                ) : (
                  <SideTaskCard
                    key={`side-${entry.task.id}`}
                    task={entry.task}
                    expanded={expandedSideTaskId === entry.task.id}
                    onToggleDetails={() => setExpandedSideTaskId((prev) => (prev === entry.task.id ? null : entry.task.id))}
                  />
                ),
              )}
            {finishedOpen && showArchived && (
              <>
                <div className="mt-1 text-[11px] font-medium uppercase tracking-wider text-faint">Archived</div>
                {archived.map((task) => (
                  <SideTaskCard
                    key={`side-${task.id}`}
                    task={task}
                    expanded={expandedSideTaskId === task.id}
                    onToggleDetails={() => setExpandedSideTaskId((prev) => (prev === task.id ? null : task.id))}
                  />
                ))}
              </>
            )}
          </div>
        </div>
      )}
    </aside>
  );
}

export default SideTaskDrawer;
