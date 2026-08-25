import { create } from "zustand";
import { invoke } from "@tauri-apps/api/core";
import { useStandardsStore } from "./standardsStore";

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
 * `memory_list`. `enabled`/`source_turn_id` are optional here (rather than
 * required) purely so existing hand-written test fixtures that predate
 * those fields keep compiling — real backend responses always include both.
 */
export interface MemoryFact {
  id: string;
  text: string;
  source: "agent" | "user";
  created_at: string;
  /** Soft-disable flag (Memory Studio). `memory_list` already filters to
   * `enabled: true` facts only, so in practice every fact reaching this
   * store has this `true` — Memory Studio's own full listing
   * (`memory_list_all` / `MemoryEntry`, see `memoryStudio.ts`) is where a
   * `false` value is actually seen and actionable. */
  enabled?: boolean;
  /** The chat turn this fact was remembered from, when known — `null`/absent
   * for facts added via the Settings "Add fact" affordance. */
  source_turn_id?: string | null;
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
   * remembered facts) are picked up without a file watcher. Standards Studio
   * piggybacks on this existing turn boundary so approved standards refresh
   * without adding a parallel orchestration lifecycle. */
  refresh: () => Promise<void>;
}

// Module-scoped (not store state) since it's purely an internal sequencing
// counter, never rendered or persisted — see `refresh`'s doc comment.
let latestRefreshRequest = 0;

export const useRulesStore = create<RulesStore>((set) => ({
  rules: [],
  facts: [],

  refresh: async () => {
    // Two `refresh()` calls can be in flight at once — e.g. a turn's own
    // post-`remember` refresh (agentLoop.ts) racing a Forget button's refresh
    // in another split pane (MessageList.tsx/RulesMemoryPanel.tsx), both
    // hitting the same unkeyed store for the same primary workspace root.
    // Backend IPC gives no ordering guarantee between concurrent invokes, so
    // an earlier-*started* call can resolve after a later one. Without a
    // sequence guard, whichever resolves last always wins the `set()` below,
    // even if it started first and is carrying a staler snapshot — silently
    // reintroducing a just-forgotten fact, for instance. Only the
    // most-recently-*started* call is allowed to commit its result; anything
    // that was superseded by a newer `refresh()` before it resolved is
    // dropped instead of overwriting fresher state.
    const requestId = ++latestRefreshRequest;

    // A broken MONKEY.md/memories.json/standards index (or, transiently, no
    // workspace open yet) must never block a turn from proceeding. The three
    // reads fail independently so a broken optional source cannot wipe out or
    // block the others.
    const [rules, facts] = await Promise.all([
      invoke<RuleFile[]>("rules_read").catch(() => [] as RuleFile[]),
      invoke<MemoryFact[]>("memory_list").catch(() => [] as MemoryFact[]),
      useStandardsStore.getState().refresh().catch(() => undefined),
    ]);

    if (requestId !== latestRefreshRequest) return;
    set({ rules, facts });
  },
}));
