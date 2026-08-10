import { create } from "zustand";
import type { WorkflowSpec } from "../lib/workflow";

/** localStorage key the saved-workflows blob is persisted under — same
 * hand-rolled hydrate/persist mechanism as `settingsStore.ts`'s
 * `STORAGE_KEY` (NOT a new file path: localStorage is already scoped per
 * app data root, so profiles stay isolated without this store knowing
 * anything about profile resolution). Exported so tests can clear it. */
export const SAVED_WORKFLOWS_STORAGE_KEY = "little-monkey-saved-workflows";

/** One saved, named workflow spec — re-runnable by name via the `workflow`
 * tool's `saved` argument (see `resolveWorkflowSpec` in lib/workflow.ts). */
export interface SavedWorkflow {
  spec: WorkflowSpec;
  /** When this name was first saved (kept across upserts). */
  savedAt: number;
  /** When a run of this spec last completed successfully — `undefined` for
   * a spec saved from a card whose run never succeeded. */
  lastRunAt?: number;
}

interface SavedWorkflowStoreState {
  /** Saved specs keyed by `spec.name` — last-run-wins on collision. */
  workflows: Record<string, SavedWorkflow>;
  /** Saves/replaces the spec under `spec.name`. `ranAt` set = this upsert
   * came from a successful run (updates `lastRunAt`); absent = an explicit
   * user save, which must not fabricate a run timestamp. */
  upsert: (spec: WorkflowSpec, ranAt?: number) => void;
  remove: (name: string) => void;
}

/** Defensive validation for one persisted entry — hand-edited or corrupt
 * localStorage must drop the bad entry, never crash hydration or corrupt
 * the rest. Same posture as `settingsStore.ts`'s `sanitize*` helpers.
 * Structural only (shape, non-empty strings); the size caps in
 * `parseWorkflowSpec` were already enforced when the spec was created. */
function sanitizeEntry(value: unknown): SavedWorkflow | null {
  if (!value || typeof value !== "object") return null;
  const entry = value as { spec?: unknown; savedAt?: unknown; lastRunAt?: unknown };
  const spec = entry.spec as WorkflowSpec | undefined;
  if (!spec || typeof spec !== "object") return null;
  if (typeof spec.name !== "string" || spec.name.trim().length === 0) return null;
  if (typeof spec.description !== "string") return null;
  if (!Array.isArray(spec.phases) || spec.phases.length === 0) return null;
  for (const phase of spec.phases) {
    if (!phase || typeof phase.title !== "string") return null;
    if (!Array.isArray(phase.agents) || phase.agents.length === 0) return null;
    for (const agent of phase.agents) {
      if (!agent || typeof agent.description !== "string") return null;
      if (typeof agent.prompt !== "string" || agent.prompt.length === 0) return null;
      if (agent.profile !== "explore" && agent.profile !== "code") return null;
    }
  }
  return {
    spec,
    savedAt: typeof entry.savedAt === "number" ? entry.savedAt : Date.now(),
    lastRunAt: typeof entry.lastRunAt === "number" ? entry.lastRunAt : undefined,
  };
}

/** Loads the persisted blob, dropping anything absent, corrupt, or malformed. */
function hydrate(): Record<string, SavedWorkflow> {
  try {
    const raw = localStorage.getItem(SAVED_WORKFLOWS_STORAGE_KEY);
    if (!raw) return {};
    const parsed: unknown = JSON.parse(raw);
    if (!parsed || typeof parsed !== "object") return {};
    const workflows: Record<string, SavedWorkflow> = {};
    for (const [name, value] of Object.entries(parsed as Record<string, unknown>)) {
      const entry = sanitizeEntry(value);
      if (entry && entry.spec.name === name) workflows[name] = entry;
    }
    return workflows;
  } catch {
    return {};
  }
}

/** Best-effort persist — mirrors `settingsStore.ts`'s `persist`. */
function persist(workflows: Record<string, SavedWorkflow>): void {
  try {
    localStorage.setItem(SAVED_WORKFLOWS_STORAGE_KEY, JSON.stringify(workflows));
  } catch {
    // Ignore — persistence is best-effort.
  }
}

export const useSavedWorkflowStore = create<SavedWorkflowStoreState>((set, get) => ({
  workflows: hydrate(),

  upsert: (spec, ranAt) => {
    const existing = get().workflows[spec.name];
    const next: Record<string, SavedWorkflow> = {
      ...get().workflows,
      [spec.name]: {
        spec,
        savedAt: existing?.savedAt ?? Date.now(),
        lastRunAt: ranAt ?? existing?.lastRunAt,
      },
    };
    set({ workflows: next });
    persist(next);
  },

  remove: (name) => {
    const next = { ...get().workflows };
    delete next[name];
    set({ workflows: next });
    persist(next);
  },
}));

/** Every saved workflow, alphabetical by name — fresh array per call, wrap
 * in `useShallow` at subscription sites, same as `selectWorkflowRunList`. */
export function selectSavedWorkflowList(state: SavedWorkflowStoreState): SavedWorkflow[] {
  return Object.values(state.workflows).sort((a, b) => a.spec.name.localeCompare(b.spec.name));
}
