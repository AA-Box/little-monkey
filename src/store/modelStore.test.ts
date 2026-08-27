import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

const invokeMock = vi.fn();
// See `apiServerStore.test.ts`'s comment on why the `listen` handler must be
// stashed via `vi.hoisted` rather than a plain outer-scope variable — a
// normal `let`/`var` closed over by a hoisted `vi.mock` factory is a
// *different* binding than the one this file's test bodies read later.
vi.mock("@tauri-apps/api/core", () => ({ invoke: (...args: unknown[]) => invokeMock(...args), isTauri: () => true }));
vi.mock("@tauri-apps/api/event", () => ({
  listen: () => Promise.resolve(() => {}),
}));

import {
  ANTHROPIC_EFFORT_FALLBACK_KEY,
  ollamaModelTargetKey,
  providerModelTargetKey,
} from "../lib/modelTargets";
import {
  modelDownloadProgressEntries,
  useModelStore,
  type ModelInfo,
  type ResolvedModelReference,
} from "./modelStore";

const EMBEDDINGS_ENABLED_STORAGE_KEY = "little-monkey-llama-embeddings-enabled";

function makeModel(overrides: Partial<ModelInfo> = {}): ModelInfo {
  return {
    id: "qwen2.5-7b-instruct",
    name: "Qwen 2.5 7B Instruct",
    repo: "example/repo",
    file: "qwen2.5-7b-instruct.gguf",
    size_gb: 4.2,
    tool_calling: true,
    installed: true,
    path: "/models/qwen2.5-7b-instruct.gguf",
    is_external: false,
    kind: "chat",
    ...overrides,
  };
}

beforeEach(() => {
  invokeMock.mockReset();
  // Guarded rather than assumed: this suite runs under vitest's `node`
  // environment (see `vitest.config.ts`), which has no `localStorage`
  // global at all — same stance `settingsStore.test.ts` takes.
  if (typeof localStorage !== "undefined") {
    localStorage.removeItem(EMBEDDINGS_ENABLED_STORAGE_KEY);
  }
  useModelStore.setState({ embeddingsEnabled: false, active: null, llamaStatus: "stopped" });
});

describe("modelStore.embeddingsEnabled", () => {
  it("defaults to false", () => {
    expect(useModelStore.getState().embeddingsEnabled).toBe(false);
  });

  it("setEmbeddingsEnabled updates state and persists to localStorage", () => {
    if (typeof localStorage === "undefined") return;
    useModelStore.getState().setEmbeddingsEnabled(true);
    expect(useModelStore.getState().embeddingsEnabled).toBe(true);
    expect(localStorage.getItem(EMBEDDINGS_ENABLED_STORAGE_KEY)).toBe("true");

    useModelStore.getState().setEmbeddingsEnabled(false);
    expect(useModelStore.getState().embeddingsEnabled).toBe(false);
    expect(localStorage.getItem(EMBEDDINGS_ENABLED_STORAGE_KEY)).toBe("false");
  });
});

