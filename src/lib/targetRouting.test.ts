import { beforeEach, describe, expect, it, vi } from "vitest";

const invokeMock = vi.fn();
vi.mock("@tauri-apps/api/core", () => ({
  invoke: (...args: unknown[]) => invokeMock(...args),
  isTauri: () => false,
}));
vi.mock("@tauri-apps/api/event", () => ({
  listen: () => Promise.resolve(() => {}),
}));

import { resolvedTargetSupportsVision, snapshotForResolvedTarget, resolveTarget } from "./targetRouting";
import { useModelStore, type ModelInfo, type OllamaModelInfo } from "../store/modelStore";

function localModel(): ModelInfo {
  return {
    id: "qwen-27b",
    name: "Qwen 27B",
    repo: "example/qwen",
    file: "qwen-27b.gguf",
    size_gb: 19,
    tool_calling: true,
    installed: true,
    path: "/models/qwen-27b.gguf",
    is_external: true,
    kind: "chat",
  };
}

function ollamaModel(): OllamaModelInfo {
  return {
    name: "qwen3.8:27b-mlx",
    size_bytes: 18_174_721_847,
    is_cloud: false,
    tool_calling: true,
    vision: true,
    modified_at: "2026-08-18T00:00:00Z",
  };
}

beforeEach(() => {
  invokeMock.mockReset();
  useModelStore.setState({
    installed: [],
    active: null,
    llamaStatus: "stopped",
    ollamaModels: [],
    ollamaReachable: false,
    activeProvider: "local",
    activeOllamaModel: null,
    providers: [],
    providerModels: {},
  });
});

describe("resolveTarget", () => {
  it("reconciles native llama status before freezing a local target", async () => {
    const model = localModel();
    useModelStore.setState({ installed: [model] });
    invokeMock.mockImplementation((command: string) => {
      if (command === "llama_status") {
        return Promise.resolve({
          status: "ready",
          port: 8090,
          model_path: model.path,
          projector_path: "/models/mmproj.gguf",
          vision_enabled: true,
        });
      }
      return Promise.resolve(command === "models_list_installed" ? [model] : []);
    });

    const target = await resolveTarget();

    expect(target).toMatchObject({ kind: "local", modelLabel: model.name });
    expect(useModelStore.getState().llamaStatus).toBe("ready");
    expect(useModelStore.getState().llamaVisionEnabled).toBe(true);
    expect(useModelStore.getState().llamaProjectorPath).toBe("/models/mmproj.gguf");
    expect(resolvedTargetSupportsVision(target)).toBe(true);
    expect(snapshotForResolvedTarget(target)).toMatchObject({
      kind: "local",
      modelPath: model.path,
    });
  });

  it("refreshes the local Ollama inventory without depending on the optional cloud catalog", async () => {
    const model = ollamaModel();
    useModelStore.setState({
      activeProvider: "ollama",
      activeOllamaModel: model.name,
    });
    invokeMock.mockImplementation((command: string) => {
      if (command === "ollama_status") {
        return Promise.resolve({ reachable: true, version: "0.32.13", binary_found: true });
      }
      if (command === "ollama_list_models") return Promise.resolve([model]);
      if (command === "ollama_example_cloud_tags") return Promise.reject(new Error("catalog unavailable"));
      return Promise.resolve([]);
    });

    const target = await resolveTarget();

    expect(target).toMatchObject({ kind: "ollama", model: model.name });
    expect(useModelStore.getState().ollamaModels).toEqual([model]);
    expect(snapshotForResolvedTarget(target)).toMatchObject({
      kind: "ollama",
      model: model.name,
    });
  });
});
