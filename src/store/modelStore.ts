import { create } from "zustand";
import { invoke, isTauri } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import {
  ANTHROPIC_EFFORT_FALLBACK_KEY,
  effortForProviderModel,
  ollamaModelTargetKey,
} from "../lib/modelTargets";
import { useUsageStore } from "./usageStore";
import { errorMessage } from "../lib/errors";

/**
 * Mirrors the Rust `ModelInfo` struct (src-tauri/src/models.rs) exactly —
 * field names/casing must match the serde JSON representation returned by
 * `models_list_curated` / `models_list_installed`.
 */
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
  /** "chat" (tool-calling instruct model) or "embedding" — see `models.rs::ModelKind`. Defaults to "chat" on the Rust side for pre-existing entries, so this is always present in practice. */
  kind: "chat" | "embedding";
  components?: ModelComponents;
  capabilities?: ModelCapabilities;
}

export type ComponentOwnership = "managed" | "external";

export interface ProjectorComponent {
  path: string;
  file: string;
  size_bytes: number;
  ownership: ComponentOwnership;
  sha256: string | null;
  missing?: boolean;
}

export interface ModelComponents {
  projector: ProjectorComponent | null;
}

export interface ProjectorCandidate {
  path: string;
  file: string;
  sizeBytes: number;
}

export interface ModelCapabilities {
  text: boolean;
  image_input: boolean;
}

/** Resolved metadata for a public GGUF model bundle reference. */
export interface ResolvedModelReference {
  source: string;
  canonicalReference: string;
  displayName: string;
  repo: string;
  revision: string;
  fileName: string;
  downloadUrl: string;
  sha256: string;
  sizeBytes: number;
  toolCalling: boolean;
  licenseName: string | null;
  licenseUrl: string | null;
  artifacts?: ResolvedModelArtifact[];
  projectorCandidates?: ResolvedModelArtifact[];
}

export type ModelArtifactRole = "model" | "projector";

export interface ResolvedModelArtifact {
  role: ModelArtifactRole;
  fileName: string;
  downloadUrl: string;
  sha256: string;
  sizeBytes: number;
}

export type LlamaStatus = "stopped" | "starting" | "ready" | "error";

export interface DownloadProgress {
  downloaded: number;
  total: number;
  component?: "model" | "projector";
  componentDownloaded?: number;
  componentTotal?: number;
}

/** Payload of the `llama://status` Tauri event emitted by src-tauri/src/llama.rs. */
interface LlamaStatusEvent {
  status: LlamaStatus;
  port: number;
  model_path: string | null;
  projector_path: string | null;
  vision_enabled: boolean;
}

/** localStorage key for the "start with embeddings" preference — mirrors
 * `EFFORT_BY_TARGET_STORAGE_KEY`'s persistence pattern below. */
const EMBEDDINGS_ENABLED_STORAGE_KEY = "little-monkey-llama-embeddings-enabled";

function readInitialEmbeddingsEnabled(): boolean {
  try {
    return localStorage.getItem(EMBEDDINGS_ENABLED_STORAGE_KEY) === "true";
  } catch {
    return false;
  }
}

/** Payload of the `models://download-progress` Tauri event emitted by src-tauri/src/models.rs. */
interface DownloadProgressEvent {
  file: string;
  reference?: string;
  component?: "model" | "projector";
  componentDownloaded?: number;
  componentTotal?: number;
  downloaded: number;
  total: number;
}

export function modelDownloadProgressEntries(
  event: DownloadProgressEvent,
): Record<string, DownloadProgress> {
  const progress = {
    downloaded: event.downloaded,
    total: event.total,
    ...(event.component ? { component: event.component } : {}),
    ...(event.componentDownloaded !== undefined ? { componentDownloaded: event.componentDownloaded } : {}),
    ...(event.componentTotal !== undefined ? { componentTotal: event.componentTotal } : {}),
  };
  return {
    [event.file]: progress,
    ...(event.reference ? { [event.reference]: progress } : {}),
  };
}

/**
 * Mirrors the Rust `OllamaModelInfo` struct (src-tauri/src/ollama.rs) exactly
 * — field names/casing must match the serde JSON representation returned by
 * `ollama_list_models`.
 */
export interface OllamaModelInfo {
  name: string;
  size_bytes: number;
  is_cloud: boolean;
  tool_calling: boolean;
  /** Best-effort signal from Ollama's own `/api/show` `capabilities` array — real, not heuristic. */
  vision: boolean;
  modified_at: string;
}

/** Payload of the `ollama://status` Tauri event / `ollama_status` return value. */
interface OllamaStatusEvent {
  reachable: boolean;
  version: string | null;
  binary_found: boolean;
}

/** Payload of the `ollama://pull-progress` Tauri event emitted by src-tauri/src/ollama.rs. */
interface OllamaPullProgressEvent {
  tag: string;
  line: string;
}

/** Which model provider is currently selected to chat against. */
export type ActiveProvider = "local" | "ollama" | "provider";