describe("modelStore.start", () => {
  it("passes the current embeddingsEnabled preference to llama_start, leaving ctx sizing to the backend", async () => {
    useModelStore.getState().setEmbeddingsEnabled(true);
    invokeMock.mockResolvedValueOnce(8_192);

    await useModelStore.getState().start(makeModel());

    expect(invokeMock).toHaveBeenCalledWith("llama_start", {
      modelPath: "/models/qwen2.5-7b-instruct.gguf",
      gpuLayers: 999,
      embeddings: true,
    });
  });

  it("passes embeddings: false when the preference is off", async () => {
    invokeMock.mockResolvedValueOnce(4_096);

    await useModelStore.getState().start(makeModel());

    expect(invokeMock).toHaveBeenCalledWith(
      "llama_start",
      expect.objectContaining({ embeddings: false }),
    );
  });

  it("passes an attached projector to the local launcher", async () => {
    useModelStore.getState().setEmbeddingsEnabled(true);
    invokeMock.mockResolvedValueOnce(8_192);
    await useModelStore.getState().start(
      makeModel({
        components: {
          projector: {
            path: "/models/components/mmproj.gguf",
            file: "mmproj.gguf",
            size_bytes: 1_024,
            ownership: "managed",
            sha256: "a".repeat(64),
            missing: false,
          },
        },
      }),
    );
    expect(invokeMock).toHaveBeenCalledWith(
      "llama_start",
      expect.objectContaining({
        projectorPath: "/models/components/mmproj.gguf",
        embeddings: false,
      }),
    );
  });

  it("refuses to start when the associated projector is missing", async () => {
    await expect(useModelStore.getState().start(
      makeModel({
        components: {
          projector: {
            path: "/models/components/mmproj.gguf",
            file: "mmproj.gguf",
            size_bytes: 1_024,
            ownership: "external",
            sha256: null,
            missing: true,
          },
        },
      }),
    )).rejects.toThrow(/projector.*no longer exists/i);
    expect(invokeMock).not.toHaveBeenCalled();
  });

  it("sets the usage-store context limit to whatever llama_start actually resolved, not a fixed guess", async () => {
    invokeMock.mockResolvedValueOnce(16_384);

    await useModelStore.getState().start(makeModel());

    const { useUsageStore } = await import("./usageStore");
    expect(useUsageStore.getState().contextLimit).toBe(16_384);
  });

  it("throws for a model with no path, without calling llama_start", async () => {
    await expect(useModelStore.getState().start(makeModel({ path: null }))).rejects.toThrow(
      /has not been downloaded yet/,
    );
    expect(invokeMock).not.toHaveBeenCalled();
  });
});

/** Without a context limit, `contextTrimmer.ts`'s `shouldTrim` returns false
 *  for any history, so a cloud model never auto-compacts and the context ring
 *  has no denominator. The provider's `/models` already carried the number. */
describe("modelStore.useProviderModel", () => {
  it("adopts the provider-reported context window, and clears it when there isn't one", async () => {
    const { useUsageStore } = await import("./usageStore");
    useModelStore.setState({
      providerModels: {
        openrouter: [{ id: "vendor/big", context_length: 1_000_000 }, { id: "vendor/silent" }],
      },
    });

    useModelStore.getState().useProviderModel("openrouter", "vendor/big");
    expect(useUsageStore.getState().contextLimit).toBe(1_000_000);

    useModelStore.getState().useProviderModel("openrouter", "vendor/silent");
    expect(useUsageStore.getState().contextLimit).toBeNull();
  });
});

