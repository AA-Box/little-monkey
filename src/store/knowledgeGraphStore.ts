/**
 * Knowledge Graph Explorer (ROADMAP.md Phase 7, item 10) — builds and holds
 * the in-memory entity/relationship graph, and answers free-text "how is X
 * related to Y" queries against it.
 *
 * Deliberately reuses existing primitives instead of adding anything new:
 * - Source text comes from `knowledgeV2Store.ts`'s `query` (Knowledge 2.0
 *   hybrid search over each selected stack) and `sessionStore.ts`'s active
 *   session transcript — no new backend command, no new indexing pass.
 * - The extraction model call is the SAME `agentLoop.ts`'s `resolveTarget` +
 *   `turnEngine.ts`'s `attemptStream` pairing `riskJudge.ts`'s
 *   `classifyToolCall` uses for its own one-shot, non-streaming, tool-less
 *   judge call (`sendForSummary`'s callers do the identical thing for
 *   context-trim summaries) — this is just a THIRD one-shot consumer of the
 *   exact same transport, not a new call path.
 * - `usageStore`/`usageHistoryStore` writes are skipped (`recordUsage:
 *   false`) for these calls, exactly like `subagent.ts`'s child-turn calls —
 *   a graph build is a background utility call, not a chat turn, and must
 *   never clobber a real session's context-usage ring.
 */
import { create } from "zustand";

import { resolveTarget } from "../lib/agentLoop";
import { attemptStream } from "../lib/turnEngine";
import { textContent } from "../lib/llamaClient";
import {
  answerRelationQuery,
  buildKnowledgeGraph,
  chunkIntoSourceBatches,
  emptyGraph,
  type KnowledgeGraph,
  type RelationQueryResult,
  type SourceBatch,
} from "../lib/knowledgeGraph";
import { useStackStore } from "./stackStore";
import { DEFAULT_HYBRID_CONFIG, useKnowledgeV2Store } from "./knowledgeV2Store";
import { useSessionStore } from "./sessionStore";

/** Synthetic session id these one-shot extraction calls are attributed
 * under (only relevant for `rateLimitTracker`'s per-provider bookkeeping,
 * which is keyed independent of `recordUsage` — see `turnEngine.ts`'s
 * `attemptStream`). Never a real chat session, so it never collides with
 * one. */
const GRAPH_BUILD_SESSION_ID = "__knowledge-graph-explorer__";

/** How many extraction calls one build issues at most — bounds both cost
 * and wall-clock time for the MVP. Each call is one small, bounded batch
 * (see `chunkIntoSourceBatches`), so this caps total source coverage per
 * build rather than any one call's size. */
const DEFAULT_MAX_BATCHES = 8;

/** How many representative chunks are pulled per stack via a single hybrid
 * search query — Knowledge 2.0 has no "dump everything" command, so a
 * broad, topic-agnostic query is used to surface a representative sample
 * of each stack's content for extraction. */
const STACK_OVERVIEW_QUERY = "overview key decisions owners dependencies people files terms";

export interface KnowledgeGraphBuildOptions {
  /** Stack ids to include; omit (or empty) to include every stack. */
  stackIds?: string[];
  /** Whether to also extract from the current chat session's transcript. */
  includeActiveSession?: boolean;
  maxBatches?: number;
}

interface KnowledgeGraphStore extends KnowledgeGraph {
  building: boolean;
  buildError: string | null;
  batchErrors: string[];
  batchCount: number;
  lastBuiltAtMs: number | null;
  queryText: string;
  queryResult: RelationQueryResult | null;
  build: (options?: KnowledgeGraphBuildOptions) => Promise<void>;
  queryRelation: (text: string) => void;
  clearQuery: () => void;
  reset: () => void;
}

