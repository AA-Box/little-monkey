import { useEffect, useRef, useState } from "react";
import { BookOpen, Library } from "lucide-react";
import { useShallow } from "zustand/react/shallow";

import { useSessionStore } from "../../store/sessionStore";
import { useStackStore } from "../../store/stackStore";
import { useT } from "../../lib/i18n";

interface StackPickerProps {
  /** The session this pill controls — each `ChatWindow` pane owns one, so
   * the primary and split panes can each attach different stacks. */
  sessionId: string;
  /** Which way the dropdown opens relative to the trigger. "up" suits the
   * composer-footer placement (default); "down" the title-bar placement,
   * where an upward panel would clip past the window's top edge. */
  placement?: "up" | "down";
}

const EMPTY_STACK_IDS: string[] = [];

/**
 * Pill button + checklist for attaching knowledge stacks (see
 * `stacks.rs`/`stackStore.ts`) to this session — mirrors `PersonaSelector`'s
 * dropdown skeleton almost exactly, but with checkboxes instead of
 * single-select rows, since any number of stacks can be attached at once
 * (see `ChatSession.attachedStackIds`). Attaching at least one stack is what
 * makes `agentLoop.ts` offer the model the `search_docs` tool this turn (see
 * `tools.ts`'s `buildTools`) — this pill is the only way a user does that.
 */
export function StackPicker({ sessionId, placement = "up" }: StackPickerProps) {
  // `useShallow` avoids the same fresh-array-reference infinite-render trap
  // documented on `PersonaSelector`'s `selectPersonas` usage.
  const stacks = useStackStore(useShallow((state) => state.stacks));
  const refresh = useStackStore((state) => state.refresh);
  const attachedStackIds = useSessionStore(
    (state) => state.sessions.find((s) => s.id === sessionId)?.attachedStackIds ?? EMPTY_STACK_IDS
  );
  const toggleAttachedStack = useSessionStore((state) => state.toggleAttachedStack);
  const docChatMode = useSessionStore((state) => state.sessions.find((s) => s.id === sessionId)?.docChatMode ?? false);
  const toggleDocChatMode = useSessionStore((state) => state.toggleDocChatMode);

  const [open, setOpen] = useState(false);
  const containerRef = useRef<HTMLDivElement>(null);
  const { t } = useT();

  useEffect(() => {
    // Refreshed on mount (like `KnowledgePanel.tsx`) so a pill mounted before
    // the Knowledge settings tab was ever opened still lists current stacks,
    // and again whenever the dropdown opens so a stack created/renamed/
    // reindexed in Settings while this pill sat closed shows up fresh.
    void refresh();
  }, [refresh]);

  useEffect(() => {
    if (!open) return;
    void refresh();
    function handlePointerDown(event: PointerEvent) {
      if (containerRef.current && !containerRef.current.contains(event.target as Node)) {
        setOpen(false);
      }
    }
    window.addEventListener("pointerdown", handlePointerDown);
    return () => window.removeEventListener("pointerdown", handlePointerDown);
  }, [open, refresh]);

  const attachedCount = attachedStackIds.length;

  return (
    <div ref={containerRef} className="relative inline-block">
      <button
        type="button"
        onClick={() => setOpen((prev) => !prev)}
        aria-haspopup="true"
        aria-expanded={open}
        className="inline-flex items-center gap-1.5 rounded-full bg-surface-2 px-2.5 py-1 text-xs font-medium text-muted transition-colors duration-150 cursor-pointer hover:bg-surface hover:text-foreground focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-accent focus-visible:ring-offset-2 focus-visible:ring-offset-background"
      >
        {docChatMode ? <BookOpen size={13} className="shrink-0" /> : <Library size={13} className="shrink-0" />}
        <span className="max-w-[10rem] truncate">
          {attachedCount > 0 ? t("StackPicker.attachedCount", { count: attachedCount }) : t("StackPicker.noneLabel")}
        </span>
      </button>

      {open && (
        <div
          className={`absolute left-0 z-20 w-64 rounded-lg border border-border bg-background py-1 shadow-lg ${
            placement === "up" ? "bottom-full mb-1" : "top-full mt-1"
          }`}
        >
          {stacks.length === 0 ? (
            <p className="px-3 py-2 text-xs text-faint">{t("StackPicker.emptyState")}</p>
          ) : (
            stacks.map((stack) => {
              const checked = attachedStackIds.includes(stack.id);
              return (
                <label
                  key={stack.id}
                  className="flex w-full cursor-pointer items-start gap-2 px-3 py-2 text-left hover:bg-surface-2"
                >
                  <input
                    type="checkbox"
                    checked={checked}
                    onChange={() => toggleAttachedStack(sessionId, stack.id)}
                    className="mt-0.5 shrink-0 cursor-pointer accent-accent"
                  />
                  <span className="min-w-0 flex-1">
                    <span className="block truncate text-sm font-medium text-foreground">{stack.name}</span>
                    <span className="block truncate text-xs text-muted">
                      {stack.indexed_at !== null
                        ? t("StackPicker.chunkCount", { count: stack.chunk_count })
                        : t("StackPicker.notIndexed")}
                    </span>
                  </span>
                </label>
              );
            })
          )}
          <label className="flex w-full cursor-pointer items-start gap-2 border-t border-border px-3 py-2 text-left hover:bg-surface-2">
            <input
              type="checkbox"
              checked={docChatMode}
              onChange={() => toggleDocChatMode(sessionId)}
              className="mt-0.5 shrink-0 cursor-pointer accent-accent"
            />
            <span className="min-w-0 flex-1">
              <span className="block truncate text-sm font-medium text-foreground">{t("StackPicker.docChatToggleLabel")}</span>
              <span className="block text-xs text-muted">{t("StackPicker.docChatToggleDescription")}</span>
            </span>
          </label>
        </div>
      )}
    </div>
  );
}

export default StackPicker;
