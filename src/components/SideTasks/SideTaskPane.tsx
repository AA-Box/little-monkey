import { useShallow } from "zustand/react/shallow";
import { Loader2, Plus, X } from "lucide-react";

import { IconButton, StatusPill } from "../ui";
import {
  selectRunningSideTaskCount,
  selectVisibleSideTasks,
  useSideTaskStore,
  type SideTaskRecord,
} from "../../store/sideTaskStore";
import { SideTaskComposer } from "./SideTaskComposer";
import { SideTaskConversation } from "./SideTaskConversation";
import { statusLabel, statusTone } from "./SideTaskDetail";

/**
 * The side-task pane: its own column beside the main chat, shaped like a
 * chat window — a tab strip across the top (one tab per open side task, plus
 * "+" for a new one), the active task's conversation in the middle, and one
 * composer pinned at the bottom that is always there, whether or not a task
 * is open. Typing into it with no tab open starts a task; typing with one
 * open continues that task.
 *
 * This is the surface that makes "side task" mean something distinct in this
 * app. A side task is a second CONVERSATION the user opened on purpose: it
 * has a transcript, a composer, and a lifecycle they drive. Headless work
 * the app does on its own — background shell commands, `task` subagent runs —
 * belongs to the Background Tasks panel in the right sidebar
 * (`BackgroundTasks/BackgroundTasksPanel.tsx`) instead, where a card and a
 * stop button are the whole interaction. The two used to share one drawer;
 * they no longer do, and neither should grow the other's affordances back.
 *
 * Closing a tab is a VIEW action: the run keeps going and can be reopened
 * from the empty state's recent list. Stopping a task is `SideTaskDetail`'s
 * Cancel (or the conversation header's stop square).
 */

function TabChip({
  task,
  active,
  onSelect,
  onClose,
}: {
  task: SideTaskRecord;
  active: boolean;
  onSelect: () => void;
  onClose: () => void;
}) {
  const running = task.status === "running" || task.status === "queued";
  return (
    <div
      className={`group inline-flex max-w-48 shrink-0 items-center rounded-lg text-sm transition-colors ${
        active ? "bg-surface-2 text-foreground" : "text-muted hover:bg-surface-2 hover:text-foreground"
      }`}
    >
      <button
        type="button"
        onClick={onSelect}
        className="inline-flex min-w-0 items-center gap-1.5 py-1.5 pl-2.5 focus-visible:outline-none focus-visible:ring-1 focus-visible:ring-accent"
      >
        {running && <Loader2 size={12} className="shrink-0 animate-spin text-warning" />}
        <span className="truncate">{task.title}</span>
      </button>
      <button
        type="button"
        aria-label={`Close "${task.title}" tab`}
        onClick={onClose}
        className="ml-0.5 mr-1 rounded-sm p-0.5 text-faint opacity-0 hover:text-danger focus-visible:opacity-100 focus-visible:outline-none focus-visible:ring-1 focus-visible:ring-accent group-hover:opacity-100"
      >
        <X size={11} />
      </button>
    </div>
  );
}

export interface SideTaskPaneProps {
  /** The chat session a manually-started side task is attributed to — the
   * seed `openComposer` gets when the pane's own "+" is used instead of a
   * message action. */
  sessionId: string;
  /** Closes the whole pane (the run keeps going). */
  onClose: () => void;
  /** Space (px) the tab strip leaves free at its right end for the app's
   * fixed dock-toggle cluster. Non-zero only while this pane is the
   * rightmost column — otherwise the icons float over another panel, not
   * this strip. Applied as a margin on the scroll viewport so chips can
   * never slide underneath the transparent icons. */
  trailingInset?: number;
}

