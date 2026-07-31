import { useCallback, useEffect, useRef, useState } from "react";
import type { FormEvent, KeyboardEvent } from "react";
import { open } from "@tauri-apps/plugin-dialog";
import { CornerDownLeft, Eye, Pencil, Square, X } from "lucide-react";

import { AttachMenu } from "../Chat/AttachMenu";
import { AttachmentChip } from "../Chat/AttachmentChip";
import { ModeSelector } from "../Chat/ModeSelector";
import { ModelSwitcher } from "../Chat/ModelSwitcher";
import {
  useSideTaskStore,
  type SideTaskProfile,
  type SideTaskRecord,
  type SideTaskSource,
} from "../../store/sideTaskStore";
import { cancelSideTask, continueSideTask, startSideTask } from "../../lib/sideTaskRunner";
import {
  appendAttachmentContext,
  attachmentSourceLabel,
  deriveSideTaskTitle,
  type SideTaskAttachment,
} from "./composerPrompt";

const MANUAL_SOURCE: SideTaskSource = { kind: "manual", label: "Manual", excerpt: "" };

// Same auto-grow cap as the main chat composer (`ChatWindow.tsx`) — both
// textareas share the `max-h-40` class, so the JS clamp must match 160px too.
const MAX_TEXTAREA_HEIGHT_PX = 160;

/**
 * The side-task pane's one composer, pinned to the bottom of the pane the
 * same way the main chat's composer is pinned to the bottom of the chat —
 * because a side task IS a conversation, and typing into a box is how you
 * start or continue one. It replaces the old "New side task" form (title
 * field + instructions field + tool radios above the transcript): the title
 * is derived from what was typed (`composerPrompt.ts`), attachments are
 * chips instead of a separate picker step, and permission mode / model /
 * tool profile sit in a control row under the input, mirroring
 * `ChatWindow.tsx`'s layout.
 *
 * Two modes, one box:
 * - NEW (no active task, or a seed was staged by `openComposer`) — Enter
 *   calls `startSideTask`, which opens the new task's own tab.
 * - FOLLOW-UP (a tab is active) — Enter calls `continueSideTask`, sending
 *   into that SAME run; while it's working the send button becomes Stop.
 *
 * Every existing entry point (a message action, the file tree, terminal
 * output, browser evidence, an MCP result) still calls
 * `useSideTaskStore.getState().openComposer(seed)`; the seed now prefills
 * this box instead of a form, and `composerOpen` means "the user is aiming
 * at a NEW task" rather than "a form is on screen".
 */

export interface SideTaskComposerProps {
  /** Session a newly-started task is attributed to (model resolution and the
   * default Promote destination). */
  sessionId: string;
  /** The active tab's task, or null when the pane has no open tab. */
  task: SideTaskRecord | null;
}

function ProfileToggle({ profile, onChange }: { profile: SideTaskProfile; onChange: (next: SideTaskProfile) => void }) {
  const canEdit = profile === "code";
  const Icon = canEdit ? Pencil : Eye;
  return (
    <button
      type="button"
      onClick={() => onChange(canEdit ? "explore" : "code")}
      aria-label={`Tool access: ${canEdit ? "can edit files" : "read-only"} — click to switch`}
      className={`inline-flex cursor-pointer items-center gap-1.5 rounded-full px-2.5 py-1 text-xs font-medium transition-colors duration-150 focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-accent focus-visible:ring-offset-2 focus-visible:ring-offset-background ${
        canEdit ? "bg-warning-soft text-warning" : "bg-surface-2 text-muted hover:bg-surface hover:text-foreground"
      }`}
    >
      <Icon size={13} className="shrink-0" />
      {canEdit ? "Can edit files" : "Read-only"}
    </button>
  );
}