describe("modelStore model reference install", () => {
  const resolved: ResolvedModelReference = {
    source: "ollama",
    canonicalReference: "hf.co/library/llama3.2-GGUF:Q4_K_M",
    displayName: "Llama 3.2 3B",
    repo: "library/llama3.2-GGUF",
    revision: "main",
    fileName: "llama3.2-3b-q4_k_m.gguf",
    downloadUrl: "https://huggingface.co/library/llama3.2-GGUF/resolve/main/llama3.2-3b-q4_k_m.gguf",
    sha256: "a".repeat(64),
    sizeBytes: 2_000_000_000,
    toolCalling: true,
    licenseName: "Llama 3.2 Community License",
    licenseUrl: "https://example.com/license",
  };

  it("resolves a model reference with the exact backend command contract", async () => {
    invokeMock.mockResolvedValueOnce(resolved);

    await expect(useModelStore.getState().resolveModelReference("llama3.2:3b")).resolves.toEqual(
      resolved,
    );
    expect(invokeMock).toHaveBeenCalledWith("models_resolve_reference", {
      reference: "llama3.2:3b",
    });
  });

  it("installs the resolved canonical reference with its expected digest and refreshes models", async () => {
    const installed = makeModel({
      id: "llama3.2-3b-q4-k-m",
      name: "Llama 3.2 3B",
      repo: resolved.repo,
      file: resolved.fileName,
      size_gb: 2,
      path: `/models/${resolved.fileName}`,
    });
    invokeMock.mockImplementation((command: string) => {
      if (command === "models_install_reference") return Promise.resolve(installed);
      if (command === "models_list_curated") return Promise.resolve([]);
      if (command === "models_list_installed") return Promise.resolve([installed]);
      if (command === "llama_status") {
        return Promise.resolve({ status: "stopped", port: 0, model_path: null });
      }
      return Promise.resolve(undefined);
    });

    await expect(
      useModelStore
        .getState()
        .installModelReference(
          resolved.canonicalReference,
          resolved.sha256,
        ),
    ).resolves.toEqual(installed);

    expect(invokeMock).toHaveBeenCalledWith("models_install_reference", {
      reference: resolved.canonicalReference,
      expectedSha256: resolved.sha256,
    });
    expect(useModelStore.getState().installed).toEqual([installed]);
  });

  it("indexes managed download progress by both local file and canonical reference", () => {
    const entries = modelDownloadProgressEntries({
      file: "ollama-llama3.2-aabbccddeeff.gguf",
      reference: "ollama:library/llama3.2:3b",
      component: "projector",
      componentDownloaded: 250,
      componentTotal: 500,
      downloaded: 500,
      total: 1_000,
    });

    expect(entries["ollama-llama3.2-aabbccddeeff.gguf"]).toEqual({
      downloaded: 500,
      total: 1_000,
      component: "projector",
      componentDownloaded: 250,
      componentTotal: 500,
    });
    expect(entries["ollama:library/llama3.2:3b"]).toEqual({
      downloaded: 500,
      total: 1_000,
      component: "projector",
      componentDownloaded: 250,
      componentTotal: 500,
    });
  });
});

describe("modelStore.cancelDownload", () => {
  it("invokes models_cancel_download with the model's file", async () => {
    invokeMock.mockResolvedValue(undefined);
    const model = makeModel({ file: "qwen2.5-14b-instruct.gguf", installed: false });

    await useModelStore.getState().cancelDownload(model);

    expect(invokeMock).toHaveBeenCalledWith("models_cancel_download", {
      file: "qwen2.5-14b-instruct.gguf",
    });
  });
});

describe("modelStore.download", () => {
  it("clears the file's downloadProgress entry when the backend call rejects (e.g. cancelled)", async () => {
    const model = makeModel({ file: "qwen2.5-14b-instruct.gguf", installed: false });
    useModelStore.setState({
      downloadProgress: { [model.file]: { downloaded: 500, total: 1_000 } },
    });
    invokeMock.mockRejectedValue(new Error("Download cancelled"));

    await expect(useModelStore.getState().download(model)).rejects.toThrow("Download cancelled");

    expect(useModelStore.getState().downloadProgress[model.file]).toBeUndefined();
  });

  it("starts the model it just pulled, so Pull is one click and not two", async () => {
    const model = makeModel({ file: "qwen2.5-14b-instruct.gguf", installed: false, path: undefined });
    const installed = makeModel({
      file: "qwen2.5-14b-instruct.gguf",
      path: "/models/qwen2.5-14b-instruct.gguf",
    });
    invokeMock.mockImplementation((command: string) => {
      if (command === "models_list_installed") return Promise.resolve([installed]);
      if (command === "models_list_curated") return Promise.resolve([]);
      if (command === "llama_status") return Promise.resolve({ status: "stopped", port: 8090, model_path: null });
      if (command === "llama_start") return Promise.resolve(8192);
      return Promise.resolve(undefined);
    });

    await useModelStore.getState().download(model);

    expect(invokeMock).toHaveBeenCalledWith("llama_start", {
      modelPath: installed.path,
      gpuLayers: 999,
      embeddings: false,
    });
    expect(useModelStore.getState().active?.path).toBe(installed.path);
  });

  it("leaves a running model alone — a background pull must not kill a live chat", async () => {
    const running = makeModel();
    useModelStore.setState({ active: running, llamaStatus: "ready" });
    const model = makeModel({ file: "qwen2.5-14b-instruct.gguf", installed: false, path: undefined });
    invokeMock.mockImplementation((command: string) => {
      if (command === "models_list_installed") {
        return Promise.resolve([running, makeModel({ file: model.file, path: "/models/new.gguf" })]);
      }
      if (command === "models_list_curated") return Promise.resolve([]);
      if (command === "llama_status") {
        return Promise.resolve({ status: "ready", port: 8090, model_path: running.path });
      }
      return Promise.resolve(undefined);
    });

    await useModelStore.getState().download(model);

    expect(invokeMock).not.toHaveBeenCalledWith("llama_start", expect.anything());
    expect(useModelStore.getState().active?.path).toBe(running.path);
  });
});