/**
 * The app's five-level effort scale — Anthropic's native
 * `output_config.effort` values. `providers.rs::build_chat_request` shapes
 * these per provider on the wire: verbatim for Anthropic, clamped onto the
 * three-level `reasoning_effort` scale for OpenAI/Gemini/OpenRouter, and
 * omitted entirely for custom providers.
 */
export type EffortLevel = "low" | "medium" | "high" | "xhigh" | "max";

/** Legacy single-global effort key (Anthropic-only control) — superseded by
 * the per-target map below; read once to migrate an existing choice. */
const LEGACY_EFFORT_STORAGE_KEY = "little-monkey-effort";
const EFFORT_BY_TARGET_STORAGE_KEY = "little-monkey-effort-by-target";
const VALID_EFFORT_LEVELS: EffortLevel[] = ["low", "medium", "high", "xhigh", "max"];

function isEffortLevel(value: unknown): value is EffortLevel {
  return typeof value === "string" && (VALID_EFFORT_LEVELS as string[]).includes(value);
}

function readInitialEffortByTarget(): Record<string, EffortLevel> {
  try {
    const raw = localStorage.getItem(EFFORT_BY_TARGET_STORAGE_KEY);
    if (raw !== null) {
      const parsed: unknown = JSON.parse(raw);
      const map: Record<string, EffortLevel> = {};
      if (parsed && typeof parsed === "object" && !Array.isArray(parsed)) {
        for (const [key, value] of Object.entries(parsed)) {
          if (isEffortLevel(value)) map[key] = value;
        }
      }
      return map;
    }
    // One-time migration from the legacy single-global control, which only
    // ever applied to Anthropic: a stored level becomes the Anthropic-wide
    // fallback entry (per-model keys can't be enumerated at hydration), so
    // an existing choice keeps affecting every Anthropic model until a
    // per-model level (or Default) is picked.
    const legacy = localStorage.getItem(LEGACY_EFFORT_STORAGE_KEY);
    if (isEffortLevel(legacy)) {
      const seeded: Record<string, EffortLevel> = { [ANTHROPIC_EFFORT_FALLBACK_KEY]: legacy };
      localStorage.setItem(EFFORT_BY_TARGET_STORAGE_KEY, JSON.stringify(seeded));
      return seeded;
    }
  } catch {
    // Best-effort; fall through to the empty default.
  }
  return {};
}

/**
 * Mirrors the Rust `ProviderConfig` struct (src-tauri/src/providers.rs)
 * exactly — a configured cloud AI provider (built-in preset or custom
 * OpenAI-compatible endpoint), with a live `has_key` keychain probe. Never
 * carries the key itself.
 */
export interface ProviderConfig {
  id: string;
  label: string;
  base_url: string;
  is_custom: boolean;
  has_key: boolean;
  /** A provider a sandboxed executable extension contributes. It reaches the
   * network from inside its own sandbox, through the origins it was granted,
   * so it has no base URL and no key here — its credentials live on the
   * extension's declared secret slots. */
  is_extension: boolean;
}

/** Mirrors the Rust `ProviderModelInfo` struct exactly. */
export interface ProviderModelInfo {
  id: string;
  /** The provider's own image-input answer, absent when its `/models` doesn't
   * carry one (the Rust side skips the field rather than sending `null`) — see
   * `lib/visionModels.ts`, which prefers this over its name-pattern guess. */
  vision?: boolean;
  /** The model's context window, when the provider publishes one — becomes
   * `usageStore`'s `contextLimit` in `useProviderModel` below. */
  context_length?: number;
  /** Whether the provider says the model accepts tools. OpenRouter answers;
   * nobody else does yet, so this is usually absent. */
  tool_calling?: boolean;
}

/**
 * Model Retirement and Compatibility Warnings (ROADMAP.md Phase 8, item 14):
 * mirrors the Rust `CloudModelRetirementWarning` struct
 * (`src-tauri/src/model_retirement.rs`) exactly. The retirement registry
 * itself is a maintained, versioned, local static list — not a live-verified
 * source, since there is no upstream API this app can call in this sandbox
 * to ask "is this model retired?". See that module's doc comment.
 */
export interface CloudModelRetirementWarning {
  provider_id: string;
  model_id: string;
  reason: string;
  suggested_replacement_model_id: string | null;
  replacement_note: string;
}

/** GPU layers to offload to the GPU; a large value offloads the full model. */
const DEFAULT_GPU_LAYERS = 999;

/**
 * Best-effort lookup of an Ollama tag's context length, straight from the
 * local daemon's own HTTP API (the same daemon chat requests already go to)
 * — not a Tauri `invoke`, just a plain `fetch`, since no secret is involved.
 * Ollama's `model_info` keys its context-length field with a
 * per-architecture prefix (e.g. "llama.context_length",
 * "qwen2.context_length") that varies per model, so every key is scanned for
 * one ending in ".context_length" rather than assuming an exact name. Wrapped
 * so it can never throw — a failure (daemon unreachable, unexpected shape,
 * etc.) just means the context limit falls back to "unknown" rather than
 * blocking or erroring the caller.
 */
