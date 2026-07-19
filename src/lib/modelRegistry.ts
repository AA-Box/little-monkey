/**
 * Frontend copy of the curated model registry.
 *
 * This mirrors `src-tauri/src/models.rs::CURATED_MODELS` field-for-field so
 * the UI can render the catalog instantly (before the Rust side has even
 * answered `models_list_curated`) and so `ModelInfo` values coming back over
 * `invoke()` deserialize into exactly this shape. Keep both lists in sync.
 */

/** Mirrors the Rust `ModelInfo` struct (src-tauri/src/models.rs) 1:1. */
export interface ModelInfo {
  id: string;
  name: string;
  repo: string;
  file: string;
  size_gb: number;
  tool_calling: boolean;
  installed: boolean;
  path: string | null;
  /** True for a model registered via `models_add_external` (a `.gguf` file outside the app's models dir) — the app never owns or deletes that file. */
  is_external: boolean;
  /** "chat" or "embedding" — see `models.rs::ModelKind`. Every entry in this frontend copy is a chat model; the two curated embedding models (nomic-embed-text-v1.5, bge-m3) are fetched live from the backend instead (see `stackStore.ts`), not duplicated here. */
  kind: "chat" | "embedding";
}

/**
 * The five curated GGUF models Little Monkey knows how to fetch from Hugging Face.
 * `installed`/`path` are always `false`/`null` here — actual install state
 * comes from the Rust-side `models_list_installed` scan and is merged in by
 * the model store / `ModelManager`.
 */
export const CURATED_MODELS: ModelInfo[] = [
  {
    id: "qwen2.5-7b",
    name: "Qwen2.5 7B Instruct",
    repo: "Qwen/Qwen2.5-7B-Instruct-GGUF",
    file: "qwen2.5-7b-instruct-q4_k_m.gguf",
    size_gb: 4.7,
    tool_calling: true,
    installed: false,
    path: null,
    is_external: false,
    kind: "chat",
  },
  {
    id: "qwen2.5-coder-14b",
    name: "Qwen2.5 Coder 14B Instruct",
    repo: "Qwen/Qwen2.5-Coder-14B-Instruct-GGUF",
    file: "qwen2.5-coder-14b-instruct-q4_k_m.gguf",
    size_gb: 9.0,
    tool_calling: true,
    installed: false,
    path: null,
    is_external: false,
    kind: "chat",
  },
  {
    id: "llama-3.1-8b",
    name: "Llama 3.1 8B Instruct",
    repo: "bartowski/Meta-Llama-3.1-8B-Instruct-GGUF",
    file: "Meta-Llama-3.1-8B-Instruct-Q4_K_M.gguf",
    size_gb: 4.9,
    tool_calling: true,
    installed: false,
    path: null,
    is_external: false,
    kind: "chat",
  },
  {
    id: "hermes-3-8b",
    name: "Hermes 3 Llama 3.1 8B",
    repo: "NousResearch/Hermes-3-Llama-3.1-8B-GGUF",
    file: "Hermes-3-Llama-3.1-8B.Q4_K_M.gguf",
    size_gb: 4.9,
    tool_calling: true,
    installed: false,
    path: null,
    is_external: false,
    kind: "chat",
  },
  {
    id: "mistral-nemo-12b",
    name: "Mistral Nemo 2407 Instruct",
    repo: "bartowski/Mistral-Nemo-Instruct-2407-GGUF",
    file: "Mistral-Nemo-Instruct-2407-Q4_K_M.gguf",
    size_gb: 7.5,
    tool_calling: true,
    installed: false,
    path: null,
    is_external: false,
    kind: "chat",
  },
];

/** Look up a curated model by its stable id (e.g. "qwen2.5-7b"). */
export function findCuratedModel(id: string): ModelInfo | undefined {
  return CURATED_MODELS.find((model) => model.id === id);
}

/** Look up a curated model by its GGUF filename, as returned by the backend's directory scan. */
export function findCuratedModelByFile(file: string): ModelInfo | undefined {
  return CURATED_MODELS.find((model) => model.file === file);
}

/** Format a byte count as a human-readable string (e.g. "2.3 GB", "512 MB"). */
export function formatBytes(bytes: number): string {
  if (!Number.isFinite(bytes) || bytes <= 0) return "0 B";
  const units = ["B", "KB", "MB", "GB", "TB"];
  const exponent = Math.min(
    Math.floor(Math.log(bytes) / Math.log(1024)),
    units.length - 1,
  );
  const value = bytes / 1024 ** exponent;
  const decimals = exponent === 0 ? 0 : value >= 10 ? 1 : 2;
  return `${value.toFixed(decimals)} ${units[exponent]}`;
}

/** Format a model's on-disk size in GB (e.g. 4.7 -> "4.7 GB"). */
export function formatSizeGb(sizeGb: number): string {
  return `${sizeGb.toFixed(1)} GB`;
}
