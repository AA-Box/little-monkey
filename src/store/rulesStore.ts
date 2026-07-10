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
 * Mirrors the Rust `Fact` struct (src-tauri/src/memory.rs) exactly — field
 * names/casing must match the serde JSON representation returned by
 * `memory_list`.
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
  /** Durable per-project facts saved via the `remember` tool, for the
   * current primary workspace root — refreshed alongside `rules`. */
  facts: MemoryFact[];
  /** Re-fetch `rules` and `facts` from the backend. Cheap (local file reads),
   * safe to call once per turn so external edits to MONKEY.md (and newly
   * remembered facts) are picked up without a file watcher. */
  refresh: () => Promise<void>;
}

export const useRulesStore = create<RulesStore>((set) => ({
  rules: [],
  facts: [],

  refresh: async () => {
    // A broken MONKEY.md/memories.json (or, transiently, no workspace open
    // yet) must never block a turn from proceeding — same "never a hard
    // error" philosophy as `rules.rs`/`memory.rs`'s own reads. Falling back to
    // `[]` just means this turn's prompt carries no rules/facts, not that the
    // turn fails. The two calls fail independently so a broken one doesn't
    // wipe out the other.
    const [rules, facts] = await Promise.all([
      invoke<RuleFile[]>("rules_read").catch(() => [] as RuleFile[]),
      invoke<MemoryFact[]>("memory_list").catch(() => [] as MemoryFact[]),
    ]);
    set({ rules, facts });
  },
}));
