import { create } from "zustand";
import { invoke, isTauri } from "@tauri-apps/api/core";

import {
  buildBrowserEvidenceHits,
  buildConnectedAppHits,
  buildKnowledgeHits,
  buildSessionHits,
  buildTaskHits,
  buildWorkspaceFileHits,
  combineUniversalSearchResults,
  escapeRegExp,
  type GrepMatchLike,
  type UniversalSearchHit,
} from "../lib/universalSearch";
import { useSessionStore } from "./sessionStore";
import { useRunStore } from "./runStore";
import { useWorkspaceStore } from "./workspaceStore";
import { useBrowserWorkbenchStore } from "./browserWorkbenchStore";
import { useMcpStore } from "./mcpStore";
import { useStackStore } from "./stackStore";

/**
 * Permission-Aware Universal Search (ROADMAP.md Phase 7): the store half of
 * `lib/universalSearch.ts`'s fan-out. `GlobalSearch.tsx` already drives the
 * backend-indexed profile search (`lib/profileSearch.ts`, covering chat
 * messages / Crew transcripts / run events); this store adds the sources
 * that live only in already-loaded frontend state or need a live backend
 * call scoped to what's currently attached — workspace files, run/task
 * summaries, knowledge stacks, browser workbench evidence, and connected
 * MCP servers. Every hit is built by `lib/universalSearch.ts`'s pure
 * functions, which apply the access check (workspace root attachment /
 * connector connection state) before a hit is ever added to `hits` — a
 * result a source is currently inaccessible for is dropped there, never
 * merely hidden by this store or the panel.
 */

const KNOWLEDGE_HITS_PER_STACK = 5;

export interface UniversalSearchOptions {
  includeArchived: boolean;
}

interface UniversalSearchStoreState {
  hits: UniversalSearchHit[];
  excludedCount: number;
  loading: boolean;
  error: string | null;
  run: (query: string, options: UniversalSearchOptions) => Promise<void>;
  clear: () => void;
}

function errorMessage(error: unknown): string {
  if (error instanceof Error) return error.message;
  if (typeof error === "string") return error;
  try {
    return JSON.stringify(error);
  } catch {
    return String(error);
  }
}

/** Regex-searches every currently attached workspace root (primary plus any
 * secondary folders) via the existing `tool_grep` command — already
 * sandboxed to attached roots by `workspace::resolve_path_and_root` on the
 * Rust side, so this can never surface a path outside them. A root whose
 * search errors (e.g. a secondary folder just detached mid-search) is
 * skipped rather than failing the whole query. */
async function grepAttachedWorkspaceRoots(query: string): Promise<GrepMatchLike[]> {
  if (!isTauri() || !query.trim()) return [];
  const roots = useWorkspaceStore.getState().roots;
  if (roots.length === 0) return [];
  const pattern = escapeRegExp(query.trim());
  const perRoot = await Promise.allSettled(
    roots.map((root) =>
      invoke<GrepMatchLike[]>("tool_grep", {
        pattern,
        path: root.is_primary ? undefined : root.label,
      }),
    ),
  );
  return perRoot.flatMap((result) => (result.status === "fulfilled" ? result.value : []));
}

/** Queries every knowledge stack already indexed locally on this device via
 * the existing `stacks_query` command (the same retrieval call
 * `agentLoop.ts`'s doc-chat mode uses) — a stack that isn't in this device's
 * own `useStackStore` list was never a candidate in the first place, so
 * there's nothing further to gate. A stack whose query errors is skipped
 * rather than failing the whole search. */
async function queryLocalKnowledgeStacks(
  query: string,
): Promise<Array<{ stackId: string; stackName: string; sourcePath: string; text: string }>> {
  if (!isTauri() || !query.trim()) return [];
  const indexedStacks = useStackStore.getState().stacks.filter((stack) => stack.indexed_at != null);
  if (indexedStacks.length === 0) return [];
  try {
    const results = await useStackStore
      .getState()
      .query(
        indexedStacks.map((stack) => stack.id),
        query,
        KNOWLEDGE_HITS_PER_STACK,
      );
    return results.map((result) => ({
      stackId: result.stack_id,
      stackName: result.stack_name,
      sourcePath: result.source_path,
      text: result.text,
    }));
  } catch {
    return [];
  }
}

let sequence = 0;

export const useUniversalSearchStore = create<UniversalSearchStoreState>((set) => ({
  hits: [],
  excludedCount: 0,
  loading: false,
  error: null,

  run: async (query, options) => {
    const mySequence = ++sequence;
    if (!query.trim()) {
      set({ hits: [], excludedCount: 0, loading: false, error: null });
      return;
    }
    set({ loading: true, error: null });
    try {
      const { sessions } = useSessionStore.getState();
      const { runs } = useRunStore.getState();
      const roots = useWorkspaceStore.getState().roots;
      const { pendingBySession } = useBrowserWorkbenchStore.getState();
      const { servers } = useMcpStore.getState();

      const [grepMatches, knowledgeHits] = await Promise.all([
        grepAttachedWorkspaceRoots(query),
        queryLocalKnowledgeStacks(query),
      ]);
      if (mySequence !== sequence) return;

      const sessionsById = new Map(sessions.map((session) => [session.id, session]));

      const combined = combineUniversalSearchResults([
        buildSessionHits(sessions, query, roots, options.includeArchived),
        buildWorkspaceFileHits(grepMatches, query),
        buildTaskHits(runs, query, roots, options.includeArchived),
        buildKnowledgeHits(knowledgeHits, query),
        buildBrowserEvidenceHits(pendingBySession, sessionsById, query, roots),
        buildConnectedAppHits(servers, query),
      ]);

      set({ hits: combined.hits, excludedCount: combined.excludedCount, loading: false, error: null });
    } catch (error) {
      if (mySequence === sequence) {
        set({ loading: false, error: errorMessage(error) });
      }
    }
  },

  clear: () => set({ hits: [], excludedCount: 0, loading: false, error: null }),
}));
