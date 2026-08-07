import { create } from "zustand";
import { invoke, isTauri } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { useStackStore, type KnowledgeStack } from "./stackStore";
import { errorMessage } from "../lib/errors";

export type KnowledgeConnector =
  | { kind: "local_file"; path: string }
  | { kind: "local_folder"; path: string }
  | { kind: "project"; path: string }
  | {
      kind: "url";
      url: string;
      allowed_origin: string;
      max_depth: number;
      max_pages: number;
      obey_robots: boolean;
      allow_loopback: boolean;
    }
  | {
      kind: "sitemap";
      url: string;
      allowed_origin: string;
      max_pages: number;
      obey_robots: boolean;
      allow_loopback: boolean;
    }
  | { kind: "selected_chats"; session_ids: string[] }
  | {
      kind: "web_dav";
      url: string;
      username: string;
      credential_ref: string;
      allow_loopback: boolean;
    }
  | {
      kind: "git_hub_repo";
      owner: string;
      repo: string;
      git_ref: string | null;
      path_prefix: string | null;
      connector_account_id: string;
    }
  | {
      kind: "s3_bucket";
      endpoint: string;
      bucket: string;
      prefix: string | null;
      region: string;
      connector_account_id: string;
    }
  | { kind: "watched_folder"; path: string; debounce_ms: number }
  | { kind: "notion_pages"; connector_account_id: string; root_id: string }
  | { kind: "slack_channels"; connector_account_id: string; channel_ids: string[] }
  | { kind: "jira_project"; connector_account_id: string; project_key: string };

/**
 * True for every connector kind whose credential is a Connector Catalog
 * reference (`connector_account_id`, see `connectorsStore.ts`) rather than a
 * pasted secret handled directly by this panel — generalizes the WebDAV-
 * specific `webdavPassword` plumbing's underlying idea (never store a raw
 * secret in this store) to every connector added after it.
 */
export function connectorUsesAccountReference(
  kind: KnowledgeConnector["kind"],
): kind is "git_hub_repo" | "s3_bucket" | "notion_pages" | "slack_channels" | "jira_project" {
  return (
    kind === "git_hub_repo" ||
    kind === "s3_bucket" ||
    kind === "notion_pages" ||
    kind === "slack_channels" ||
    kind === "jira_project"
  );
}

export interface ConnectorObjectState {
  object_id: string;
  canonical_uri: string;
  content_sha256: string;
  etag: string | null;
  modified_unix_ms: number | null;
  chunk_ids: string[];
}

export interface KnowledgeSourceV2 {
  id: string;
  stack_id: string;
  label: string;
  enabled: boolean;
  connector: KnowledgeConnector;
  cursor: string | null;
  checkpoint: string | null;
  last_refresh_at_ms: number | null;
  last_error: string | null;
  objects: ConnectorObjectState[];
  retries: Array<{ attempted_at_ms: number; message: string }>;
}

export interface KnowledgeRefreshProgress {
  stack_id: string;
  source_id: string | null;
  phase: "enumerating" | "fetching" | "extracting" | "embedding" | "publishing" | "done";
  objects_done: number;
  objects_total: number;
  chunks: number;
  reused_chunks: number;
}

export interface KnowledgeRefreshReport {
  stack_id: string;
  generation_id: string;
  parent_generation_id: string | null;
  source_count: number;
  object_count: number;
  changed_objects: number;
  unchanged_objects: number;
  deleted_objects: number;
  embedded_chunks: number;
  reused_chunks: number;
  warnings: string[];
  duration_ms: number;
}

/** Mirrors the Rust `KnowledgeV1ImportReport` (camelCase-serialized). */
export interface KnowledgeV1ImportReport {
  stackId: string;
  generationId: string;
  objectCount: number;
  chunkCount: number;
  skippedRows: number;
  dimension: number;
  warnings: string[];
}

export interface KnowledgeBackgroundRefreshConfig {
  enabled: boolean;
  intervalMinutes: number;
  stackIds: string[];
  lastAttemptMs: number | null;
  lastSuccessMs: number | null;
  nextDueMs: number | null;
  lastError: string | null;
  consecutiveFailures: number;
}

export interface HybridSearchConfig {
  lexical_candidates: number;
  vector_candidates: number;
  final_results: number;
  rrf_k: number;
  lexical_weight_micros: number;
  vector_weight_micros: number;
  rerank_candidates: number;
}

export const DEFAULT_HYBRID_CONFIG: HybridSearchConfig = {
  lexical_candidates: 50,
  vector_candidates: 50,
  final_results: 8,
  rrf_k: 60,
  lexical_weight_micros: 1_000_000,
  vector_weight_micros: 1_000_000,
  rerank_candidates: 20,
};

export interface DocumentLocation {
  kind: string;
  [key: string]: unknown;
}