function errorText(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

/** Turns one knowledge-stack's retrieval hits into raw `(quote, locator)`
 * pairs ready for `chunkIntoSourceBatches` — the locator is a
 * `canonical_uri#charStart-charEnd` pointer back into that source object,
 * the same citation shape the Knowledge Inspector already shows. */
async function gatherStackSpans(stackId: string): Promise<Array<{ quote: string; locator: string }>> {
  const response = await useKnowledgeV2Store.getState().query(
    stackId,
    STACK_OVERVIEW_QUERY,
    DEFAULT_HYBRID_CONFIG,
    [],
    false,
    4000,
  );
  return response.search.hits.map((hit) => ({
    quote: hit.chunk.text,
    locator: `${hit.chunk.citation.canonical_uri}#${hit.chunk.citation.block_char_start}-${hit.chunk.citation.block_char_end}`,
  }));
}

/** Turns the active chat session's transcript into raw `(quote, locator)`
 * pairs, one per message — the locator identifies the message by index and
 * role since chat messages have no stable id of their own in this store. */
function gatherSessionSpans(): Array<{ quote: string; locator: string }> {
  const state = useSessionStore.getState();
  const session = state.sessions.find((s) => s.id === state.activeSessionId);
  if (!session) return [];
  return session.messages
    .map((message, index) => ({ message, index }))
    .filter(({ message }) => message.role === "user" || message.role === "assistant")
    .map(({ message, index }) => ({
      quote: textContent(message.content),
      locator: `message #${index} (${message.role})`,
    }))
    .filter((span) => span.quote.trim().length > 0);
}

async function gatherSourceBatches(options: KnowledgeGraphBuildOptions): Promise<{ batches: SourceBatch[]; errors: string[] }> {
  const allStacks = useStackStore.getState().stacks;
  const selectedStacks = options.stackIds && options.stackIds.length > 0
    ? allStacks.filter((stack) => options.stackIds!.includes(stack.id))
    : allStacks;

  const errors: string[] = [];
  let batches: SourceBatch[] = [];

  for (const stack of selectedStacks) {
    try {
      const rawSpans = await gatherStackSpans(stack.id);
      batches = batches.concat(chunkIntoSourceBatches("knowledge_stack", stack.id, stack.name, rawSpans, `stack-${stack.id}`));
    } catch (err) {
      errors.push(`${stack.name}: ${errorText(err)}`);
    }
  }

  if (options.includeActiveSession) {
    const state = useSessionStore.getState();
    const session = state.sessions.find((s) => s.id === state.activeSessionId);
    const rawSpans = gatherSessionSpans();
    if (session && rawSpans.length > 0) {
      batches = batches.concat(chunkIntoSourceBatches("chat_session", session.id, session.title, rawSpans, `session-${session.id}`));
    }
  }

  const maxBatches = options.maxBatches ?? DEFAULT_MAX_BATCHES;
  return { batches: batches.slice(0, maxBatches), errors };
}

export const useKnowledgeGraphStore = create<KnowledgeGraphStore>((set, get) => ({
  nodes: [],
  edges: [],
  building: false,
  buildError: null,
  batchErrors: [],
  batchCount: 0,
  lastBuiltAtMs: null,
  queryText: "",
  queryResult: null,

  build: async (options = {}) => {
    set({ building: true, buildError: null });
    try {
      const { batches, errors: gatherErrors } = await gatherSourceBatches(options);
      if (batches.length === 0) {
        set({
          ...emptyGraph(),
          building: false,
          batchErrors: gatherErrors,
          batchCount: 0,
          lastBuiltAtMs: Date.now(),
          buildError: gatherErrors.length > 0 ? null : "No source content was found to build a graph from.",
        });
        return;
      }

      const target = await resolveTarget();
      const result = await buildKnowledgeGraph(batches, (messages, signal) =>
        attemptStream(target, messages, [], signal, undefined, GRAPH_BUILD_SESSION_ID, undefined, false),
      );

      set({
        nodes: result.nodes,
        edges: result.edges,
        batchErrors: [...gatherErrors, ...result.batchErrors],
        batchCount: batches.length,
        lastBuiltAtMs: Date.now(),
        building: false,
      });
    } catch (err) {
      set({ building: false, buildError: errorText(err) });
    }
  },

  queryRelation: (text: string) => {
    const { nodes, edges } = get();
    const result = answerRelationQuery({ nodes, edges }, text);
    set({ queryText: text, queryResult: result });
  },

  clearQuery: () => set({ queryText: "", queryResult: null }),

  reset: () =>
    set({
      ...emptyGraph(),
      building: false,
      buildError: null,
      batchErrors: [],
      batchCount: 0,
      lastBuiltAtMs: null,
      queryText: "",
      queryResult: null,
    }),
}));
