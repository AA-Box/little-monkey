import { create } from "zustand";

import {
  BriefStudioPolicyError,
  buildKnowledgeStackSource,
  buildPastedSource,
  buildSessionSource,
  generateBriefAsset,
  type BriefAssetType,
  type BriefSourceKind,
  type GeneratedBriefAsset,
  type RawSourceBlock,
} from "../lib/briefStudio";
import { sessionMessages, useSessionStore } from "./sessionStore";
import { useStackStore } from "./stackStore";
import { DEFAULT_HYBRID_CONFIG, useKnowledgeV2Store } from "./knowledgeV2Store";

/**
 * Orchestrates Source-Grounded Brief Studio (ROADMAP.md Phase 7, item 7):
 * holds the panel's source-picker/asset-type selection and drives
 * `briefStudio.ts`'s pure `generateBriefAsset` with material assembled from
 * whichever other store the user picked as a source — `sessionStore.ts` for
 * a chat session, `stackStore.ts`/`knowledgeV2Store.ts` for a knowledge
 * stack query, or the user's own pasted text. No new Rust command: the
 * knowledge-stack path reuses `knowledgeV2Store.query()` (the same hybrid
 * retrieval the Knowledge panel's inspector already calls) purely to fetch
 * grounding chunks, not to add a second retrieval mechanism.
 */

function errorMessage(error: unknown): string {
  if (error instanceof BriefStudioPolicyError) return error.message;
  if (error instanceof Error) return error.message;
  if (typeof error === "string") return error;
  try {
    return JSON.stringify(error);
  } catch {
    return String(error);
  }
}

interface BriefStudioState {
  sourceKind: BriefSourceKind;
  assetType: BriefAssetType;
  requireLocalOnly: boolean;

  pastedLabel: string;
  pastedText: string;

  selectedSessionId: string | null;

  selectedStackId: string | null;
  focusQuery: string;

  generating: boolean;
  error: string | null;
  result: GeneratedBriefAsset | null;
  /** Assets generated so far this panel session, most recent first — lets
   * the user glance back at an earlier asset type for the same source
   * without re-running generation. Purely in-memory, never persisted. */
  history: GeneratedBriefAsset[];

  setSourceKind: (kind: BriefSourceKind) => void;
  setAssetType: (assetType: BriefAssetType) => void;
  setRequireLocalOnly: (value: boolean) => void;
  setPastedLabel: (value: string) => void;
  setPastedText: (value: string) => void;
  setSelectedSessionId: (id: string | null) => void;
  setSelectedStackId: (id: string | null) => void;
  setFocusQuery: (value: string) => void;
  generate: () => Promise<void>;
  clearResult: () => void;
}

/** How many top hits a knowledge-stack generation retrieves — generous
 * enough for a real brief/study-guide without dragging in the whole stack;
 * `normalizeSourceBlocks` (`briefStudio.ts`) still caps total characters and
 * block count on top of this. */
const KNOWLEDGE_QUERY_TOKEN_BUDGET = 4000;

export const useBriefStudioStore = create<BriefStudioState>((set, get) => ({
  sourceKind: "pasted",
  assetType: "brief",
  requireLocalOnly: false,

  pastedLabel: "",
  pastedText: "",

  selectedSessionId: null,

  selectedStackId: null,
  focusQuery: "",

  generating: false,
  error: null,
  result: null,
  history: [],

  setSourceKind: (sourceKind) => set({ sourceKind, error: null }),
  setAssetType: (assetType) => set({ assetType, error: null }),
  setRequireLocalOnly: (requireLocalOnly) => set({ requireLocalOnly }),
  setPastedLabel: (pastedLabel) => set({ pastedLabel }),
  setPastedText: (pastedText) => set({ pastedText }),
  setSelectedSessionId: (selectedSessionId) => set({ selectedSessionId }),
  setSelectedStackId: (selectedStackId) => set({ selectedStackId }),
  setFocusQuery: (focusQuery) => set({ focusQuery }),

  clearResult: () => set({ result: null, error: null }),

  generate: async () => {
    const state = get();
    set({ generating: true, error: null });
    try {
      let source;

      if (state.sourceKind === "pasted") {
        if (!state.pastedText.trim()) {
          throw new Error("Paste some source text first.");
        }
        source = buildPastedSource(state.pastedLabel, state.pastedText);
      } else if (state.sourceKind === "session") {
        if (!state.selectedSessionId) {
          throw new Error("Pick a chat session first.");
        }
        const session = useSessionStore.getState().sessions.find((s) => s.id === state.selectedSessionId);
        const messages = sessionMessages(state.selectedSessionId);
        source = buildSessionSource(session?.title ?? "Chat session", messages);
      } else {
        if (!state.selectedStackId) {
          throw new Error("Pick a knowledge stack first.");
        }
        if (!state.focusQuery.trim()) {
          throw new Error("Enter a focus topic to retrieve relevant material from the stack.");
        }
        const stack = useStackStore.getState().stacks.find((s) => s.id === state.selectedStackId);
        const response = await useKnowledgeV2Store.getState().query(
          state.selectedStackId,
          state.focusQuery.trim(),
          DEFAULT_HYBRID_CONFIG,
          [],
          false,
          KNOWLEDGE_QUERY_TOKEN_BUDGET,
        );
        const hits: RawSourceBlock[] = response.search.hits.map((hit) => {
          const headingSuffix = hit.chunk.heading_path.length ? ` > ${hit.chunk.heading_path.join(" > ")}` : "";
          return { label: `${hit.chunk.citation.canonical_uri}${headingSuffix}`, text: hit.chunk.text };
        });
        if (hits.length === 0) {
          throw new Error("No matching material found in that knowledge stack for this topic.");
        }
        source = buildKnowledgeStackSource(stack?.name ?? "Knowledge stack", hits);
      }

      const asset = await generateBriefAsset(source, state.assetType, {
        requireLocalOnly: state.requireLocalOnly,
      });
      set((current) => ({
        generating: false,
        result: asset,
        history: [asset, ...current.history].slice(0, 20),
      }));
    } catch (error) {
      set({ generating: false, error: errorMessage(error) });
    }
  },
}));
