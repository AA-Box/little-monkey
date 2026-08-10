import { create } from "zustand";
import { invoke } from "@tauri-apps/api/core";

/** The four lifecycle events hooks can attach to in this slice — see
 * `src/lib/userHooks.ts` for what firing each one means. */
export const USER_HOOK_EVENTS = ["PreToolUse", "PostToolUse", "SessionStart", "UserPromptSubmit"] as const;
export type UserHookEvent = (typeof USER_HOOK_EVENTS)[number];

/** One user-configured hook: a shell command wired to a lifecycle event,
 * optionally filtered to matching tool names (PreToolUse/PostToolUse only). */
export interface UserHookDef {
  id: string;
  event: UserHookEvent;
  command: string;
  /** Tool-name filter, regex or exact name — see `matcherMatches`. */
  matcher?: string;
}

interface UserHooksStoreState {
  hooks: UserHookDef[];
  /** True once `initialize` settled (either way) — the Settings editor shows
   * a loading state until then. */
  loaded: boolean;
  /** Loads `hooks.json` through the Rust side (`hooks_load`) — call once at
   * boot; safe to call again (idempotent refresh). */
  initialize: () => Promise<void>;
  add: (hook: Omit<UserHookDef, "id">) => void;
  remove: (id: string) => void;
}

function isUserHookEvent(value: unknown): value is UserHookEvent {
  return typeof value === "string" && (USER_HOOK_EVENTS as readonly string[]).includes(value);
}

/** Defensive per-entry validation for the persisted config — a hand-edited
 * `hooks.json` must drop bad entries, never crash hydration. Same posture as
 * `savedWorkflowStore.ts`'s `sanitizeEntry`. */
function sanitizeHook(value: unknown): UserHookDef | null {
  if (!value || typeof value !== "object") return null;
  const entry = value as { id?: unknown; event?: unknown; command?: unknown; matcher?: unknown };
  if (!isUserHookEvent(entry.event)) return null;
  if (typeof entry.command !== "string" || entry.command.trim().length === 0) return null;
  return {
    id: typeof entry.id === "string" && entry.id.length > 0 ? entry.id : crypto.randomUUID(),
    event: entry.event,
    command: entry.command,
    matcher: typeof entry.matcher === "string" && entry.matcher.trim().length > 0 ? entry.matcher : undefined,
  };
}

/** Parses raw `hooks.json` content into valid entries. Exported for the
 * DOM-free logic tests. */
export function parseHooksConfig(raw: string): UserHookDef[] {
  if (raw.trim().length === 0) return [];
  try {
    const parsed: unknown = JSON.parse(raw);
    if (!Array.isArray(parsed)) return [];
    return parsed.map(sanitizeHook).filter((hook): hook is UserHookDef => hook !== null);
  } catch {
    return [];
  }
}

/** Best-effort write-through to `hooks.json` — mirrors `settingsStore`'s
 * persist-on-every-mutation shape, via the Rust profile chokepoint. */
function persist(hooks: UserHookDef[]): void {
  void invoke("hooks_save", { content: JSON.stringify(hooks, null, 2) }).catch((err) => {
    console.warn("Could not save hooks config:", err);
  });
}

export const useUserHooksStore = create<UserHooksStoreState>((set, get) => ({
  hooks: [],
  loaded: false,

  initialize: async () => {
    try {
      const raw = await invoke<string>("hooks_load");
      set({ hooks: parseHooksConfig(raw), loaded: true });
    } catch (err) {
      console.warn("Could not load hooks config:", err);
      set({ loaded: true });
    }
  },

  add: (hook) => {
    const next = [...get().hooks, { ...hook, id: crypto.randomUUID() }];
    set({ hooks: next });
    persist(next);
  },

  remove: (id) => {
    const next = get().hooks.filter((hook) => hook.id !== id);
    set({ hooks: next });
    persist(next);
  },
}));