export function SideTaskPane({ sessionId, onClose, trailingInset = 0 }: SideTaskPaneProps) {
  const openTabs = useSideTaskStore(useShallow((state) => state.openTabs));
  const activeTabId = useSideTaskStore((state) => state.activeTabId);
  const tasks = useSideTaskStore((state) => state.tasks);
  const runningCount = useSideTaskStore(selectRunningSideTaskCount);
  // A DRAFT tab: "+" (or any source's "start side task" action) opens a new,
  // empty tab straight away rather than silently re-aiming the composer at a
  // task that does not exist yet. It has no record in the store until the
  // first message is sent — `startSideTask` is what turns it into a real tab
  // — so it lives here as one extra chip driven by `composerOpen`.
  const draftOpen = useSideTaskStore((state) => state.composerOpen);
  // Everything not currently in a tab — the empty state's "reopen this"
  // list, so closing a tab is never a way to lose a task.
  const reopenable = useSideTaskStore(useShallow(selectVisibleSideTasks)).filter((task) => !openTabs.includes(task.id));

  const activeTask = activeTabId ? tasks[activeTabId] ?? null : null;
  // While the draft is up it IS the shown tab: its body is empty and its
  // composer starts a new task, so an open conversation must step aside
  // rather than sit under a composer that would not talk to it.
  const showingDraft = draftOpen;

  const startNew = () =>
    useSideTaskStore.getState().openComposer({
      title: "",
      prompt: "",
      profile: "explore",
      source: { kind: "manual", label: "Manual", excerpt: "" },
      sessionId,
    });

  const selectTab = (id: string) => {
    // Leaving the draft for an existing conversation drops the draft, the
    // same way closing its chip does — one composer, one target.
    useSideTaskStore.getState().closeComposer();
    useSideTaskStore.getState().setActiveTab(id);
  };

  return (
    <div className="flex min-h-0 w-full min-w-0 flex-1 flex-col border-l border-border bg-surface">
      {/* The border-b lives on the outer wrapper so it always spans the full
          pane width; the inner scroll viewport is what stops short of the
          fixed dock-toggle cluster (`trailingInset`), keeping every chip —
          and the pane's own close X — reachable and never underneath the
          floating icons. */}
      <div className="shrink-0 border-b border-border">
        <div
          data-tauri-drag-region
          className="flex h-11 items-center gap-1 overflow-x-auto px-3 [scrollbar-width:thin]"
          style={trailingInset > 0 ? { marginRight: trailingInset } : undefined}
        >
          {openTabs.length === 0 && !draftOpen && (
            <span className="flex items-center gap-1.5 text-[11px] font-semibold uppercase tracking-wider text-faint">
              Side tasks
              {runningCount > 0 && <StatusPill tone="warning">{runningCount}</StatusPill>}
            </span>
          )}
          {openTabs.map((id) => {
            const task = tasks[id];
            if (!task) return null;
            return (
              <TabChip
                key={id}
                task={task}
                active={!showingDraft && id === activeTabId}
                onSelect={() => selectTab(id)}
                onClose={() => useSideTaskStore.getState().closeTab(id)}
              />
            );
          })}
          {draftOpen && (
            <div className="group inline-flex max-w-48 shrink-0 items-center rounded-lg bg-surface-2 text-sm text-foreground">
              <span className="inline-flex min-w-0 items-center gap-1.5 py-1.5 pl-2.5">
                <span className="truncate italic text-muted">New side task</span>
              </span>
              <button
                type="button"
                aria-label="Discard the new side task"
                onClick={() => useSideTaskStore.getState().closeComposer()}
                className="ml-0.5 mr-1 rounded-sm p-0.5 text-faint opacity-0 hover:text-danger focus-visible:opacity-100 focus-visible:outline-none focus-visible:ring-1 focus-visible:ring-accent group-hover:opacity-100"
              >
                <X size={11} />
              </button>
            </div>
          )}
          <IconButton size="sm" variant="ghost" onClick={startNew} aria-label="Start a new side task">
            <Plus size={15} />
          </IconButton>
          <IconButton size="sm" variant="ghost" className="ml-auto" onClick={onClose} aria-label="Close side tasks pane">
            <X size={15} />
          </IconButton>
        </div>
      </div>

      {showingDraft ? (
        // The draft tab's body: deliberately empty, like a new chat.
        <div className="min-h-0 flex-1" />
      ) : activeTask ? (
        // Keyed so switching tabs resets the details/scroll state instead of
        // carrying one task's expanded evidence into another's.
        <SideTaskConversation key={activeTask.id} task={activeTask} />
      ) : (
        <div className="flex min-h-0 flex-1 flex-col justify-end p-3">
          {reopenable.length > 0 && (
            <div className="w-full">
              <div className="mb-1 text-[11px] font-medium uppercase tracking-wider text-faint">Recent</div>
              <div className="flex flex-col gap-1">
                {reopenable.slice(0, 8).map((task) => (
                  <button
                    key={task.id}
                    type="button"
                    onClick={() => useSideTaskStore.getState().openTab(task.id)}
                    className="flex items-center gap-2 rounded-lg px-2.5 py-1.5 text-left text-sm text-foreground hover:bg-surface-2"
                  >
                    <span className="min-w-0 flex-1 truncate">{task.title}</span>
                    <StatusPill tone={statusTone(task.status)}>{statusLabel(task.status)}</StatusPill>
                  </button>
                ))}
              </div>
            </div>
          )}
        </div>
      )}

      {/* One composer for the whole pane, always on screen: it starts a task
          while the draft tab is up (or when no tab is open at all) and
          continues the active one otherwise — see `SideTaskComposer`. */}
      <SideTaskComposer sessionId={sessionId} task={showingDraft ? null : activeTask} />
    </div>
  );
}

export default SideTaskPane;
