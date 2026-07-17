import { invoke } from "@tauri-apps/api/core";

/**
 * Thin typed wrapper around the `modelfile_*` Tauri commands backing
 * "Modelfile Studio" (Phase 8) — a real Ollama Modelfile parser, validator,
 * short-name guard, and GGUF/safetensors format sniffer living in
 * `src-tauri/src/modelfile.rs`. Every type below mirrors that module's
 * `#[serde(rename_all = "camelCase")]` structs exactly; field names/casing
 * must stay in sync with the Rust source of truth.
 *
 * The actual install step (`ollama create -f <modelfile>`) is a *separate*
 * command, `ollama::ollama_create_from_modelfile`, invoked through
 * `modelStore.createModelfileModel` rather than from here — that command
 * streams progress over the same `ollama://pull-progress` event the
 * existing pull/import flows already use, so it belongs next to those in
 * the store instead of duplicating that wiring in this module.
 */

export interface ModelfileParameter {
  key: string;
  value: string;
}

export interface ModelfileMessage {
  role: string;
  content: string;
}

/** Grammar-only parse result — no filesystem or semantic checks. See
 * `ModelfileDryRunReport` for those. */
export interface ParsedModelfile {
  from: string | null;
  requires: string | null;
  template: string | null;
  system: string | null;
  parameters: ModelfileParameter[];
  adapters: string[];
  licenses: string[];
  messages: ModelfileMessage[];
}

export type DetectedFormat =
  | "gguf"
  | "safetensorsFile"
  | "safetensorsDirectory"
  | "existingModelReference";

export interface SourceInspection {
  originalPath: string;
  sizeBytes: number;
  format: DetectedFormat;
  warnings: string[];
}

export interface ModelfileDryRunRequest {
  shortName: string;
  modelfileText: string;
}

/** Structured preview shown to the user before anything is installed into
 * the model library — the Phase 8 acceptance requirement this module
 * exists to satisfy. */
export interface ModelfileDryRunReport {
  shortName: string;
  from: string | null;
  source: SourceInspection | null;
  requires: string | null;
  templatePresent: boolean;
  systemPresent: boolean;
  parameters: ModelfileParameter[];
  licensePresent: boolean;
  licenses: string[];
  adapters: string[];
  messagesCount: number;
  warnings: string[];
}

export const modelfileClient = {
  /** Parses Modelfile text for live editor feedback — grammar only, no
   * filesystem access. Cheap enough to call on every keystroke debounce.
   * Rejects (throws the Rust-formatted `"line N: ..."` message) on the
   * first structural problem. */
  parse: (text: string) => invoke<ParsedModelfile>("modelfile_parse", { text }),

  /** Full preview/validate pipeline backing the "Preview & Validate" step:
   * short-name validation, grammar parse, semantic validation (parameter
   * types, `REQUIRES` semver shape, `ADAPTER` existence), and `FROM` source
   * inspection (GGUF/safetensors header sanity, or an existing-model
   * reference). Never touches the model library — this only ever reads. */
  dryRun: (request: ModelfileDryRunRequest) =>
    invoke<ModelfileDryRunReport>("modelfile_dry_run", { request }),

  /** Loads a small local text file's contents (e.g. a `LICENSE` or saved
   * `SYSTEM` prompt file) for insertion into the Modelfile Studio editor.
   * Bounded server-side to 2 MiB and rejects non-UTF-8 content. */
  readTextFile: (path: string) => invoke<string>("modelfile_read_text_file", { path }),
};