describe("modelStore.createModelfileModel", () => {
  beforeEach(() => {
    useModelStore.setState({ ollamaPullProgress: {}, ollamaPullError: {} });
  });

  it("invokes ollama_create_from_modelfile with {shortName, modelfileText} and refreshes on success", async () => {
    invokeMock.mockImplementation((command: string) => {
      if (command === "ollama_create_from_modelfile") return Promise.resolve(undefined);
      if (command === "ollama_status") {
        return Promise.resolve({ reachable: true, version: "0.5.0", binary_found: true });
      }
      if (command === "ollama_list_models") return Promise.resolve([]);
      if (command === "ollama_example_cloud_tags") return Promise.resolve([]);
      return Promise.resolve(undefined);
    });

    await useModelStore
      .getState()
      .createModelfileModel("my-model", "FROM llama3.2:latest\nPARAMETER temperature 0.7\n");

    expect(invokeMock).toHaveBeenCalledWith("ollama_create_from_modelfile", {
      shortName: "my-model",
      modelfileText: "FROM llama3.2:latest\nPARAMETER temperature 0.7\n",
    });
    expect(useModelStore.getState().ollamaReachable).toBe(true);
    expect(useModelStore.getState().ollamaPullError).toEqual({});
  });

  it("records the failure keyed by shortName and rethrows, without clearing other tags' state", async () => {
    useModelStore.setState({
      ollamaPullProgress: { "other-tag": "pulling..." },
      ollamaPullError: {},
    });
    invokeMock.mockRejectedValueOnce(new Error("line 1: FROM requires a value"));

    await expect(
      useModelStore.getState().createModelfileModel("my-model", "FROM \n"),
    ).rejects.toThrow("line 1: FROM requires a value");

    expect(useModelStore.getState().ollamaPullError).toEqual({
      "my-model": "line 1: FROM requires a value",
    });
    expect(useModelStore.getState().ollamaPullProgress).toEqual({ "other-tag": "pulling..." });
  });
});

const EFFORT_BY_TARGET_STORAGE_KEY = "little-monkey-effort-by-target";
const LEGACY_EFFORT_STORAGE_KEY = "little-monkey-effort";

function fakeLocalStorage(initial: Record<string, string> = {}): Storage {
  const store = new Map(Object.entries(initial));
  return {
    getItem: (key: string) => store.get(key) ?? null,
    setItem: (key: string, value: string) => void store.set(key, value),
    removeItem: (key: string) => void store.delete(key),
    clear: () => store.clear(),
    key: (index: number) => [...store.keys()][index] ?? null,
    get length() {
      return store.size;
    },
  } as Storage;
}

/** `effortByTarget` hydration (and the one-time legacy migration) runs at
 * module import, so every scenario needs a fresh module instance against its
 * own pre-seeded localStorage — same `resetModules` + dynamic-import idiom
 * as `settingsStore.test.ts`'s default-hydration tests, plus a stubbed
 * `localStorage` since vitest's `node` environment has none. */
