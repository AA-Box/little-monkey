import { useMemo } from "react";
import { useShallow } from "zustand/react/shallow";
import {
  Archive,
  ArchiveRestore,
  CheckCircle2,
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
import { usePermissionStore } from "../../store/permissionStore";
import {
  cancelSideTask,
  openSideTaskAsFullChat,
  pauseSideTask,
  promoteSideTask,
  resumeSideTask,
  retrySideTask,
} from "../../lib/sideTaskRunner";
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

function SideTaskListRow({ task, selected, onSelect }: { task: SideTaskRecord; selected: boolean; onSelect: () => void }) {
  return (
    <button
      type="button"
      onClick={onSelect}
      className={`flex w-full cursor-pointer flex-col items-start gap-1 rounded-md border px-2.5 py-2 text-left transition-colors duration-150 ${
        selected ? "border-accent bg-surface-2" : "border-transparent hover:bg-surface-2"
      }`}
    >
      <div className="flex w-full items-center gap-1.5">
        <span className="min-w-0 flex-1 truncate text-sm font-medium text-foreground">{task.title}</span>
        <StatusPill tone={statusTone(task.status)}>{statusLabel(task.status)}</StatusPill>
      </div>
      <div className="flex w-full items-center gap-1.5 text-[11px] text-faint">
        <span className="truncate">{sourceKindLabel(task.source.kind)}</span>
        <span>·</span>
        <span className="truncate">{formatRelativeTime(task.updatedAt)}</span>
      </div>
    </button>
  );
}

function SideTaskDetail({ task }: { task: SideTaskRecord }) {
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
export function SideTaskDrawer({ sessionId }: SideTaskDrawerProps) {
  const open = useSideTaskStore((state) => state.drawerOpen);
  const toggleDrawer = useSideTaskStore((state) => state.toggleDrawer);
  const composerOpen = useSideTaskStore((state) => state.composerOpen);
  const openComposer = useSideTaskStore((state) => state.openComposer);
  const visible = useSideTaskStore(useShallow(selectVisibleSideTasks));
  const archived = useSideTaskStore(useShallow(selectArchivedSideTasks));
  const runningCount = useSideTaskStore(selectRunningSideTaskCount);
  const selectedTaskId = useSideTaskStore((state) => state.selectedTaskId);
  const selectedTask = useSideTaskStore((state) => (state.selectedTaskId ? state.tasks[state.selectedTaskId] : null));

  const showArchived = useMemo(() => archived.length > 0, [archived]);

  return (
    <aside
      className={`flex shrink-0 flex-col border-l border-border bg-surface transition-[width] duration-200 ${
        open ? "w-96" : "w-12"
      }`}
    >
      <div className="flex h-11 shrink-0 items-center justify-between border-b border-border px-3">
        {open && (
          <span className="flex items-center gap-1.5 text-[11px] font-semibold uppercase tracking-wider text-faint">
            Side Tasks
            {runningCount > 0 && <StatusPill tone="warning">{runningCount}</StatusPill>}
          </span>
        )}
        <div className={`flex items-center gap-1 ${open ? "" : "mx-auto"}`}>
          {open && (
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
          <IconButton
            size="sm"
            onClick={toggleDrawer}
            aria-label={open ? "Collapse side tasks panel" : "Expand side tasks panel"}
          >
            {open ? <PanelRightClose size={16} /> : <PanelRight size={16} />}
          </IconButton>
        </div>
      </div>

      {!open && runningCount > 0 && (
        <div className="flex justify-center py-2">
          <Loader2 size={16} className="animate-spin text-warning" />
        </div>
      )}

      {open && (
        <div className="flex min-h-0 flex-1 flex-col">
          {composerOpen && <SideTaskComposer />}

          <div className="flex min-h-0 flex-1">
            <div className="flex w-40 shrink-0 flex-col gap-1 overflow-y-auto border-r border-border p-2">
              {visible.length === 0 && !composerOpen && (
                <p className="p-2 text-xs text-faint">
                  No side tasks yet. Start one from a chat message, or the + button above.
                </p>
              )}
              {visible.map((task) => (
                <SideTaskListRow
                  key={task.id}
                  task={task}
                  selected={task.id === selectedTaskId}
                  onSelect={() => useSideTaskStore.getState().selectTask(task.id)}
                />
              ))}
              {showArchived && (
                <>
                  <div className="mt-2 px-1 text-[10px] font-semibold uppercase tracking-wider text-faint">Archived</div>
                  {archived.map((task) => (
                    <SideTaskListRow
                      key={task.id}
                      task={task}
                      selected={task.id === selectedTaskId}
                      onSelect={() => useSideTaskStore.getState().selectTask(task.id)}
                    />
                  ))}
                </>
              )}
            </div>

            {selectedTask ? (
              <SideTaskDetail task={selectedTask} />
            ) : (
              <div className="flex flex-1 items-center justify-center p-4 text-center text-xs text-faint">
                Select a side task to see its details.
              </div>
            )}
          </div>
        </div>
      )}
    </aside>
  );
}

export default SideTaskDrawer;