export function SideTaskComposer({ sessionId, task }: SideTaskComposerProps) {
  const seed = useSideTaskStore((state) => state.composerSeed);
  const composerOpen = useSideTaskStore((state) => state.composerOpen);
  const closeComposer = useSideTaskStore((state) => state.closeComposer);
  const consumeComposerSeed = useSideTaskStore((state) => state.consumeComposerSeed);

  const [text, setText] = useState("");
  const [profile, setProfile] = useState<SideTaskProfile>("explore");
  const [attachments, setAttachments] = useState<SideTaskAttachment[]>([]);
  // Copied out of `composerSeed` into LOCAL state — `consumeComposerSeed()`
  // nulls the store field right after the prefill effect runs, so anything
  // the box still needs afterward (the source preview, the title the source
  // already wrote, which session it belongs to) has to live here.
  const [seedTitle, setSeedTitle] = useState<string | null>(null);
  const [seedSessionId, setSeedSessionId] = useState<string | null>(null);
  const [source, setSource] = useState<SideTaskSource>(MANUAL_SOURCE);
  const textareaRef = useRef<HTMLTextAreaElement>(null);

  // Mirrors ChatWindow's resizeTextarea exactly: grow with content, clamp at
  // the same max height, then let the textarea scroll internally.
  const resizeTextarea = useCallback(() => {
    const el = textareaRef.current;
    if (!el) return;
    el.style.height = "auto";
    el.style.height = `${Math.min(el.scrollHeight, MAX_TEXTAREA_HEIGHT_PX)}px`;
  }, []);

  // A staged seed always wins over the active tab: the user clicked "start a
  // side task" on some piece of context, so this box must be aiming at a NEW
  // task even while another task's conversation is on screen.
  const newTask = composerOpen || task === null;
  const busy = task !== null && (task.status === "running" || task.status === "queued");

  useEffect(() => {
    if (!seed) return;
    setText(seed.prompt);
    setProfile(seed.profile);
    setSeedTitle(seed.title.trim() || null);
    setSeedSessionId(seed.sessionId);
    setSource(seed.source);
    setAttachments([]);
    consumeComposerSeed();
    // The seeded prompt lands after this render, so measure on the next frame.
    requestAnimationFrame(resizeTextarea);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [seed]);

  // Focus whenever the user aims this box at a new task ("+", or any source's
  // "start side task" action), so a seeded prompt can be edited or sent
  // without a click.
  useEffect(() => {
    if (composerOpen) textareaRef.current?.focus();
  }, [composerOpen]);

  const resetNewTaskState = () => {
    setText("");
    setProfile("explore");
    setAttachments([]);
    setSeedTitle(null);
    setSeedSessionId(null);
    setSource(MANUAL_SOURCE);
    requestAnimationFrame(resizeTextarea);
  };

  const handleAddPaths = async (directory: boolean) => {
    try {
      const selected = await open({ multiple: true, directory });
      if (!selected) return; // cancelled
      const paths = Array.isArray(selected) ? selected : [selected];
      setAttachments((prev) => {
        const existing = new Set(prev.map((attachment) => attachment.path));
        const additions = paths.filter((path) => !existing.has(path)).map((path) => ({ path, isDir: directory }));
        return additions.length > 0 ? [...prev, ...additions] : prev;
      });
    } catch (error) {
      console.error("Failed to open picker for side task attachments", error);
    }
  };

  const send = (event?: FormEvent) => {
    event?.preventDefault();
    const typed = text.trim();

    if (!newTask) {
      if (!task || busy || !typed) return;
      if (continueSideTask(task.id, typed)) {
        setText("");
        requestAnimationFrame(resizeTextarea);
      }
      return;
    }

    const startSessionId = seedSessionId ?? sessionId;
    const prompt = appendAttachmentContext(typed, attachments);
    if (!prompt.trim()) return;
    startSideTask({
      title: seedTitle ?? deriveSideTaskTitle(typed),
      prompt,
      profile,
      // Attaching paths to an otherwise manual task makes it the same kind of
      // thing the file tree's own action starts, so it gets the same source
      // tag rather than a "Manual" one that hides where the context came from.
      source:
        source.kind === "manual" && attachments.length > 0
          ? {
              kind: "selected_files",
              label: attachmentSourceLabel(attachments),
              excerpt: attachments.map((attachment) => attachment.path).join("\n"),
            }
          : source,
      sessionId: startSessionId,
    });
    resetNewTaskState();
    // Back to follow-up mode: `create` already made the new task the active
    // tab, so the next thing typed continues it.
    closeComposer();
  };

  const handleKeyDown = (event: KeyboardEvent<HTMLTextAreaElement>) => {
    if (event.key === "Enter" && !event.shiftKey) {
      event.preventDefault();
      send();
    }
    if (event.key === "Escape" && composerOpen) {
      // Discards the draft tab — same as clicking its X — and falls back to
      // whichever conversation was open behind it.
      event.preventDefault();
      resetNewTaskState();
      closeComposer();
    }
  };

  const showStop = !newTask && busy;
  const canSend = newTask ? text.trim().length > 0 || attachments.length > 0 : !busy && text.trim().length > 0;
  const placeholder = newTask
    ? "Start a side task — what should it do?"
    : busy
      ? "Running…"
      : "Ask for follow-up changes";

  return (
    <form onSubmit={send} className="shrink-0 px-3 pb-3 pt-2">
      {newTask && source.kind !== "manual" && (
        <div className="mb-1.5 flex items-start gap-2 rounded-lg border border-border bg-surface-2 px-2.5 py-1.5 text-xs">
          <div className="min-w-0 flex-1">
            <div className="truncate font-medium text-foreground">{source.label}</div>
            {source.excerpt && <div className="mt-0.5 line-clamp-2 whitespace-pre-wrap text-faint">{source.excerpt}</div>}
          </div>
          <button
            type="button"
            onClick={() => setSource(MANUAL_SOURCE)}
            aria-label="Drop the captured context"
            className="shrink-0 cursor-pointer text-faint hover:text-danger"
          >
            <X size={11} />
          </button>
        </div>
      )}

      <div className="flex flex-col rounded-3xl border border-border bg-surface px-3 py-2 transition-colors focus-within:border-accent focus-within:ring-1 focus-within:ring-accent">
        {newTask && attachments.length > 0 && (
          <div className="mb-1.5 flex flex-wrap gap-1.5">
            {attachments.map((attachment) => {
              const segments = attachment.path.split(/[\\/]/).filter(Boolean);
              return (
                <AttachmentChip
                  key={attachment.path}
                  name={segments[segments.length - 1] ?? attachment.path}
                  isDir={attachment.isDir}
                  onRemove={() =>
                    setAttachments((prev) => prev.filter((entry) => entry.path !== attachment.path))
                  }
                />
              );
            })}
          </div>
        )}
        <div className="flex items-end gap-2">
          <textarea
            ref={textareaRef}
            value={text}
            onChange={(event) => {
              setText(event.target.value);
              resizeTextarea();
            }}
            onKeyDown={handleKeyDown}
            rows={1}
            disabled={showStop}
            placeholder={placeholder}
            data-focus-ring="custom"
            className="max-h-40 min-h-[1.75rem] flex-1 resize-none bg-transparent py-1 text-sm leading-relaxed text-foreground outline-none placeholder:text-faint disabled:cursor-not-allowed"
          />
          <button
            type={showStop ? "button" : "submit"}
            onClick={showStop && task ? () => cancelSideTask(task.id) : undefined}
            disabled={!showStop && !canSend}
            aria-label={showStop ? `Stop "${task?.title ?? "side task"}"` : newTask ? "Start side task" : "Send follow-up"}
            className="flex h-7 w-7 shrink-0 cursor-pointer items-center justify-center rounded-full text-faint transition-colors duration-150 hover:bg-surface-2 hover:text-foreground disabled:cursor-not-allowed disabled:opacity-40 disabled:hover:bg-transparent focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-accent focus-visible:ring-offset-2 focus-visible:ring-offset-background"
          >
            {showStop ? <Square size={13} className="fill-current" /> : <CornerDownLeft size={16} />}
          </button>
        </div>
      </div>

      <div className="mt-1.5 flex flex-wrap items-center justify-between gap-x-3 gap-y-1.5">
        <div className="flex flex-wrap items-center gap-1.5">
          <ModeSelector />
          {newTask && (
            <>
              <AttachMenu onAddFiles={() => void handleAddPaths(false)} onAddFolder={() => void handleAddPaths(true)} />
              <ProfileToggle profile={profile} onChange={setProfile} />
            </>
          )}
        </div>
        <div className="flex min-w-0 items-center gap-2">
          {newTask ? (
            // The app-wide model picker — a side task resolves its model at
            // start from the same store the main chat reads, so this is the
            // model the NEXT task starts with (and, like everywhere else in
            // the app, switching here switches the chat's model too).
            <ModelSwitcher />
          ) : (
            // A running task's model was frozen at start; showing the picker
            // here would imply it can still be changed.
            <span className="max-w-full truncate text-[11px] text-faint" title={task?.modelLabel}>
              {task?.modelLabel}
            </span>
          )}
        </div>
      </div>
    </form>
  );
}

export default SideTaskComposer;
