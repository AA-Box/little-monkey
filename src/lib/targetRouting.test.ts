import { beforeEach, describe, expect, it, vi } from "vitest";

const invokeMock = vi.fn();
vi.mock("@tauri-apps/api/core", () => ({
  invoke: (...args: unknown[]) => invokeMock(...args),
  isTauri: () => false,
}));
vi.mock("@tauri-apps/api/event", () => ({
  listen: () => Promise.resolve(() => {}),
}));

import {
  resolveLoadedLocalEndpoint,
  resolveTarget,
  resolvedTargetSupportsVision,
  snapshotForResolvedTarget,
} from "./targetRouting";
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
    mlxChat: null,
  });
});

describe("MLX local runtime", () => {
  const mlxModel = (): ModelInfo => ({
    ...localModel(),
    id: "mlx-qwen",
    file: "mlx-0123456789ab-Qwen3.8-27B-OptiQ-4bit",
    path: "/models/mlx-0123456789ab-Qwen3.8-27B-OptiQ-4bit",
    runtime: "mlx",
  });

  it("resolves a local target to the MLX endpoint when an MLX model is active", async () => {
    const model = mlxModel();
    useModelStore.setState({ installed: [model], active: model });
    invokeMock.mockImplementation((command: string) => {
      if (command === "mlx_chat_status") {
        return Promise.resolve({ running: true, port: 51234, modelId: "m", modelPath: model.path, vision: true });
      }
      // llama-server is legitimately stopped while MLX holds the slot; reading
      // it here would resolve the turn to a port with nothing behind it.
      if (command === "llama_status") {
        return Promise.resolve({ status: "stopped", port: 8090, model_path: null, projector_path: null, vision_enabled: false });
      }
      return Promise.resolve(undefined);
    });

    const target = await resolveTarget();

    expect(target).toMatchObject({ kind: "local", baseUrl: "http://127.0.0.1:51234" });
    expect(useModelStore.getState().llamaStatus).toBe("ready");
    expect(useModelStore.getState().llamaVisionEnabled).toBe(true);
  });

  it("falls back to llama-server when the active model is a GGUF", async () => {
    const model = localModel();
    useModelStore.setState({ installed: [model], active: model });
    invokeMock.mockImplementation((command: string) => {
      if (command === "llama_status") {
        return Promise.resolve({ status: "ready", port: 8090, model_path: model.path, projector_path: null, vision_enabled: false });
      }
      return Promise.resolve(undefined);
    });

    const target = await resolveTarget();

    expect(target).toMatchObject({ kind: "local", baseUrl: "http://127.0.0.1:8090" });
    // A GGUF model must not cost an MLX probe on every turn.
    expect(invokeMock).not.toHaveBeenCalledWith("mlx_chat_status");
  });

  it("resolves a loaded model to whichever runtime holds it, and reports when neither does", async () => {
    const mlx = mlxModel();
    invokeMock.mockImplementation((command: string) => {
      if (command === "mlx_chat_status") {
        return Promise.resolve({ running: true, port: 51234, modelId: "m", modelPath: mlx.path, vision: false });
      }
      return Promise.resolve({ status: "ready", port: 8090, model_path: "/models/other.gguf", projector_path: null, vision_enabled: false });
    });

    await expect(resolveLoadedLocalEndpoint(mlx.path!, "MLX model")).resolves.toBe(
      "http://127.0.0.1:51234",
    );
    await expect(resolveLoadedLocalEndpoint("/models/other.gguf", "GGUF model")).resolves.toBe(
      "http://127.0.0.1:8090",
    );
    await expect(resolveLoadedLocalEndpoint("/models/gone.gguf", "Missing model")).rejects.toThrow(
      /no longer loaded/,
    );
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