async function loadStore(initial: Record<string, string> = {}) {
  vi.resetModules();
  vi.stubGlobal("localStorage", fakeLocalStorage(initial));
  return await import("./modelStore");
}

afterEach(() => {
  vi.unstubAllGlobals();
});

describe("modelStore.effortByTarget hydration", () => {
  it("hydrates a persisted per-target map, dropping entries with unknown levels", async () => {
    const key = providerModelTargetKey("openai", "gpt-5");
    const fresh = await loadStore({
      [EFFORT_BY_TARGET_STORAGE_KEY]: JSON.stringify({
        [key]: "low",
        [providerModelTargetKey("anthropic", "claude-sonnet")]: "extreme",
        [ollamaModelTargetKey("llama3.2:latest")]: 3,
      }),
    });
    expect(fresh.useModelStore.getState().effortByTarget).toEqual({ [key]: "low" });
  });

  it("hydrates an empty map from corrupt JSON or a non-object payload", async () => {
    expect(
      (await loadStore({ [EFFORT_BY_TARGET_STORAGE_KEY]: "{not json" })).useModelStore.getState().effortByTarget,
    ).toEqual({});
    expect(
      (await loadStore({ [EFFORT_BY_TARGET_STORAGE_KEY]: '["low"]' })).useModelStore.getState().effortByTarget,
    ).toEqual({});
  });

  it("hydrates an empty map when nothing was ever persisted", async () => {
    const fresh = await loadStore();
    expect(fresh.useModelStore.getState().effortByTarget).toEqual({});
  });

  it("migrates a legacy global effort choice into the Anthropic-wide fallback entry, once", async () => {
    const fresh = await loadStore({ [LEGACY_EFFORT_STORAGE_KEY]: "xhigh" });
    expect(fresh.useModelStore.getState().effortByTarget).toEqual({ [ANTHROPIC_EFFORT_FALLBACK_KEY]: "xhigh" });
    // The migration persists immediately, so the next hydration reads the
    // per-target map instead of re-running the migration.
    expect(JSON.parse(localStorage.getItem(EFFORT_BY_TARGET_STORAGE_KEY) ?? "null")).toEqual({
      [ANTHROPIC_EFFORT_FALLBACK_KEY]: "xhigh",
    });
  });

  it("ignores an invalid legacy value instead of migrating it", async () => {
    const fresh = await loadStore({ [LEGACY_EFFORT_STORAGE_KEY]: "extreme" });
    expect(fresh.useModelStore.getState().effortByTarget).toEqual({});
  });

  it("never re-runs the migration once the per-target key exists — even for an empty map", async () => {
    const fresh = await loadStore({
      [EFFORT_BY_TARGET_STORAGE_KEY]: "{}",
      [LEGACY_EFFORT_STORAGE_KEY]: "max",
    });
    expect(fresh.useModelStore.getState().effortByTarget).toEqual({});
  });
});

describe("modelStore.setEffortForTarget", () => {
  const openaiKey = providerModelTargetKey("openai", "gpt-5");
  const anthropicKey = providerModelTargetKey("anthropic", "claude-sonnet");

  it("sets a per-model level and persists the whole map", async () => {
    const fresh = await loadStore();
    fresh.useModelStore.getState().setEffortForTarget(openaiKey, "low");
    fresh.useModelStore.getState().setEffortForTarget(anthropicKey, "max");
    expect(fresh.useModelStore.getState().effortByTarget).toEqual({ [openaiKey]: "low", [anthropicKey]: "max" });
    expect(JSON.parse(localStorage.getItem(EFFORT_BY_TARGET_STORAGE_KEY) ?? "null")).toEqual({
      [openaiKey]: "low",
      [anthropicKey]: "max",
    });
  });

  it("clears back to Default by deleting the entry", async () => {
    const fresh = await loadStore({
      [EFFORT_BY_TARGET_STORAGE_KEY]: JSON.stringify({ [openaiKey]: "high" }),
    });
    fresh.useModelStore.getState().setEffortForTarget(openaiKey, null);
    expect(fresh.useModelStore.getState().effortByTarget).toEqual({});
    expect(JSON.parse(localStorage.getItem(EFFORT_BY_TARGET_STORAGE_KEY) ?? "null")).toEqual({});
  });

  it("choosing Default for an Anthropic model also retires the migrated legacy fallback", async () => {
    const fresh = await loadStore({ [LEGACY_EFFORT_STORAGE_KEY]: "max" });
    fresh.useModelStore.getState().setEffortForTarget(anthropicKey, null);
    expect(fresh.useModelStore.getState().effortByTarget).toEqual({});
  });

  it("choosing Default for a non-Anthropic model leaves the Anthropic fallback alone", async () => {
    const fresh = await loadStore({ [LEGACY_EFFORT_STORAGE_KEY]: "max" });
    fresh.useModelStore.getState().setEffortForTarget(openaiKey, null);
    expect(fresh.useModelStore.getState().effortByTarget).toEqual({ [ANTHROPIC_EFFORT_FALLBACK_KEY]: "max" });
  });
});