async function fetchOllamaContextLimit(tag: string): Promise<void> {
  try {
    const response = await fetch("http://127.0.0.1:11434/api/show", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ model: tag }),
    });
    if (!response.ok) {
      useUsageStore.getState().setContextLimit(null);
      return;
    }
    const payload = (await response.json()) as { model_info?: Record<string, unknown> };
    const modelInfo = payload.model_info;
    let contextLimit: number | null = null;
    if (modelInfo && typeof modelInfo === "object") {
      for (const [key, value] of Object.entries(modelInfo)) {
        if (key.endsWith(".context_length") && typeof value === "number") {
          contextLimit = value;
          break;
        }
      }
    }
    useUsageStore.getState().setContextLimit(contextLimit);
  } catch {
    useUsageStore.getState().setContextLimit(null);
  }
}

export interface ModelStore {
  curated: ModelInfo[];
  installed: ModelInfo[];
  active: ModelInfo | null;
  downloadProgress: Record<string, DownloadProgress>;
  llamaStatus: LlamaStatus;
  llamaVisionEnabled: boolean;
  llamaProjectorPath: string | null;
  /** Why the last `start()` failed, verbatim from the backend. A generic
   * "llama-server failed to start" tells nobody whether the runtime is
   * unverified, the port is taken or the GGUF is corrupt — so the real message
   * is kept and rendered instead of being swallowed. */
  llamaError: string | null;
  /** Whether the next `start()` should launch llama-server with `--embeddings`
   * (surfaced as a checkbox in the Models panel — see `docs/roadmap/p1-local-api-server.md`
   * phase 3). Persisted to localStorage so the preference survives a restart,
   * same as `effortByTarget` below. Only takes effect on the *next* start — restarting
   * a model that's currently running is required to pick up a change, exactly
   * like every other llama-server launch flag (ctx size, gpu layers). */
  embeddingsEnabled: boolean;
  /** Update the "start with embeddings" preference and persist it. */
  setEmbeddingsEnabled: (value: boolean) => void;
  /** Reload curated + installed model lists and sync llama-server status from the backend. */
  refresh: () => Promise<void>;
  /** Download a curated model's GGUF weights, then refresh the installed list. */
  download: (model: ModelInfo) => Promise<void>;
  /** Cancel an in-flight curated download or public bundle install. */
  cancelDownload: (modelOrReference: ModelInfo | string) => Promise<void>;
  /** Resolve an Ollama tag or Hugging Face reference into a verified public GGUF artifact. */
  resolveModelReference: (reference: string) => Promise<ResolvedModelReference>;
  /** Install a previously-resolved artifact and refresh the installed model list. */
  installModelReference: (
    reference: string,
    expectedSha256: string,
    expectedProjectorSha256?: string,
  ) => Promise<ModelInfo>;
  /** Start llama-server on the given (installed) model. */
  start: (model: ModelInfo) => Promise<void>;
  /** Stop the running llama-server process. */
  stop: () => Promise<void>;
  /** Delete (app-downloaded weights) or unregister (external file) an installed model, then refresh. */
  removeModel: (model: ModelInfo) => Promise<void>;
  /** Register an arbitrary on-disk `.gguf` file (outside the app's models dir) as a usable local model. */
  addExternalModel: (path: string) => Promise<ModelInfo>;
  detectProjectors: (modelPath: string) => Promise<ProjectorCandidate[]>;
  setProjector: (modelPath: string, projectorPath: string) => Promise<ModelInfo>;
  removeProjector: (modelPath: string) => Promise<ModelInfo>;

  // --- Ollama (second, sibling model provider) ---
  ollamaReachable: boolean;
  ollamaVersion: string | null;
  ollamaBinaryFound: boolean;
  ollamaModels: OllamaModelInfo[];
  ollamaExampleTags: string[];
  /** Tag -> latest `ollama pull` progress line, for tags currently pulling. */
  ollamaPullProgress: Record<string, string>;
  /** Tag -> last failure message from a failed pull. */
  ollamaPullError: Record<string, string>;
  ollamaSigninMessage: string | null;
  /** Username parsed out of `ollamaSigninMessage` (e.g. "already signed in as user 'x'"), so the UI can show a persistent connected state instead of a stale "Sign in" button after the fact. `null` until `signinOllama` has been called at least once this session and matched that pattern. */
  ollamaSignedInUser: string | null;
  /** Which provider the chat should currently target. */
  activeProvider: ActiveProvider;
  /** The Ollama tag selected to chat with, when `activeProvider === "ollama"`. */
  activeOllamaModel: string | null;
  /** Refresh Ollama reachability, installed models, and example cloud tags. */
  refreshOllama: () => Promise<void>;
  /** Ensure Ollama's daemon is reachable (starting it if necessary), then refresh. */
  startOllama: () => Promise<void>;
  /** Pull a model tag via the Ollama CLI, tracking progress/errors for it. */
  pullOllamaModel: (tag: string) => Promise<void>;
  /** Cancel an in-flight `pullOllamaModel` pull for `tag` — kills the underlying `ollama pull` process. */
  cancelOllamaPull: (tag: string) => Promise<void>;
  /** Import a local `.gguf` file or Safetensors model directory into Ollama under `name` (via `ollama create`), tracking progress/errors the same way as `pullOllamaModel`. */
  importOllamaModel: (name: string, path: string) => Promise<void>;
  /** Create a model from a full, user-authored Modelfile via Modelfile Studio's hardened `ollama_create_from_modelfile` command — re-parses/re-validates `modelfileText` server-side regardless of any prior preview, then streams `ollama create` output the same way as `pullOllamaModel`/`importOllamaModel` (keyed by `shortName`). */
  createModelfileModel: (shortName: string, modelfileText: string) => Promise<void>;
  /** Select an already-pulled Ollama tag as the active chat target. Instant, no backend call. */
  useOllamaModel: (tag: string) => void;
  /** Remove a locally-pulled Ollama tag, then refresh. */
  removeOllamaModel: (tag: string) => Promise<void>;
  /** Kick off `ollama signin`'s browser OAuth flow, capturing its initial output. */
  signinOllama: () => Promise<string>;

