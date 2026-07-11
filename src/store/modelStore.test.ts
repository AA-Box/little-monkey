import { beforeEach, describe, expect, it, vi } from "vitest";

const invokeMock = vi.fn();
// See `apiServerStore.test.ts`'s comment on why the `listen` handler must be
// stashed via `vi.hoisted` rather than a plain outer-scope variable — a
// normal `let`/`var` closed over by a hoisted `vi.mock` factory is a
// *different* binding than the one this file's test bodies read later.
vi.mock("@tauri-apps/api/core", () => ({ invoke: (...args: unknown[]) => invokeMock(...args) }));
vi.mock("@tauri-apps/api/event", () => ({
  listen: () => Promise.resolve(() => {}),
}));

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
