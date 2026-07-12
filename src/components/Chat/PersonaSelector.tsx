import { useEffect, useRef, useState } from "react";
import { UserCog } from "lucide-react";
import { useShallow } from "zustand/react/shallow";

import { selectPersonas, usePromptStore } from "../../store/promptStore";
import { useSessionStore } from "../../store/sessionStore";
import { useT } from "../../lib/i18n";

interface PersonaSelectorProps {
  /** The session this pill controls — each `ChatWindow` pane owns one, so
   * the primary and split panes can each have their own active persona. */
  sessionId: string;
  /** Opens Settings on the Prompts tab (App.tsx wires this to the same
   * "deep-link to a tab" hook `SettingsModal` exposes). Invoked by the
   * "Manage prompts…" row. */
  onManagePrompts: () => void;
}

/**
 * Pill button + dropdown for picking the session's active persona (see
 * `ChatSession.personaId`/`composeSystemPrompt`), mirroring `EffortSelector`'s
 * dropdown skeleton and `ModeSelector`'s list-of-options body. Lists every
 * saved persona plus a "None" row to clear it, and a "Manage prompts…" row
 * that opens Settings on the Prompts tab instead of managing personas here.
 */
export function PersonaSelector({ sessionId, onManagePrompts }: PersonaSelectorProps) {
  // `useShallow` is load-bearing: `selectPersonas` filters `entries` into a
  // fresh array on every call, and an unwrapped fresh-reference snapshot
  // spins `useSyncExternalStore` into an infinite re-render loop that blanks
  // the whole app (same trap documented in ProviderCard's EMPTY_MODELS).
  const personas = usePromptStore(useShallow(selectPersonas));
  const personaId = useSessionStore(
    (state) => state.sessions.find((s) => s.id === sessionId)?.personaId ?? null
  );
  const setSessionPersona = useSessionStore((state) => state.setSessionPersona);

  const [open, setOpen] = useState(false);
  const containerRef = useRef<HTMLDivElement>(null);
  const { t } = useT();

  useEffect(() => {
    if (!open) return;
    function handlePointerDown(event: PointerEvent) {
      if (containerRef.current && !containerRef.current.contains(event.target as Node)) {
        setOpen(false);
      }
    }
    window.addEventListener("pointerdown", handlePointerDown);
    return () => window.removeEventListener("pointerdown", handlePointerDown);
  }, [open]);

  const active = personas.find((p) => p.id === personaId) ?? null;

  function handleSelect(id: string | null) {
    setSessionPersona(sessionId, id);
    setOpen(false);
  }

  function handleManage() {
    setOpen(false);
    onManagePrompts();
  }

  return (
    <div ref={containerRef} className="relative inline-block">
      <button
        type="button"
        onClick={() => setOpen((prev) => !prev)}
        aria-haspopup="true"
        aria-expanded={open}
        className="inline-flex items-center gap-1.5 rounded-full bg-surface-2 px-2.5 py-1 text-xs font-medium text-muted transition-colors duration-150 cursor-pointer hover:bg-surface hover:text-foreground focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-accent focus-visible:ring-offset-2 focus-visible:ring-offset-background"
      >
        <UserCog size={13} className="shrink-0" />
        <span className="max-w-[10rem] truncate">{active ? active.name : t("PersonaSelector.noneLabel")}</span>
      </button>

      {open && (
        <div className="absolute bottom-full left-0 z-20 mb-1 w-64 rounded-lg border border-border bg-background py-1 shadow-lg">
          <button
            type="button"
            onClick={() => handleSelect(null)}
            className={`flex w-full cursor-pointer items-center px-3 py-2 text-left text-sm ${
              personaId === null ? "bg-accent-soft text-accent" : "text-foreground hover:bg-surface-2"
            }`}
          >
            {t("PersonaSelector.noneLabel")}
          </button>

          {personas.length === 0 ? (
            <p className="px-3 py-2 text-xs text-faint">{t("PersonaSelector.emptyState")}</p>
          ) : (
            personas.map((persona) => {
              const isActive = persona.id === personaId;
              return (
                <button
                  key={persona.id}
                  type="button"
                  onClick={() => handleSelect(persona.id)}
                  className={`flex w-full min-w-0 cursor-pointer flex-col items-start px-3 py-2 text-left ${
                    isActive ? "bg-accent-soft" : "hover:bg-surface-2"
                  }`}
                >
                  <span className={`block w-full truncate text-sm font-medium ${isActive ? "text-accent" : "text-foreground"}`}>
                    {persona.name}
                  </span>
                  {persona.description && (
                    <span className="block w-full truncate text-xs text-muted">{persona.description}</span>
                  )}
                </button>
              );
            })
          )}

          <div className="mt-1 border-t border-border pt-1">
            <button
              type="button"
              onClick={handleManage}
              className="block w-full cursor-pointer px-3 py-2 text-left text-xs text-muted hover:bg-surface-2 hover:text-foreground"
            >
              {t("PersonaSelector.managePromptsLabel")}
            </button>
          </div>
        </div>
      )}
    </div>
  );
}

export default PersonaSelector;
