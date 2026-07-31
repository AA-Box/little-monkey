import { useShallow } from "zustand/react/shallow";
import { Sparkles, X } from "lucide-react";

import {
  selectPendingSuggestions,
  useTaskSuggestionStore,
  type TaskSuggestion,
} from "../../store/taskSuggestionStore";
import { useSessionStore } from "../../store/sessionStore";
import { runAgentTurn } from "../../lib/agentLoop";

/**
 * The spawned-task chips: follow-up work the model flagged mid-turn with the
 * `spawn_task` tool, offered above the composer. Clicking one spins it off
 * into its OWN chat session — a fresh session named after the chip, seeded
 * with the suggestion's self-contained prompt — and leaves this conversation
 * exactly where it was.
 *
 * Until that click nothing runs: `spawn_task` only stages a row in
 * `taskSuggestionStore.ts`. That is what keeps a model-proposed follow-up
 * from quietly becoming a model-started one.
 */
export default function TaskSuggestionChips({ sessionId }: { sessionId: string }) {
  const suggestions = useTaskSuggestionStore(useShallow(selectPendingSuggestions(sessionId)));
  if (suggestions.length === 0) return null;

  const spawn = (suggestion: TaskSuggestion) => {
    const sessions = useSessionStore.getState();
    sessions.newSession();
    const spawnedId = useSessionStore.getState().activeSessionId;
    useSessionStore.getState().renameSession(spawnedId, suggestion.title);
    useTaskSuggestionStore.getState().markStarted(suggestion.id, spawnedId);
    void runAgentTurn(spawnedId, suggestion.prompt).catch(() => {
      // The new session shows the failure in its own transcript; the chip has
      // already done its job by getting the user there.
    });
  };

  return (
    <div className="mx-4 mb-2">
      <div className="mx-auto flex max-w-3xl flex-wrap gap-1.5">
        {suggestions.map((suggestion) => (
          <div
            key={suggestion.id}
            className="group inline-flex max-w-full items-center rounded-full border border-border bg-surface-2 text-xs text-muted transition-colors duration-150 hover:border-accent hover:text-foreground"
          >
            <button
              type="button"
              onClick={() => spawn(suggestion)}
              title={suggestion.tldr || suggestion.title}
              className="inline-flex min-w-0 cursor-pointer items-center gap-1.5 py-1 pl-2.5"
            >
              <Sparkles size={12} className="shrink-0 text-accent" />
              <span className="truncate">{suggestion.title}</span>
            </button>
            <button
              type="button"
              aria-label={`Dismiss "${suggestion.title}"`}
              onClick={() => useTaskSuggestionStore.getState().dismiss(suggestion.id)}
              className="ml-1 mr-1.5 cursor-pointer rounded-full p-0.5 text-faint hover:text-danger"
            >
              <X size={11} />
            </button>
          </div>
        ))}
      </div>
    </div>
  );
}
