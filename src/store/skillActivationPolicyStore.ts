import { create } from "zustand";
import { isTauri } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { getCurrentWindow } from "@tauri-apps/api/window";
import {
  skillActivationClient,
  type SkillActivationEntry,
  type SkillActivationPolicy,
} from "../lib/skillActivationClient";

export type { SkillActivationPolicy } from "../lib/skillActivationClient";

const STORAGE_KEY = "little-monkey.skill-activation-policies";
const SKILL_ACTIVATION_CHANGED_EVENT = "skill-activation://changed";

let subscribed = false;

function legacyEntries(): SkillActivationEntry[] {
  if (typeof localStorage === "undefined") return [];
  try {
    const parsed: unknown = JSON.parse(localStorage.getItem(STORAGE_KEY) ?? "{}");
    if (!parsed || typeof parsed !== "object") return [];
    return Object.entries(parsed)
      .filter(([, policy]) => policy === "automatic" || policy === "ask" || policy === "manual")
      .map(([key, policy]) => ({ key, policy: policy as SkillActivationPolicy, pinned: false, updated_at_unix_ms: 0 }));
  } catch {
    return [];
  }
}

function mapEntries(entries: SkillActivationEntry[]): Record<string, SkillActivationEntry> {
  return Object.fromEntries(entries.map((entry) => [entry.key, entry]));
}

export interface SkillActivationPolicyStore {
  policies: Record<string, SkillActivationEntry>;
  hydrated: boolean;
  hydrating: boolean;
  error: string | null;
  hydrate: () => Promise<void>;
  refresh: () => Promise<void>;
  getPolicy: (key: string) => SkillActivationPolicy;
  isPinned: (key: string) => boolean;
  setPolicy: (key: string, policy: SkillActivationPolicy) => Promise<void>;
  setPinned: (key: string, pinned: boolean) => Promise<void>;
}

export const useSkillActivationPolicyStore = create<SkillActivationPolicyStore>((set, get) => ({
  policies: {},
  // Ask is the safe pre-hydration policy. The agent loop also withholds the
  // implicit skill tools until hydrated, so Automatic can never race startup.
  hydrated: false,
  hydrating: false,
  error: null,

  refresh: async () => {
    if (!isTauri()) return;
    try {
      const entries = await skillActivationClient.list();
      set({ policies: mapEntries(entries), error: null });
    } catch (error) {
      // Unknown backend state must not leave an old Automatic cache active.
      // The next turn retries against the profile-owned store.
      set({ hydrated: false, error: String(error) });
    }
  },

  hydrate: async () => {
    if (get().hydrating || get().hydrated) return;
    set({ hydrating: true, error: null });
    try {
      if (isTauri() && !subscribed) {
        const ownLabel = getCurrentWindow().label;
        await listen<string>(SKILL_ACTIVATION_CHANGED_EVENT, (event) => {
          if (event.payload === ownLabel) return;
          void get().refresh();
        });
        subscribed = true;
      }
      let entries = await skillActivationClient.list();
      const migrated = await skillActivationClient.migrate(legacyEntries());
      entries = migrated.length === entries.length && migrated.every((entry) =>
        entries.some((current) => current.key === entry.key && current.updated_at_unix_ms === entry.updated_at_unix_ms),
      ) ? entries : migrated;
      if (typeof localStorage !== "undefined") localStorage.removeItem(STORAGE_KEY);
      set({ policies: mapEntries(entries), hydrated: true, hydrating: false });
    } catch (error) {
      // Keep hydrated=false and therefore fail closed. A transient backend
      // failure must not turn an unknown skill into Automatic.
      set({ hydrating: false, error: String(error) });
    }
  },

  getPolicy: (key) => {
    if (!get().hydrated) return "ask";
    // Missing keys are new skills, which retain the existing Automatic
    // default once the backend snapshot is known. During hydration the
    // fail-closed Ask result above is never offered to the model because the
    // agent loop withholds implicit skill tools entirely.
    return get().policies[key]?.policy ?? "automatic";
  },

  isPinned: (key) => get().hydrated && get().policies[key]?.pinned === true,

  setPolicy: async (key, policy) => {
    const current = get().policies[key];
    const saved = await skillActivationClient.set(key, policy, current?.pinned ?? false);
    set((state) => ({ policies: { ...state.policies, [key]: saved } }));
  },

  setPinned: async (key, pinned) => {
    const current = get().policies[key];
    const saved = await skillActivationClient.set(key, current?.policy ?? get().getPolicy(key), pinned);
    set((state) => ({ policies: { ...state.policies, [key]: saved } }));
  },
}));

/** Stable keys keep a skill's policy across content updates. */
export function skillActivationPolicyKey(
  source: "local" | "native" | "package",
  command: string,
  id?: string,
): string {
  if (source === "local") return `local:${id ?? command}`;
  if (source === "package") return `package:${id ?? "any"}:${command}`;
  return `native:${id ?? "any"}:${command}`;
}

export function skillActivationPolicyFor(key: string): SkillActivationPolicy {
  return useSkillActivationPolicyStore.getState().getPolicy(key);
}

export function skillActivationIsPinned(key: string): boolean {
  return useSkillActivationPolicyStore.getState().isPinned(key);
}
