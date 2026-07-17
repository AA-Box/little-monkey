import { create } from "zustand";

import {
  buildCrossRepoIndex,
  queryImpact,
  type CrossRepoFileRef,
  type CrossRepoSymbol,
  type ImpactResult,
} from "../lib/crossRepoIndex";
import { useWorkspaceStore } from "./workspaceStore";

/**
 * Cross-Repo Code Intelligence (ROADMAP.md Phase 7): holds the symbol index
 * built by `lib/crossRepoIndex.ts` for the current workspace session, plus
 * whatever "impact" query the panel most recently ran. Purely in-memory —
 * "rebuild on demand" per the roadmap's own MVP scope, not a persisted or
 * auto-refreshing background index (see follow-ups in crossRepoIndex.ts's
 * doc comment).
 */

function errorMessage(error: unknown): string {
  if (error instanceof Error) return error.message;
  if (typeof error === "string") return error;
  try {
    return JSON.stringify(error);
  } catch {
    return String(error);
  }
}

export type CrossRepoIndexStatus = "idle" | "building" | "ready" | "error";

interface CrossRepoIndexState {
  status: CrossRepoIndexStatus;
  symbols: CrossRepoSymbol[];
  files: CrossRepoFileRef[];
  builtAtMs: number | null;
  error: string | null;
  /** Rebuilds the index from whatever workspace roots are currently
   * attached (`workspaceStore.roots`). Safe to call again while `building`
   * — the caller (the panel's "Rebuild" button) simply disables itself. */
  rebuild: () => Promise<void>;

  impactQuery: string;
  impact: ImpactResult | null;
  impactLoading: boolean;
  impactError: string | null;
  setImpactQuery: (value: string) => void;
  runImpactQuery: (symbolName: string) => Promise<void>;
  clearImpact: () => void;
}

export const useCrossRepoIndexStore = create<CrossRepoIndexState>((set, get) => ({
  status: "idle",
  symbols: [],
  files: [],
  builtAtMs: null,
  error: null,

  rebuild: async () => {
    const { roots } = useWorkspaceStore.getState();
    if (roots.length === 0) {
      set({
        status: "error",
        error: "No workspace folder is open. Open a folder first.",
        symbols: [],
        files: [],
      });
      return;
    }
    set({ status: "building", error: null });
    try {
      const { symbols, files } = await buildCrossRepoIndex(roots);
      set({ status: "ready", symbols, files, builtAtMs: Date.now(), error: null });
    } catch (err) {
      set({ status: "error", error: errorMessage(err) });
    }
  },

  impactQuery: "",
  impact: null,
  impactLoading: false,
  impactError: null,

  setImpactQuery: (value) => set({ impactQuery: value }),

  runImpactQuery: async (symbolName) => {
    const trimmed = symbolName.trim();
    if (!trimmed) return;
    const { roots } = useWorkspaceStore.getState();
    const { symbols, files } = get();
    set({ impactLoading: true, impactError: null, impactQuery: trimmed });
    try {
      const impact = await queryImpact({ symbolName: trimmed, roots, symbols, files });
      set({ impact, impactLoading: false });
    } catch (err) {
      set({ impactLoading: false, impactError: errorMessage(err) });
    }
  },

  clearImpact: () => set({ impact: null, impactQuery: "", impactError: null }),
}));