export interface InspectorCandidate {
  chunk_id: string;
  lexical_rank: number | null;
  lexical_bm25_micros: number | null;
  lexical_rrf_units: number;
  vector_rank: number | null;
  vector_similarity_micros: number | null;
  vector_rrf_units: number;
  fused_score_units: number;
  rerank_score_micros: number | null;
  final_rank: number | null;
  citation: {
    citation_id: string;
    source_id: string;
    object_id: string;
    canonical_uri: string;
    location: DocumentLocation;
    block_char_start: number;
    block_char_end: number;
  };
  content_preview: string;
  content_type: string;
  confidence_micros: number | null;
  low_confidence: boolean;
}

export interface KnowledgeInspectorResponse {
  query_id: string;
  normalized_query: string;
  excluded_source_ids: string[];
  token_budget: number;
  estimated_context_tokens: number;
  final_context: string;
  search: {
    hits: Array<{
      rank: number;
      chunk: {
        chunk_id: string;
        source_id: string;
        object_id: string;
        text: string;
        heading_path: string[];
        location: DocumentLocation;
        citation: InspectorCandidate["citation"];
        content_type: string;
        confidence_micros: number | null;
        low_confidence: boolean;
      };
      fused_score_units: number;
      rerank_score_micros: number | null;
    }>;
    diagnostics: {
      diagnostic_version: number;
      generation_id: string;
      index_digest: string;
      query_sha256: string;
      embedding_fingerprint: string;
      config: HybridSearchConfig;
      reranker_id: string | null;
      candidates: InspectorCandidate[];
      result_chunk_ids: string[];
      trace_sha256: string;
    };
  };
}

export interface PiiPreview {
  original_sha256: string;
  redacted_sha256: string;
  findings: Array<{
    kind: string;
    byte_start: number;
    byte_end: number;
    line: number;
    column: number;
    confidence_micros: number;
    masked_preview: string;
  }>;
  redacted_text: string;
}

export interface KnowledgeOcrConfig {
  enabled: boolean;
  executable_path: string | null;
  pdf_renderer_path: string | null;
  asset: {
    asset_id: string;
    sha256: string;
    engine: string;
    engine_version: string;
    languages: string[];
    license: string;
    provenance: string;
  } | null;
  languages: string[];
  low_confidence_micros: number;
}

export interface OcrInstallRequest {
  url: string;
  version: string;
  expected_sha256: string;
  size_bytes: number;
  license_name: string;
  license_url: string | null;
  provenance: string;
  languages: string[];
}

interface KnowledgeV2Store {
  sources: KnowledgeSourceV2[];
  progress: Record<string, KnowledgeRefreshProgress>;
  reports: Record<string, KnowledgeRefreshReport>;
  v1Imports: Record<string, KnowledgeV1ImportReport>;
  errors: Record<string, string>;
  loading: boolean;
  backgroundConfig: KnowledgeBackgroundRefreshConfig | null;
  refreshSources: (stackId?: string) => Promise<void>;
  addSource: (
    stackId: string,
    label: string,
    connector: KnowledgeConnector,
    webdavPassword?: string,
  ) => Promise<KnowledgeSourceV2>;
  updateSource: (
    sourceId: string,
    label: string,
    enabled: boolean,
    connector: KnowledgeConnector,
    webdavPassword?: string,
  ) => Promise<KnowledgeSourceV2>;
  removeSource: (sourceId: string) => Promise<void>;
  refreshStack: (stackId: string) => Promise<KnowledgeRefreshReport>;
  importFromV1: (stackId: string) => Promise<KnowledgeV1ImportReport>;
  cancelRefresh: (stackId: string) => Promise<boolean>;
  updateChunking: (stackId: string, chunkChars: number, chunkOverlap: number) => Promise<KnowledgeStack>;
  refreshBackgroundConfig: () => Promise<KnowledgeBackgroundRefreshConfig>;
  saveBackgroundConfig: (enabled: boolean, intervalMinutes: number, stackIds: string[]) => Promise<KnowledgeBackgroundRefreshConfig>;
  query: (
    stackId: string,
    query: string,
    config: HybridSearchConfig,
    excludedSourceIds: string[],
    rerank: boolean,
    tokenBudget: number,
    queryId?: string,
  ) => Promise<KnowledgeInspectorResponse>;
  cancelQuery: (queryId: string) => Promise<boolean>;
  piiPreview: (text: string) => Promise<PiiPreview>;
  ocrStatus: () => Promise<KnowledgeOcrConfig>;
  configureExternalOcr: (
    executablePath: string,
    pdfRendererPath: string | null,
    languages: string[],
    lowConfidenceMicros: number,
  ) => Promise<KnowledgeOcrConfig>;
  installOcr: (request: OcrInstallRequest) => Promise<KnowledgeOcrConfig>;
  setOcrEnabled: (enabled: boolean) => Promise<KnowledgeOcrConfig>;
}

function errorText(error: unknown): string {
  return errorMessage(error);
}

