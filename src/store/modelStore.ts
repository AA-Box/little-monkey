import { create } from "zustand";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { useUsageStore } from "./usageStore";

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
}

export type LlamaStatus = "stopped" | "starting" | "ready" | "error";

export interface DownloadProgress {
  downloaded: number;
  total: number;
}

/** Payload of the `llama://status` Tauri event emitted by src-tauri/src/llama.rs. */
interface LlamaStatusEvent {
  status: LlamaStatus;
  port: number;
  model_path: string | null;
}

/** Payload of the `models://download-progress` Tauri event emitted by src-tauri/src/models.rs. */
interface DownloadProgressEvent {
  file: string;
  downloaded: number;
  total: number;
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
 * Anthropic's `output_config.effort` levels (low/medium/high/xhigh/max) —
 * see `providers.rs::build_chat_request`, which only forwards this for
 * `provider_id === "anthropic"`. Every other provider ignores it.
 */
export type EffortLevel = "low" | "medium" | "high" | "xhigh" | "max";

const EFFORT_STORAGE_KEY = "little-monkey-effort";
const VALID_EFFORT_LEVELS: EffortLevel[] = ["low", "medium", "high", "xhigh", "max"];

function readInitialEffort(): EffortLevel {
  try {
    const stored = localStorage.getItem(EFFORT_STORAGE_KEY);
    if (stored && (VALID_EFFORT_LEVELS as string[]).includes(stored)) {
      return stored as EffortLevel;
    }
  } catch {
    // Best-effort; fall through to the default.
  }
  return "high";
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
}

/** Mirrors the Rust `ProviderModelInfo` struct exactly. */
export interface ProviderModelInfo {
  id: string;
}

/** Context window size used when starting llama-server. */
const DEFAULT_CTX_SIZE = 4096;
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
  /** Reload curated + installed model lists and sync llama-server status from the backend. */
  refresh: () => Promise<void>;
  /** Download a curated model's GGUF weights, then refresh the installed list. */
  download: (model: ModelInfo) => Promise<void>;
  /** Start llama-server on the given (installed) model. */
  start: (model: ModelInfo) => Promise<void>;
  /** Stop the running llama-server process. */
  stop: () => Promise<void>;
  /** Delete (app-downloaded weights) or unregister (external file) an installed model, then refresh. */
  removeModel: (model: ModelInfo) => Promise<void>;
  /** Register an arbitrary on-disk `.gguf` file (outside the app's models dir) as a usable local model. */
  addExternalModel: (path: string) => Promise<ModelInfo>;

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
  /** Import a local `.gguf` file or Safetensors model directory into Ollama under `name` (via `ollama create`), tracking progress/errors the same way as `pullOllamaModel`. */
  importOllamaModel: (name: string, path: string) => Promise<void>;
  /** Select an already-pulled Ollama tag as the active chat target. Instant, no backend call. */
  useOllamaModel: (tag: string) => void;
  /** Remove a locally-pulled Ollama tag, then refresh. */
  removeOllamaModel: (tag: string) => Promise<void>;
  /** Kick off `ollama signin`'s browser OAuth flow, capturing its initial output. */
  signinOllama: () => Promise<string>;

  // --- Cloud AI providers (OpenAI/Anthropic/Gemini/OpenRouter/custom) ---
  providers: ProviderConfig[];
  providerModels: Record<string, ProviderModelInfo[]>;
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

  /** Anthropic `output_config.effort` level, persisted to localStorage. Only meaningful/sent when chatting against the "anthropic" provider. */
  effort: EffortLevel;
  /** Update the effort level and persist it to localStorage. */
  setEffort: (effort: EffortLevel) => void;
}

export const useModelStore = create<ModelStore>((set, get) => ({
  curated: [],
  installed: [],
  active: null,
  downloadProgress: {},
  llamaStatus: "stopped",

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
        active: status.model_path
          ? installed.find((m) => m.path === status.model_path) ?? state.active
          : state.active,
      }));
    } catch (error) {
      console.error("Failed to fetch llama status", error);
    }
  },

  download: async (model) => {
    await invoke<string>("models_download", {
      repo: model.repo,
      file: model.file,
    });
    await get().refresh();
  },

  start: async (model) => {
    if (!model.path) {
      throw new Error(`Model "${model.name}" has not been downloaded yet`);
    }
    set({ active: model, llamaStatus: "starting", activeProvider: "local" });
    await invoke("llama_start", {
      modelPath: model.path,
      ctxSize: DEFAULT_CTX_SIZE,
      gpuLayers: DEFAULT_GPU_LAYERS,
    });
    // The context limit for a local model is exactly the ctx_size it was
    // started with.
    useUsageStore.getState().setContextLimit(DEFAULT_CTX_SIZE);
  },

  stop: async () => {
    await invoke("llama_stop");
    set({ llamaStatus: "stopped" });
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

  refreshOllama: async () => {
    const [status, models, exampleTags] = await Promise.all([
      invoke<OllamaStatusEvent>("ollama_status"),
      invoke<OllamaModelInfo[]>("ollama_list_models").catch(() => [] as OllamaModelInfo[]),
      invoke<string[]>("ollama_example_cloud_tags"),
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
      const message = err instanceof Error ? err.message : String(err);
      set((state) => ({
        ollamaPullError: { ...state.ollamaPullError, [tag]: message },
      }));
      throw err;
    }
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
      const message = err instanceof Error ? err.message : String(err);
      set((state) => ({
        ollamaPullError: { ...state.ollamaPullError, [name]: message },
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
      const { [id]: _discardError, ...restErrors } = state.providerKeyError;
      const stillActive = state.activeProviderId === id;
      return {
        providerModels: restModels,
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
      await get().refreshProviders();
    } catch (err) {
      const message = err instanceof Error ? err.message : String(err);
      set((state) => ({ providerKeyError: { ...state.providerKeyError, [id]: message } }));
      throw err;
    }
  },

  removeProviderKey: async (id) => {
    await invoke("providers_remove_key", { id });
    set((state) => {
      const { [id]: _discard, ...restModels } = state.providerModels;
      const stillActive = state.activeProviderId === id;
      return {
        providerModels: restModels,
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
    } catch (err) {
      const message = err instanceof Error ? err.message : String(err);
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
  },

  effort: readInitialEffort(),

  setEffort: (effort) => {
    set({ effort });
    try {
      localStorage.setItem(EFFORT_STORAGE_KEY, effort);
    } catch {
      // Best-effort persistence; a failure here shouldn't block the switch.
    }
  },
}));

void listen<LlamaStatusEvent>("llama://status", (event) => {
  useModelStore.setState((state) => ({
    llamaStatus: event.payload.status,
    active: event.payload.model_path
      ? state.installed.find((m) => m.path === event.payload.model_path) ??
        state.active
      : state.active,
  }));
}).catch((error) => {
  console.error("Failed to listen for llama://status events", error);
});

void listen<DownloadProgressEvent>("models://download-progress", (event) => {
  const { file, downloaded, total } = event.payload;
  useModelStore.setState((state) => ({
    downloadProgress: {
      ...state.downloadProgress,
      [file]: { downloaded, total },
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
