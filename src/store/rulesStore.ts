import { create } from "zustand";
import { invoke } from "@tauri-apps/api/core";

/**
 * Mirrors the Rust `RuleFile` struct (src-tauri/src/rules.rs) exactly —
 * field names/casing must match the serde JSON representation returned by
 * `rules_read`.
 */
export interface RuleFile {
  scope: "global" | "project";
  label: string;
  path: string;
  content: string;
  truncated: boolean;
}

/**
 * Mirrors the Rust `Fact` struct memory.rs will define (slice 3). Kept here
 * now, empty, purely so `buildSystemPrompt`'s signature and this store's
 * shape don't have to change again once slice 3 lands.
 */
export interface MemoryFact {
  id: string;
  text: string;
  source: "agent" | "user";
  created_at: string;
}

export interface RulesStore {
  /** Every MONKEY.md file currently in effect (global + attached roots),
   * refreshed once per agent turn — see `agentLoop.ts`'s `runAgentTurnBody`. */
  rules: RuleFile[];
  /** Durable per-project facts saved via the `remember` tool. Always empty
   * until slice 3 adds `memory.rs`/`memory_list`. */
  facts: MemoryFact[];
  /** Re-fetch `rules` from the backend. Cheap (local file reads), safe to
   * call once per turn so external edits to MONKEY.md are picked up without
   * a file watcher. */
  refresh: () => Promise<void>;
}

export const useRulesStore = create<RulesStore>((set) => ({
  rules: [],
  facts: [],

  refresh: async () => {
    // A broken MONKEY.md (or, transiently, no workspace open yet) must never
    // block a turn from proceeding — same "never a hard error" philosophy as
    // `rules.rs`'s own file reads. Falling back to `[]` just means this
    // turn's prompt carries no rules, not that the turn fails.
    try {
      const rules = await invoke<RuleFile[]>("rules_read");
      set({ rules });
    } catch {
      set({ rules: [] });
    }
  },
}));