  // --- Cloud AI providers (OpenAI/Anthropic/Gemini/OpenRouter/custom) ---
  providers: ProviderConfig[];
  providerModels: Record<string, ProviderModelInfo[]>;
  /** Model Retirement and Compatibility Warnings (ROADMAP.md Phase 8, item
   * 14): provider id -> model id -> retirement warning, computed once per
   * `providerModels` refresh (see `setProviderKey`/`refreshProviderModels`)
   * rather than on every render. A provider/model absent here simply hasn't
   * been checked yet or isn't retired — read via
   * `lib/modelRetirement.ts`'s `cloudModelRetirementWarning`. */
  providerModelRetirements: Record<string, Record<string, CloudModelRetirementWarning>>;
  /** Provider id -> last failure message from a failed `setProviderKey`/`refreshProviderModels` call. */
  providerKeyError: Record<string, string>;
  /** Which provider id is selected to chat with, when `activeProvider === "provider"`. */
  activeProviderId: string | null;
  /** Which of that provider's models is selected, when `activeProvider === "provider"`. */
  activeProviderModel: string | null;
  /** Provider id -> the last model id manually or automatically selected for it, so a failover/vision-switch candidate that revisits a provider can reuse the same model instead of always falling back to the first one in its list. */
  lastModelForProvider: Record<string, string>;
  /** Refresh the configured provider list (presets + custom) from the backend. */
  refreshProviders: () => Promise<void>;
  /** Register a new custom OpenAI-compatible provider (no key yet). */
  addCustomProvider: (label: string, baseUrl: string) => Promise<void>;
  /** Remove a custom provider's metadata and any saved key for it. */
  removeCustomProvider: (id: string) => Promise<void>;
  /** Validate + save a provider's API key, populating its model list on success. */
  setProviderKey: (id: string, apiKey: string) => Promise<void>;
  /** Remove a provider's saved key (and its cached model list). */
  removeProviderKey: (id: string) => Promise<void>;
  /** Re-fetch a provider's model list using its already-saved key. */
  refreshProviderModels: (id: string) => Promise<void>;
  /** Select an already-fetched provider model as the active chat target. Instant, no backend call. */
  useProviderModel: (providerId: string, modelId: string) => void;

  /** Per-model effort levels keyed by model-target key (see
   * `modelTargets.ts`'s `providerModelTargetKey`/`ollamaModelTargetKey`),
   * persisted to localStorage. A model with no entry sends no effort field
   * at all — the provider's own default applies (OpenAI hard-errors on
   * `reasoning_effort` for non-reasoning models, so "unset" must mean
   * "absent from the request", never a guessed level). */
  effortByTarget: Record<string, EffortLevel>;
  /** Set the effort level for one model target — or, with `null`, clear it
   * back to "Default" (nothing sent) — and persist the map. */
  setEffortForTarget: (targetKey: string, effort: EffortLevel | null) => void;
}

/**
 * Model Retirement and Compatibility Warnings (ROADMAP.md Phase 8, item 14):
 * checks a provider's whole fetched model list against the local retired-
 * model registry (`model_retirement.rs`) in one batched call, so switching
 * providers/models never needs a per-lookup round trip. Never throws — a
 * check failure is diagnostic and must never block the model list refresh
 * itself (mirrors `refreshCompatibilityReport`'s soft-fail shape in
 * `runtimeHubStore.ts`).
 */
async function fetchProviderModelRetirements(
  providerId: string,
  models: ProviderModelInfo[],
): Promise<Record<string, CloudModelRetirementWarning>> {
  if (!models.length) return {};
  try {
    const warnings = await invoke<CloudModelRetirementWarning[]>("providers_check_model_retirements", {
      providerId,
      modelIds: models.map((model) => model.id),
    });
    return Object.fromEntries(warnings.map((warning) => [warning.model_id, warning]));
  } catch {
    return {};
  }
}

