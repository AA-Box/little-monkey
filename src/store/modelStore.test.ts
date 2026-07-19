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
import { useModelStore, type ModelInfo } from "./modelStore";

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
  it("passes the current embeddingsEnabled preference to llama_start", async () => {
    useModelStore.getState().setEmbeddingsEnabled(true);
    invokeMock.mockResolvedValueOnce(undefined);

    await useModelStore.getState().start(makeModel());

    expect(invokeMock).toHaveBeenCalledWith("llama_start", {
      modelPath: "/models/qwen2.5-7b-instruct.gguf",
      ctxSize: 4096,
      gpuLayers: 999,
      embeddings: true,
    });
  });

  it("passes embeddings: false when the preference is off", async () => {
    invokeMock.mockResolvedValueOnce(undefined);

    await useModelStore.getState().start(makeModel());

    expect(invokeMock).toHaveBeenCalledWith(
      "llama_start",
      expect.objectContaining({ embeddings: false }),
    );
  });

  it("throws for a model with no path, without calling llama_start", async () => {
    await expect(useModelStore.getState().start(makeModel({ path: null }))).rejects.toThrow(
      /has not been downloaded yet/,
    );
    expect(invokeMock).not.toHaveBeenCalled();
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