export const useKnowledgeV2Store = create<KnowledgeV2Store>((set, get) => ({
  sources: [],
  progress: {},
  reports: {},
  v1Imports: {},
  errors: {},
  loading: false,
  backgroundConfig: null,

  refreshSources: async (stackId) => {
    set({ loading: true });
    try {
      const sources = await invoke<KnowledgeSourceV2[]>("knowledge_v2_list_sources", {
        stackId: stackId ?? null,
      });
      set({ sources });
    } finally {
      set({ loading: false });
    }
  },

  addSource: async (stackId, label, connector, webdavPassword) => {
    const source = await invoke<KnowledgeSourceV2>("knowledge_v2_add_source", {
      stackId,
      label,
      connector,
      webdavPassword: webdavPassword || null,
    });
    set((state) => ({ sources: [...state.sources, source] }));
    return source;
  },

  updateSource: async (sourceId, label, enabled, connector, webdavPassword) => {
    const source = await invoke<KnowledgeSourceV2>("knowledge_v2_update_source", {
      sourceId,
      label,
      enabled,
      connector,
      webdavPassword: webdavPassword || null,
    });
    set((state) => ({
      sources: state.sources.map((existing) => (existing.id === source.id ? source : existing)),
    }));
    return source;
  },

  removeSource: async (sourceId) => {
    await invoke("knowledge_v2_remove_source", { sourceId });
    set((state) => ({ sources: state.sources.filter((source) => source.id !== sourceId) }));
  },

  refreshStack: async (stackId) => {
    set((state) => {
      const errors = { ...state.errors };
      delete errors[stackId];
      return { errors };
    });
    try {
      const report = await invoke<KnowledgeRefreshReport>("knowledge_v2_refresh", { stackId });
      set((state) => ({ reports: { ...state.reports, [stackId]: report } }));
      await Promise.all([get().refreshSources(), useStackStore.getState().refresh()]);
      return report;
    } catch (error) {
      const message = errorText(error);
      set((state) => ({ errors: { ...state.errors, [stackId]: message } }));
      throw error;
    }
  },

  // Same shape as `refreshStack`: errors land in `errors[stackId]` for the
  // panel's existing error slot, and both stores are re-read afterwards because
  // the import seeds v2 sources and updates the stack's legacy readiness badge.
  importFromV1: async (stackId) => {
    set((state) => {
      const errors = { ...state.errors };
      delete errors[stackId];
      return { errors };
    });
    try {
      const report = await invoke<KnowledgeV1ImportReport>("knowledge_v2_import_from_v1", { stackId });
      set((state) => ({ v1Imports: { ...state.v1Imports, [stackId]: report } }));
      await Promise.all([get().refreshSources(), useStackStore.getState().refresh()]);
      return report;
    } catch (error) {
      const message = errorText(error);
      set((state) => ({ errors: { ...state.errors, [stackId]: message } }));
      throw error;
    }
  },

  cancelRefresh: async (stackId) => invoke<boolean>("knowledge_v2_cancel_refresh", { stackId }),

  updateChunking: async (stackId, chunkChars, chunkOverlap) => {
    const stack = await invoke<KnowledgeStack>("knowledge_v2_update_chunking", {
      stackId,
      chunkChars,
      chunkOverlap,
    });
    await useStackStore.getState().refresh();
    return stack;
  },

  refreshBackgroundConfig: async () => {
    const backgroundConfig = await invoke<KnowledgeBackgroundRefreshConfig>("knowledge_v2_background_config_get");
    set({ backgroundConfig });
    return backgroundConfig;
  },

  saveBackgroundConfig: async (enabled, intervalMinutes, stackIds) => {
    const backgroundConfig = await invoke<KnowledgeBackgroundRefreshConfig>("knowledge_v2_background_config_save", {
      request: { enabled, intervalMinutes, stackIds },
    });
    set({ backgroundConfig });
    return backgroundConfig;
  },

  query: async (stackId, query, config, excludedSourceIds, rerank, tokenBudget, requestedQueryId) =>
    invoke<KnowledgeInspectorResponse>("knowledge_v2_query", {
      request: {
        stack_id: stackId,
        query_id: requestedQueryId ?? crypto.randomUUID(),
        query,
        config,
        excluded_source_ids: excludedSourceIds,
        rerank,
        token_budget: tokenBudget,
      },
    }),

  cancelQuery: async (queryId) => invoke<boolean>("knowledge_v2_cancel_query", { queryId }),

  piiPreview: async (text) => invoke<PiiPreview>("knowledge_v2_pii_preview", { text }),

  ocrStatus: async () => invoke<KnowledgeOcrConfig>("knowledge_ocr_status"),

  configureExternalOcr: async (executablePath, pdfRendererPath, languages, lowConfidenceMicros) =>
    invoke<KnowledgeOcrConfig>("knowledge_ocr_configure_external", {
      executablePath,
      pdfRendererPath,
      languages,
      lowConfidenceMicros,
    }),

  installOcr: async (request) => invoke<KnowledgeOcrConfig>("knowledge_ocr_install", { request }),

  setOcrEnabled: async (enabled) => invoke<KnowledgeOcrConfig>("knowledge_ocr_set_enabled", { enabled }),
}));

if (isTauri()) {
  void listen<KnowledgeRefreshProgress>("knowledge-v2://refresh-progress", (event) => {
    useKnowledgeV2Store.setState((state) => ({
      progress: { ...state.progress, [event.payload.stack_id]: event.payload },
    }));
  }).catch((error) => console.error("Failed to listen for Knowledge 2.0 progress", error));
}