export const useModelStore = create<ModelStore>((set, get) => ({
  curated: [],
  installed: [],
  active: null,
  downloadProgress: {},
  llamaStatus: "stopped",
  llamaVisionEnabled: false,
  llamaProjectorPath: null,
  llamaError: null,
  embeddingsEnabled: readInitialEmbeddingsEnabled(),

  setEmbeddingsEnabled: (value) => {
    set({ embeddingsEnabled: value });
    try {
      localStorage.setItem(EMBEDDINGS_ENABLED_STORAGE_KEY, String(value));
    } catch {
      // Best-effort persistence; a failure here shouldn't block the toggle.
    }
  },

  ollamaReachable: false,
  ollamaVersion: null,
  ollamaBinaryFound: false,
  ollamaModels: [],
  ollamaExampleTags: [],
  ollamaPullProgress: {},
  ollamaPullError: {},
  ollamaSigninMessage: null,
  ollamaSignedInUser: null,
  activeProvider: "local",
  activeOllamaModel: null,

  providers: [],
  providerModels: {},
  providerModelRetirements: {},
  providerKeyError: {},
  activeProviderId: null,
  activeProviderModel: null,
  lastModelForProvider: {},

  refresh: async () => {
    const [curated, installed] = await Promise.all([
      invoke<ModelInfo[]>("models_list_curated"),
      invoke<ModelInfo[]>("models_list_installed"),
    ]);
    set({ curated, installed });

    // Best-effort sync of the live llama-server status, so the UI reflects
    // reality (e.g. after a fresh app launch) without requiring a separate
    // call. Failures here shouldn't blow up the model list refresh.
    try {
      const status = await invoke<LlamaStatusEvent>("llama_status");
      set((state) => ({
        llamaStatus: status.status,
        llamaVisionEnabled: status.vision_enabled === true,
        llamaProjectorPath: status.projector_path ?? null,
        active: status.model_path
          ? installed.find((m) => m.path === status.model_path) ?? state.active
          : state.active,
      }));
    } catch (error) {
      console.error("Failed to fetch llama status", error);
    }
  },

  download: async (model) => {
    try {
      await invoke<string>("models_download", {
        repo: model.repo,
        file: model.file,
      });
    } catch (error) {
      // A cancelled or failed download must not leave a stale progress
      // entry behind — `isDownloading` in ModelCard keys off its presence,
      // so without this the card would be stuck showing a progress bar
      // (with no Pull button to retry) forever.
      set((state) => {
        const { [model.file]: _removed, ...rest } = state.downloadProgress;
        return { downloadProgress: rest };
      });
      throw error;
    }
    await get().refresh();

    // Pulling a model is a request to use it, so don't make the user click
    // Start too. Skipped when a model is already loaded: `llama_start` kills
    // the running process, and nothing about a background pull should yank a
    // live chat's model out from under it. `start` records its own failure in
    // `llamaError`, and the pull itself succeeded, so don't reject here.
    if (get().llamaStatus === "stopped") {
      const pulled = get().installed.find((entry) => entry.file === model.file);
      if (pulled?.path) {
        try {
          await get().start(pulled);
        } catch {
          // Already surfaced on the card via `llamaError`.
        }
      }
    }
  },

  cancelDownload: async (modelOrReference) => {
    const file = typeof modelOrReference === "string" ? modelOrReference : modelOrReference.file;
    await invoke("models_cancel_download", { file });
  },

  resolveModelReference: async (reference) =>
    invoke<ResolvedModelReference>("models_resolve_reference", { reference }),

  installModelReference: async (reference, expectedSha256, expectedProjectorSha256) => {
    const args: Record<string, unknown> = { reference, expectedSha256 };
    if (expectedProjectorSha256) args.expectedProjectorSha256 = expectedProjectorSha256;
    const model = await invoke<ModelInfo>("models_install_reference", args);
    await get().refresh();
    return model;
  },

  start: async (model) => {
    if (!model.path) {
      throw new Error(`Model "${model.name}" has not been downloaded yet`);
    }
    set({
      active: model,
      llamaStatus: "starting",
      llamaVisionEnabled: false,
      llamaProjectorPath: null,
      llamaError: null,
      activeProvider: "local",
    });
    let resolvedCtxSize: number;
    try {
      // `ctxSize` is omitted so the backend auto-sizes the context window
      // from the model's own GGUF metadata (`llama.rs::resolve_ctx_size`)
      // instead of one fixed guess for every model — it returns whatever it
      // actually launched with.
      const startArgs: Record<string, unknown> = {
        modelPath: model.path,
        gpuLayers: DEFAULT_GPU_LAYERS,
      };
      const projector = model.components?.projector;
      if (projector?.missing) {
        throw new Error("The multimodal projector configured for this model no longer exists.");
      }
      if (projector) {
        startArgs.projectorPath = projector.path;
        startArgs.embeddings = false;
      } else {
        startArgs.embeddings = get().embeddingsEnabled;
      }
      resolvedCtxSize = await invoke<number>("llama_start", startArgs);
    } catch (err) {
      // `llama_start` can reject before ever spawning the process (e.g.
      // model verification or runtime binary resolution failure) — those
      // paths never emit a `llama://status` event, so without this the
      // optimistic "starting" set above would never be corrected and the
      // UI would be stuck showing "Starting..." indefinitely.
      set({ llamaStatus: "error", llamaError: errorMessage(err) });
      throw err;
    }
    // The context limit for a local model is exactly the ctx_size it was
    // started with.
    useUsageStore.getState().setContextLimit(resolvedCtxSize);
  },

  stop: async () => {
    await invoke("llama_stop");
    set({ llamaStatus: "stopped", llamaError: null, llamaVisionEnabled: false, llamaProjectorPath: null });
  },

  removeModel: async (model) => {
    if (!model.path) return;
    if (model.is_external) {
      await invoke("models_remove_external", { id: model.id });
    } else {
      await invoke("models_delete", { path: model.path });
    }
    await get().refresh();
  },

  addExternalModel: async (path) => {
    const model = await invoke<ModelInfo>("models_add_external", { path });
    await get().refresh();
    return model;
  },

  detectProjectors: (modelPath) =>
    invoke<ProjectorCandidate[]>("models_detect_projectors", { modelPath }),

  setProjector: async (modelPath, projectorPath) => {
    const model = await invoke<ModelInfo>("models_set_projector", { modelPath, projectorPath });
    await get().refresh();
    return model;
  },

  removeProjector: async (modelPath) => {
    const model = await invoke<ModelInfo>("models_remove_projector", { modelPath });
    await get().refresh();
    return model;
  },

  refreshOllama: async () => {
    const [status, models, exampleTags] = await Promise.all([
      invoke<OllamaStatusEvent>("ollama_status"),
      invoke<OllamaModelInfo[]>("ollama_list_models").catch(() => [] as OllamaModelInfo[]),
      // The local tag inventory is still valid when the optional cloud
      // catalog is unavailable. Do not discard usable local Ollama/MLX
      // models because that separate catalog request failed.
      invoke<string[]>("ollama_example_cloud_tags").catch(() => [] as string[]),
    ]);
    set({
      ollamaReachable: status.reachable,
      ollamaVersion: status.version,
      ollamaBinaryFound: status.binary_found,
      ollamaModels: models,
      ollamaExampleTags: exampleTags,
    });
  },

  startOllama: async () => {
    await invoke("ollama_start");
    await get().refreshOllama();
  },

  pullOllamaModel: async (tag) => {
    set((state) => {
      const { [tag]: _discard, ...rest } = state.ollamaPullError;
      return { ollamaPullError: rest };
    });
    try {
      await invoke("ollama_pull_model", { tag });
      set((state) => {
        const { [tag]: _discard, ...rest } = state.ollamaPullProgress;
        return { ollamaPullProgress: rest };
      });
      await get().refreshOllama();
    } catch (err) {
      const message = errorMessage(err);
      set((state) => ({
        ollamaPullError: { ...state.ollamaPullError, [tag]: message },
      }));
      throw err;
    }
  },

  cancelOllamaPull: async (tag) => {
    await invoke("ollama_cancel_pull", { tag });
  },

  importOllamaModel: async (name, path) => {
    set((state) => {
      const { [name]: _discard, ...rest } = state.ollamaPullError;
      return { ollamaPullError: rest };
    });
    try {
      await invoke("ollama_import_model", { name, path });
      set((state) => {
        const { [name]: _discard, ...rest } = state.ollamaPullProgress;
        return { ollamaPullProgress: rest };
      });
      await get().refreshOllama();
    } catch (err) {
      const message = errorMessage(err);
      set((state) => ({
        ollamaPullError: { ...state.ollamaPullError, [name]: message },
      }));
      throw err;
    }
  },

  createModelfileModel: async (shortName, modelfileText) => {
    set((state) => {
      const { [shortName]: _discard, ...rest } = state.ollamaPullError;
      return { ollamaPullError: rest };
    });
    try {
      await invoke("ollama_create_from_modelfile", { shortName, modelfileText });
      set((state) => {
        const { [shortName]: _discard, ...rest } = state.ollamaPullProgress;
        return { ollamaPullProgress: rest };
      });
      await get().refreshOllama();
    } catch (err) {
      const message = errorMessage(err);
      set((state) => ({
        ollamaPullError: { ...state.ollamaPullError, [shortName]: message },
      }));
      throw err;
    }
  },

  useOllamaModel: (tag) => {
    set({ activeProvider: "ollama", activeOllamaModel: tag });
    // Best-effort, fire-and-forget lookup of this tag's context length so
    // `ContextUsageIndicator` can show a real percentage instead of falling
    // back to a raw token count — never awaited here, so this action stays
    // instant/synchronous-feeling.
    void fetchOllamaContextLimit(tag);
  },

  removeOllamaModel: async (tag) => {
    await invoke("ollama_remove_model", { tag });
    set((state) =>
      state.activeOllamaModel === tag
        ? { activeProvider: "local" as const, activeOllamaModel: null }
        : {},
    );
    await get().refreshOllama();
  },

  signinOllama: async () => {
    const message = await invoke<string>("ollama_signin");
    // `ollama signin` prints "You are already signed in as user 'x'" (no
    // browser opens) when a valid session already exists, or the analogous
    // "Signed in as user 'x'" once a fresh OAuth flow completes — pull the
    // username out of either so the UI can show a persistent connected
    // state instead of leaving a stale "Sign in" button after the fact.
    const match = message.match(/signed in as user ['"]([^'"]+)['"]/i);
    set({ ollamaSigninMessage: message, ollamaSignedInUser: match ? match[1] : null });
    return message;
  },

  refreshProviders: async () => {
    const providers = await invoke<ProviderConfig[]>("providers_list_configured");
    set({ providers });

    // Keys persist in the OS keychain across restarts, but `providerModels`
    // is in-memory only — re-hydrate any already-connected provider's model
    // list that this session hasn't fetched yet, so the sidebar's
    // "Cloud Models" list isn't empty just because the app was relaunched.
    const { providerModels } = get();
    for (const provider of providers) {
      if (provider.has_key && !providerModels[provider.id]) {
        void get().refreshProviderModels(provider.id);
      }
    }
  },

  addCustomProvider: async (label, baseUrl) => {
    await invoke<ProviderConfig>("providers_add_custom", { label, baseUrl });
    await get().refreshProviders();
  },

  removeCustomProvider: async (id) => {
    await invoke("providers_remove_custom", { id });
    set((state) => {
      const { [id]: _discardModels, ...restModels } = state.providerModels;
      const { [id]: _discardRetirements, ...restRetirements } = state.providerModelRetirements;
      const { [id]: _discardError, ...restErrors } = state.providerKeyError;
      const stillActive = state.activeProviderId === id;
      return {
        providerModels: restModels,
        providerModelRetirements: restRetirements,
        providerKeyError: restErrors,
        ...(stillActive
          ? { activeProvider: "local" as const, activeProviderId: null, activeProviderModel: null }
          : {}),
      };
    });
    await get().refreshProviders();
  },

  setProviderKey: async (id, apiKey) => {
    set((state) => {
      const { [id]: _discard, ...rest } = state.providerKeyError;
      return { providerKeyError: rest };
    });
    try {
      const models = await invoke<ProviderModelInfo[]>("providers_set_key", { id, apiKey });
      set((state) => ({ providerModels: { ...state.providerModels, [id]: models } }));
      const retirements = await fetchProviderModelRetirements(id, models);
      set((state) => ({ providerModelRetirements: { ...state.providerModelRetirements, [id]: retirements } }));
      await get().refreshProviders();
    } catch (err) {
      const message = errorMessage(err);
      set((state) => ({ providerKeyError: { ...state.providerKeyError, [id]: message } }));
      throw err;
    }
  },

  removeProviderKey: async (id) => {
    await invoke("providers_remove_key", { id });
    set((state) => {
      const { [id]: _discard, ...restModels } = state.providerModels;
      const { [id]: _discardRetirements, ...restRetirements } = state.providerModelRetirements;
      const stillActive = state.activeProviderId === id;
      return {
        providerModels: restModels,
        providerModelRetirements: restRetirements,
        ...(stillActive
          ? { activeProvider: "local" as const, activeProviderId: null, activeProviderModel: null }
          : {}),
      };
    });
    await get().refreshProviders();
  },

  refreshProviderModels: async (id) => {
    set((state) => {
      const { [id]: _discard, ...rest } = state.providerKeyError;
      return { providerKeyError: rest };
    });
    try {
      const models = await invoke<ProviderModelInfo[]>("providers_list_models", { id });
      set((state) => ({ providerModels: { ...state.providerModels, [id]: models } }));
      const retirements = await fetchProviderModelRetirements(id, models);
      set((state) => ({ providerModelRetirements: { ...state.providerModelRetirements, [id]: retirements } }));
    } catch (err) {
      const message = errorMessage(err);
      set((state) => ({ providerKeyError: { ...state.providerKeyError, [id]: message } }));
      throw err;
    }
  },

  useProviderModel: (providerId, modelId) => {
    set((state) => ({
      activeProvider: "provider",
      activeProviderId: providerId,
      activeProviderModel: modelId,
      lastModelForProvider: { ...state.lastModelForProvider, [providerId]: modelId },
    }));
    // Already in hand from the last `/models` refresh, so unlike the Ollama
    // path this needs no lookup. A provider that publishes no context window
    // leaves this null, which is what `contextTrimmer.ts` reads as "no budget
    // to aim for" — the same state every cloud model was stuck in before.
    const contextLength = get().providerModels[providerId]?.find((model) => model.id === modelId)
      ?.context_length;
    useUsageStore.getState().setContextLimit(contextLength ?? null);
  },

  effortByTarget: readInitialEffortByTarget(),

  setEffortForTarget: (targetKey, effort) => {
    const next = { ...get().effortByTarget };
    if (effort === null) {
      delete next[targetKey];
      // Choosing "Default" for an Anthropic model also retires the migrated
      // legacy fallback (which only ever exists via the one-time migration
      // of the old single-global control) — without this, the fallback would
      // immediately re-apply and "Default" would be unreachable. The old
      // control was global across Anthropic models anyway, so retiring it
      // globally on the first explicit Default matches its own semantics.
      if (targetKey.startsWith(`${ANTHROPIC_EFFORT_FALLBACK_KEY}:`)) {
        delete next[ANTHROPIC_EFFORT_FALLBACK_KEY];
      }
    } else {
      next[targetKey] = effort;
    }
    set({ effortByTarget: next });
    try {
      localStorage.setItem(EFFORT_BY_TARGET_STORAGE_KEY, JSON.stringify(next));
    } catch {
      // Best-effort persistence; a failure here shouldn't block the switch.
    }
  },
}));