describe("effortForTarget", () => {
  it("resolves a provider target's own entry, and undefined (Default: send nothing) without one", async () => {
    const key = providerModelTargetKey("openai", "gpt-5");
    const fresh = await loadStore({
      [EFFORT_BY_TARGET_STORAGE_KEY]: JSON.stringify({ [key]: "medium" }),
    });
    expect(fresh.effortForTarget({ kind: "provider", providerId: "openai", model: "gpt-5" })).toBe("medium");
    expect(fresh.effortForTarget({ kind: "provider", providerId: "openai", model: "gpt-5-mini" })).toBeUndefined();
    expect(fresh.effortForTarget({ kind: "provider", providerId: "my-custom-provider", model: "m" })).toBeUndefined();
    expect(fresh.effortForTarget({ kind: "provider", providerId: null, model: null })).toBeUndefined();
  });

  it("falls back to the migrated Anthropic-wide entry for Anthropic models only", async () => {
    const fresh = await loadStore({ [LEGACY_EFFORT_STORAGE_KEY]: "xhigh" });
    expect(fresh.effortForTarget({ kind: "provider", providerId: "anthropic", model: "claude-sonnet" })).toBe("xhigh");
    expect(fresh.effortForTarget({ kind: "provider", providerId: "openai", model: "gpt-5" })).toBeUndefined();
  });

  it("resolves Ollama targets by tag key and local targets to undefined", async () => {
    const tagKey = ollamaModelTargetKey("llama3.2:latest");
    const fresh = await loadStore({
      [EFFORT_BY_TARGET_STORAGE_KEY]: JSON.stringify({ [tagKey]: "low" }),
    });
    expect(fresh.effortForTarget({ kind: "ollama", model: "llama3.2:latest" })).toBe("low");
    expect(fresh.effortForTarget({ kind: "ollama", model: null })).toBeUndefined();
    expect(fresh.effortForTarget({ kind: "local" })).toBeUndefined();
  });
});

describe("start", () => {
  it("refuses an MLX model instead of handing a directory to llama-server", async () => {
    invokeMock.mockClear();
    const store = useModelStore.getState();
    const mlx = makeModel({
      name: "Qwen3.8 27B OptiQ 4bit",
      file: "mlx-0123456789ab-Qwen3.8-27B-OptiQ-4bit",
      path: "/models/mlx-0123456789ab-Qwen3.8-27B-OptiQ-4bit",
      runtime: "mlx",
    });

    await expect(store.start(mlx)).rejects.toThrow(/only loads GGUF/);
    // The point of the guard: no llama_start call was ever made.
    expect(invokeMock).not.toHaveBeenCalledWith("llama_start", expect.anything());
    expect(useModelStore.getState().llamaStatus).toBe("stopped");
    expect(useModelStore.getState().llamaError).toMatch(/MLX model/);
  });
});
