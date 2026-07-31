import { create } from "zustand";
import { invoke, isTauri } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { errorMessage } from "../lib/errors";

/** Mirrors the Rust `EmbeddingBackend` enum (src-tauri/src/stacks.rs) exactly. */
export type EmbeddingBackend = "llama" | "ollama";

/** Mirrors the Rust `SourceKind` enum exactly. */
export type SourceKind = "folder" | "file";

/** Mirrors the Rust `EmbeddingSpec` struct exactly — pins the exact model a
 * stack's vectors were embedded with. A mismatch on reload (backend/model/dim)
 * hard-fails to "reindex required" on the Rust side rather than silently
 * mixing vectors from two different embedding spaces. */
export interface EmbeddingSpec {
  backend: EmbeddingBackend;
  model_id_or_tag: string;
  dim: number;
  query_prefix: string;
  doc_prefix: string;
}

/** Mirrors the Rust `StackSource` struct exactly. */
export interface StackSource {
  path: string;
  kind: SourceKind;
}

/** Mirrors the Rust `KnowledgeStack` struct (src-tauri/src/stacks.rs) exactly. */
export interface KnowledgeStack {
  id: string;
  name: string;
  sources: StackSource[];
  embedding: EmbeddingSpec;
  chunk_chars: number;
  chunk_overlap: number;
  indexed_at: number | null;
  chunk_count: number;
}

/** Mirrors the Rust `StackQueryResult` struct exactly — one retrieval hit
 * from `stacks_query`, used by the Knowledge panel's test-search box. */
export interface StackQueryResult {
  stack_id: string;
  stack_name: string;
  source_path: string;
  score: number;
  text: string;
  heading: string | null;
}

/** Payload of the `stacks://index-progress` event emitted by
 * `stacks.rs::reindex_impl`. */
interface IndexProgressEvent {
  stack_id: string;
  files_done: number;
  files_total: number;
  chunks: number;
  phase: string;
}

/** Status of the managed embeddings-only `llama-server` instance (port 8091)
 * — mirrors `LlamaStatus` in `modelStore.ts`, but for the separate
 * `AppState::embed_llama` process. */
export type EmbedServerStatus = "stopped" | "starting" | "ready" | "error";

/** Payload of the `embed://status` event / `embed_server_status` return value. */
interface EmbedStatusEvent {
  status: EmbedServerStatus;
  port: number;
  model_path: string | null;
}

/**
 * Known prefix/dimension requirements for the curated embedding models
 * (`nomic-embed-text-v1.5`, `bge-m3` — see `models.rs::curated_models()`).
 * Not derived from any backend field: `ModelInfo` only carries `kind`, so
 * this small lookup is the frontend's own record of what each curated
 * embedding model actually needs. A stack created against a curated model
 * not listed here (or a custom/Ollama tag) falls back to no prefix.
 */
export const CURATED_EMBEDDING_SPECS: Record<string, { dim: number; queryPrefix: string; docPrefix: string }> = {
  "nomic-embed-text-v1.5": { dim: 768, queryPrefix: "search_query: ", docPrefix: "search_document: " },
  "bge-m3": { dim: 1024, queryPrefix: "", docPrefix: "" },
};

interface StackStore {
  stacks: KnowledgeStack[];
  /** Stack id -> latest `stacks://index-progress` event for it. Cleared when
   * a `phase: "done"` event arrives isn't necessary — the panel just reads
   * `phase` to decide whether to keep showing a progress bar. */
  indexProgress: Record<string, IndexProgressEvent>;
  /** Stack id -> error message from a failed reindex, cleared on the next
   * successful (or newly-started) reindex of that stack. */
  reindexError: Record<string, string>;
  /** Stack id -> whether `stacks_is_stale` last reported a source file
   * modified after `indexed_at` (RAG design doc slice 4). Only ever
   * populated for indexed stacks — an unindexed stack simply has no entry
   * here, since `KnowledgePanel.tsx` already shows "Not indexed yet" for
   * those instead of a staleness badge. */
  staleById: Record<string, boolean>;

  embedStatus: EmbedServerStatus;
  embedPort: number;
  embedModelPath: string | null;
  /** Error from the last failed `embed_server_start` call. */
  embedError: string | null;

  refresh: () => Promise<void>;
  create: (name: string, embedding: EmbeddingSpec) => Promise<KnowledgeStack>;
  remove: (id: string) => Promise<void>;
  rename: (id: string, name: string) => Promise<void>;
  addSource: (id: string, path: string, kind: SourceKind) => Promise<void>;
  removeSource: (id: string, path: string) => Promise<void>;
  reindex: (id: string) => Promise<void>;
  cancelIndex: (id: string) => Promise<void>;
  query: (stackIds: string[], query: string, k?: number) => Promise<StackQueryResult[]>;
  /** Refreshes `staleById` for every currently-indexed stack in `stacks` by
   * calling `stacks_is_stale` for each, in parallel. A per-stack failure
   * (e.g. a source path no longer resolvable) is swallowed rather than
   * failing the whole refresh — the badge just stays at its previous value
   * for that stack. Kept separate from `refresh()` (rather than folded into
   * it) so `refresh()`'s own call shape stays exactly "list, then done" —
   * callers that want the badge call both, see `KnowledgePanel.tsx`. */
  refreshStale: () => Promise<void>;