// These backend events only exist under the Tauri shell — in plain-browser
// dev (`vite` without it) `listen` itself throws, so don't subscribe at all.
if (isTauri()) {
  void listen<LlamaStatusEvent>("llama://status", (event) => {
    useModelStore.setState((state) => ({
      llamaStatus: event.payload.status,
      llamaVisionEnabled: event.payload.vision_enabled === true,
      llamaProjectorPath: event.payload.projector_path ?? null,
      active: event.payload.model_path
        ? state.installed.find((m) => m.path === event.payload.model_path) ??
          state.active
        : state.active,
    }));
  }).catch((error) => {
    console.error("Failed to listen for llama://status events", error);
  });

  void listen<DownloadProgressEvent>("models://download-progress", (event) => {
    useModelStore.setState((state) => ({
      downloadProgress: {
        ...state.downloadProgress,
        ...modelDownloadProgressEntries(event.payload),
      },
    }));
  }).catch((error) => {
    console.error("Failed to listen for models://download-progress events", error);
  });

  void listen<OllamaStatusEvent>("ollama://status", (event) => {
    useModelStore.setState({
      ollamaReachable: event.payload.reachable,
      ollamaVersion: event.payload.version,
      ollamaBinaryFound: event.payload.binary_found,
    });
  }).catch((error) => {
    console.error("Failed to listen for ollama://status events", error);
  });

  void listen<OllamaPullProgressEvent>("ollama://pull-progress", (event) => {
    const { tag, line } = event.payload;
    useModelStore.setState((state) => ({
      ollamaPullProgress: {
        ...state.ollamaPullProgress,
        [tag]: line,
      },
    }));
  }).catch((error) => {
    console.error("Failed to listen for ollama://pull-progress events", error);
  });
}

