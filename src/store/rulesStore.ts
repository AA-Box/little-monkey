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

export interface MemoryFact {
  id: string;
  text: string;
  source: "agent" | "user";
  created_at: string;
  enabled?: boolean;
  source_turn_id?: string | null;
}

export interface RulesStore {
  rules: RuleFile[];
  facts: MemoryFact[];
  /** Refresh standing instructions, memories, and the structured standards
   * index together so every existing turn runner that refreshes rules also
   * sees externally-edited/approved standards without a second orchestration
   * integration point. Standards failures remain isolated and cannot block a
   * normal turn. */
  refresh: () => Promise<void>;
}

let latestRefreshRequest = 0;

export const useRulesStore = create<RulesStore>((set) => ({
  rules: [],
  facts: [],

  refresh: async () => {
    const requestId = ++latestRefreshRequest;
    const [rules, facts] = await Promise.all([
      invoke<RuleFile[]>("rules_read").catch(() => [] as RuleFile[]),
      invoke<MemoryFact[]>("memory_list").catch(() => [] as MemoryFact[]),
      useStandardsStore.getState().refresh().catch(() => undefined),
    ]);
    if (requestId !== latestRefreshRequest) return;
    set({ rules, facts });
  },
}));