  refreshEmbedStatus: () => Promise<void>;
  startEmbedServer: (modelPath: string) => Promise<void>;
  stopEmbedServer: () => Promise<void>;
}

export const useStackStore = create<StackStore>((set, get) => ({
  stacks: [],
  indexProgress: {},
  reindexError: {},
  staleById: {},

  embedStatus: "stopped",
  embedPort: 8091,
  embedModelPath: null,
  embedError: null,

  refresh: async () => {
    const stacks = await invoke<KnowledgeStack[]>("stacks_list");
    set({ stacks });
  },

  create: async (name, embedding) => {
    const stack = await invoke<KnowledgeStack>("stacks_create", { name, embedding });
    await get().refresh();
    return stack;
  },

  remove: async (id) => {
    await invoke("stacks_delete", { id });
    set((state) => {
      const { [id]: _discardProgress, ...restProgress } = state.indexProgress;
      const { [id]: _discardError, ...restErrors } = state.reindexError;
      return { indexProgress: restProgress, reindexError: restErrors };
    });
    await get().refresh();
  },

  rename: async (id, name) => {
    await invoke("stacks_rename", { id, name });
    await get().refresh();
  },

  addSource: async (id, path, kind) => {
    await invoke("stacks_add_source", { id, path, kind });
    await get().refresh();
  },

  removeSource: async (id, path) => {
    await invoke("stacks_remove_source", { id, path });
    await get().refresh();
  },

  reindex: async (id) => {
    set((state) => {
      const { [id]: _discard, ...rest } = state.reindexError;
      return { reindexError: rest };
    });
    try {
      await invoke("stacks_reindex", { id });
      await get().refresh();
      // A completed reindex bumps `indexed_at` past every source mtime, so
      // the stale badge (`staleById`, only ever set by `refreshStale`) must
      // be recomputed here too — otherwise it keeps showing "Needs reindex"
      // next to the now-fresh `indexed_at` until the Settings modal happens
      // to be closed and reopened (the only other place `refreshStale` runs,
      // see `KnowledgePanel.tsx`'s mount effect).
      await get().refreshStale();
    } catch (err) {
      const message = errorMessage(err);
      set((state) => ({ reindexError: { ...state.reindexError, [id]: message } }));
      throw err;
    }
  },

  cancelIndex: async (id) => {
    await invoke("stacks_cancel_index", { id });
  },

  query: async (stackIds, query, k) => {
    return invoke<StackQueryResult[]>("stacks_query", { stackIds, query, k });
  },

  refreshStale: async () => {
    const indexed = get().stacks.filter((s) => s.indexed_at != null);
    const entries = await Promise.all(
      indexed.map(async (stack): Promise<[string, boolean] | null> => {
        try {
          const stale = await invoke<boolean>("stacks_is_stale", { id: stack.id });
          return [stack.id, stale];
        } catch (error) {
          console.error(`Failed to check staleness for stack ${stack.id}`, error);
          return null;
        }
      }),
    );
    set((state) => {
      const staleById = { ...state.staleById };
      for (const entry of entries) {
        if (entry) staleById[entry[0]] = entry[1];
      }
      return { staleById };
    });
  },

  refreshEmbedStatus: async () => {
    const status = await invoke<EmbedStatusEvent>("embed_server_status");
    set({ embedStatus: status.status, embedPort: status.port, embedModelPath: status.model_path });
  },

  startEmbedServer: async (modelPath) => {
    set({ embedStatus: "starting", embedError: null });
    try {
      await invoke("embed_server_start", { modelPath });
    } catch (err) {
      const message = errorMessage(err);
      set({ embedError: message });
      throw err;
    }
  },

  stopEmbedServer: async () => {
    await invoke("embed_server_stop");
    set({ embedStatus: "stopped" });
  },
}));

// These backend events only exist under the Tauri shell — in plain-browser
// dev (`vite` without it) `listen` itself throws, so don't subscribe at all.
if (isTauri()) {
  void listen<IndexProgressEvent>("stacks://index-progress", (event) => {
    useStackStore.setState((state) => ({
      indexProgress: { ...state.indexProgress, [event.payload.stack_id]: event.payload },
    }));
  }).catch((error) => {
    console.error("Failed to listen for stacks://index-progress events", error);
  });

  void listen<EmbedStatusEvent>("embed://status", (event) => {
    useStackStore.setState({
      embedStatus: event.payload.status,
      embedPort: event.payload.port,
      embedModelPath: event.payload.model_path,
    });
  }).catch((error) => {
    console.error("Failed to listen for embed://status events", error);
  });
}