/**
 * Fast, synchronous resolution of what `agentLoop.ts` should chat against
 * right now, without duplicating any store-reading logic there:
 *  - `"local"` — the local llama.cpp provider; `agentLoop.ts` keeps its own
 *    existing `llama_status`-based port resolution for this case.
 *  - `"ollama"` — chat directly against the local Ollama daemon (no secret
 *    involved, so a plain `fetch` via `streamChat` is fine). `model` may be
 *    `null` if no tag has been selected yet — callers should treat that as
 *    a pre-flight error rather than silently falling back to local.
 *  - `"provider"` — a configured cloud AI provider (OpenAI/Anthropic/
 *    Gemini/OpenRouter/custom); its API key lives in the OS keychain, so
 *    chat must go through the Rust-proxied `streamProviderChat` instead of
 *    a direct `fetch`. `providerId`/`model` may be `null` if none is
 *    selected yet — same pre-flight-error treatment.
 */
export type ChatTarget =
  | { kind: "local" }
  | { kind: "ollama"; baseUrl: string; model: string | null }
  | { kind: "provider"; providerId: string | null; model: string | null };

export function getActiveChatTarget(): ChatTarget {
  const { activeProvider, activeOllamaModel, activeProviderId, activeProviderModel } = useModelStore.getState();
  if (activeProvider === "ollama") {
    return { kind: "ollama", baseUrl: "http://127.0.0.1:11434", model: activeOllamaModel };
  }
  if (activeProvider === "provider") {
    return { kind: "provider", providerId: activeProviderId, model: activeProviderModel };
  }
  return { kind: "local" };
}

/**
 * Resolves the effort level to send for a chat target from the per-model
 * `effortByTarget` map — structurally accepts both `ChatTarget` above and
 * `turnEngine.ts`'s `ResolvedTarget`. `undefined` means the user left this
 * model on "Default": no effort field is sent at all and the provider's own
 * default applies (the Rust proxy additionally owns the per-provider wire
 * mapping/omission — see `providers.rs::build_chat_request`).
 */
export function effortForTarget(
  target:
    | { kind: "local" }
    | { kind: "ollama"; model: string | null }
    | { kind: "provider"; providerId: string | null; model: string | null },
): EffortLevel | undefined {
  const { effortByTarget } = useModelStore.getState();
  if (target.kind === "provider") {
    if (!target.providerId || !target.model) return undefined;
    return effortForProviderModel(effortByTarget, target.providerId, target.model);
  }
  if (target.kind === "ollama" && target.model) {
    return effortByTarget[ollamaModelTargetKey(target.model)];
  }
  return undefined;
}
