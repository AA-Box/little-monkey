import { useEffect, useState } from "react";
import { X } from "lucide-react";

import { Button } from "../ui";
import { useSideTaskStore, type SideTaskProfile, type SideTaskSource } from "../../store/sideTaskStore";
import { startSideTask } from "../../lib/sideTaskRunner";

const MANUAL_SOURCE: SideTaskSource = { kind: "manual", label: "Manual", excerpt: "" };

/**
 * "Start a side task" form (ROADMAP.md's "Side Tasks" acceptance: start from
 * selected chat context, selected files, terminal output, browser evidence,
 * or an MCP result). Every real entry point in the app (right now:
 * `MessageBubble.tsx`'s per-message action) calls
 * `useSideTaskStore.getState().openComposer(seed)` with a prefilled
 * title/prompt/profile/source — this form just lets the user review/edit
 * that seed (or, opened via the drawer's own "+ New" button with no seed,
 * write one from scratch) before committing. Nothing here runs a model call
 * itself; `startSideTask` (fire-and-forget, see that function's doc
 * comment) is the only thing that does.
 */
export function SideTaskComposer() {
  const seed = useSideTaskStore((state) => state.composerSeed);
  const closeComposer = useSideTaskStore((state) => state.closeComposer);
  const consumeComposerSeed = useSideTaskStore((state) => state.consumeComposerSeed);
  const selectTask = useSideTaskStore((state) => state.selectTask);

  const [title, setTitle] = useState(seed?.title ?? "");
  const [prompt, setPrompt] = useState(seed?.prompt ?? "");
  const [profile, setProfile] = useState<SideTaskProfile>(seed?.profile ?? "explore");
  // Copied out of `composerSeed` into LOCAL state, same as title/prompt/
  // profile above — `consumeComposerSeed()` (below) nulls the store's own
  // `composerSeed` right after this effect runs, so anything the form still
  // needs afterward (which chat session this task belongs to, the source
  // preview) has to live here, not be read live off the store field.
  const [sessionId, setSessionId] = useState<string | null>(seed?.sessionId ?? null);
  const [source, setSource] = useState<SideTaskSource>(seed?.source ?? MANUAL_SOURCE);

  // Re-seed the form whenever a NEW seed arrives (e.g. the user starts a
  // second side task from another message while this form is still open) —
  // consumed once so re-opening the composer without a fresh seed (the "+
  // New" button) doesn't keep replaying a stale one.
  useEffect(() => {
    if (!seed) return;
    setTitle(seed.title);
    setPrompt(seed.prompt);
    setProfile(seed.profile);
    setSessionId(seed.sessionId);
    setSource(seed.source);
    consumeComposerSeed();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [seed]);

  const canStart = title.trim().length > 0 && prompt.trim().length > 0 && sessionId !== null;

  const handleStart = () => {
    if (!canStart || sessionId === null) return;
    const taskId = startSideTask({
      title: title.trim(),
      prompt: prompt.trim(),
      profile,
      source,
      sessionId,
    });
    selectTask(taskId);
    closeComposer();
    setTitle("");
    setPrompt("");
    setProfile("explore");
    setSessionId(null);
    setSource(MANUAL_SOURCE);
  };

  return (
    <div className="flex flex-col gap-3 border-b border-border bg-surface-2 p-3">
      <div className="flex items-center justify-between">
        <span className="text-xs font-semibold uppercase tracking-wider text-faint">New side task</span>
        <button
          type="button"
          onClick={closeComposer}
          aria-label="Close new side task form"
          className="flex h-6 w-6 cursor-pointer items-center justify-center rounded-md text-faint hover:bg-surface hover:text-foreground"
        >
          <X size={14} />
        </button>
      </div>

      {source.kind !== "manual" && (
        <div className="rounded-md border border-border bg-surface px-2.5 py-1.5 text-xs text-muted">
          <div className="font-medium text-foreground">{source.label}</div>
          {source.excerpt && <div className="mt-0.5 line-clamp-3 whitespace-pre-wrap text-faint">{source.excerpt}</div>}
        </div>
      )}

      <label className="flex flex-col gap-1">
        <span className="text-xs font-medium text-muted">Title</span>
        <input
          type="text"
          value={title}
          onChange={(event) => setTitle(event.target.value)}
          placeholder="e.g. Explain the auth flow"
          className="rounded-md border border-border bg-background px-2.5 py-1.5 text-sm text-foreground outline-none focus:border-accent"
        />
      </label>

      <label className="flex flex-col gap-1">
        <span className="text-xs font-medium text-muted">Instructions</span>
        <textarea
          value={prompt}
          onChange={(event) => setPrompt(event.target.value)}
          rows={5}
          placeholder="What should this side task do?"
          className="resize-none rounded-md border border-border bg-background px-2.5 py-1.5 text-sm text-foreground outline-none focus:border-accent"
        />
      </label>

      <div className="flex items-center gap-3">
        <span className="text-xs font-medium text-muted">Tools</span>
        <label className="flex cursor-pointer items-center gap-1.5 text-xs text-foreground">
          <input type="radio" name="side-task-profile" checked={profile === "explore"} onChange={() => setProfile("explore")} />
          Read-only
        </label>
        <label className="flex cursor-pointer items-center gap-1.5 text-xs text-foreground">
          <input type="radio" name="side-task-profile" checked={profile === "code"} onChange={() => setProfile("code")} />
          Can edit files
        </label>
      </div>

      <div className="flex justify-end gap-2">
        <Button variant="ghost" size="sm" onClick={closeComposer}>
          Cancel
        </Button>
        <Button variant="primary" size="sm" onClick={handleStart} disabled={!canStart}>
          Start side task
        </Button>
      </div>
    </div>
  );
}

export default SideTaskComposer;
