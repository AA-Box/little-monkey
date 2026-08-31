import { describe, expect, it } from "vitest";
import type { ModelInfo, OllamaModelInfo, ProviderConfig, ProviderModelInfo } from "../store/modelStore";
import {
  buildRecipeTargetOptions,
  isTargetAvailable,
  recipeTargetKey,
  recipeTargetLabel,
} from "./channelRouteModel";

const chatModel = (id: string, overrides: Partial<ModelInfo> = {}): ModelInfo => ({
  id,
  name: id,
  repo: "",
  file: "",
  size_gb: 1,
  tool_calling: true,
  installed: true,
  path: `/models/${id}.gguf`,
  is_external: false,
  kind: "chat",
  ...overrides,
});

const ollama = (name: string): OllamaModelInfo => ({
  name,
  size_bytes: 1,
  is_cloud: false,
  tool_calling: true,
  vision: false,
  modified_at: "",
});

const provider = (id: string, overrides: Partial<ProviderConfig> = {}): ProviderConfig => ({
  id,
  label: id,
  base_url: `https://${id}.example`,
  is_custom: false,
  has_key: true,
  is_extension: false,
  ...overrides,
});

const providerModel = (id: string): ProviderModelInfo => ({ id });

describe("buildRecipeTargetOptions", () => {
  it("offers every installed chat model, not just the active one", () => {
    // The chat inventory (`buildModelTargetInventory`) offers only the active
    // local model, and only while llama is ready, because a chat turn starts
    // now. A recipe target is resolved later by the runner, which starts the
    // managed runtime itself — so an installed-but-idle model is a legal
    // choice, and excluding it would hide models the operator actually has.
    const options = buildRecipeTargetOptions({
      installed: [chatModel("Qwen2.5-7B"), chatModel("Llama-3-8B")],
      ollamaModels: [],
      ollamaReachable: false,
      providers: [],
      providerModels: {},
    });
    expect(options.map((option) => option.target.managed_model)).toEqual([
      "Qwen2.5-7B",
      "Llama-3-8B",
    ]);
  });

  it("skips embedding models and uninstalled ones", () => {
    const options = buildRecipeTargetOptions({
      installed: [
        chatModel("bge-m3", { kind: "embedding" }),
        chatModel("not-here", { installed: false }),
        chatModel("real"),
      ],
      ollamaModels: [],
      ollamaReachable: false,
      providers: [],
      providerModels: {},
    });
    expect(options.map((option) => option.displayName)).toEqual(["real"]);
  });

  it("omits Ollama entirely when the daemon is unreachable", () => {
    const input = {
      installed: [],
      ollamaModels: [ollama("qwen3.8:27b-mlx")],
      providers: [],
      providerModels: {},
    };
    expect(buildRecipeTargetOptions({ ...input, ollamaReachable: false })).toHaveLength(0);
    expect(buildRecipeTargetOptions({ ...input, ollamaReachable: true })).toHaveLength(1);
  });

  it("offers a provider's models only when it is connected", () => {
    // A provider target with no credential fails at run time with the
    // provider's own 401, which names nothing the operator can act on. But
    // "connected" is `providerIsConnected`, not `has_key`: an extension
    // provider authenticates inside its own sandbox and holds no key here, so
    // reading `has_key` would hide models that work.
    const options = buildRecipeTargetOptions({
      installed: [],
      ollamaModels: [],
      ollamaReachable: false,
      providers: [
        provider("openrouter"),
        provider("openai", { has_key: false }),
        provider("ext", { has_key: false, is_extension: true }),
      ],
      providerModels: {
        openrouter: [providerModel("anthropic/claude-sonnet")],
        openai: [providerModel("gpt-5")],
        ext: [providerModel("sandboxed-1")],
      },
    });
    expect(options.map((option) => option.key)).toEqual([
      "provider:openrouter/anthropic/claude-sonnet",
      "provider:ext/sandboxed-1",
    ]);
  });

  it("produces targets that satisfy the recipe XOR", () => {
    const options = buildRecipeTargetOptions({
      installed: [chatModel("local-one")],
      ollamaModels: [ollama("tag")],
      ollamaReachable: true,
      providers: [provider("openrouter")],
      providerModels: { openrouter: [providerModel("a/b")] },
    });
    expect(options).toHaveLength(3);
    for (const { target } of options) {
      const set = [target.provider, target.ollama, target.local_url, target.managed_model].filter(
        (value) => value !== undefined,
      );
      expect(set).toHaveLength(1);
    }
  });
});

describe("recipeTargetKey", () => {
  it("round-trips every option back to its own key", () => {
    const options = buildRecipeTargetOptions({
      installed: [chatModel("local-one")],
      ollamaModels: [ollama("tag")],
      ollamaReachable: true,
      providers: [provider("openrouter")],
      providerModels: { openrouter: [providerModel("a/b")] },
    });
    for (const option of options) {
      expect(recipeTargetKey(option.target)).toBe(option.key);
    }
  });

  it("has no key for an empty or unset target", () => {
    expect(recipeTargetKey(null)).toBeNull();
    expect(recipeTargetKey({})).toBeNull();
    // `provider` without `model` cannot name a model, so it is not a selection.
    expect(recipeTargetKey({ provider: "openrouter" })).toBeNull();
  });
});

describe("recipeTargetLabel", () => {
  it("names the target the recipe actually holds", () => {
    expect(recipeTargetLabel({ ollama: "qwen3.8:27b-mlx" })).toBe("Ollama · qwen3.8:27b-mlx");
    expect(recipeTargetLabel({ managed_model: "Qwen2.5-7B" })).toBe("Local · Qwen2.5-7B");
    expect(recipeTargetLabel({ provider: "openrouter", model: "a/b" })).toBe("openrouter · a/b");
    expect(recipeTargetLabel({ local_url: "http://127.0.0.1:8090" })).toBe(
      "Custom · http://127.0.0.1:8090",
    );
    expect(recipeTargetLabel(null)).toBe("No model set");
  });

  it("still names a model this machine no longer has", () => {
    // Showing "unknown" would hide the one thing the operator opened this to
    // find out — which model the route would answer on.
    expect(recipeTargetLabel({ ollama: "uninstalled:70b" })).toBe("Ollama · uninstalled:70b");
  });
});

describe("isTargetAvailable", () => {
  it("separates a target this machine can still run from one it cannot", () => {
    const options = buildRecipeTargetOptions({
      installed: [],
      ollamaModels: [ollama("here")],
      ollamaReachable: true,
      providers: [],
      providerModels: {},
    });
    expect(isTargetAvailable({ ollama: "here" }, options)).toBe(true);
    expect(isTargetAvailable({ ollama: "gone" }, options)).toBe(false);
    expect(isTargetAvailable(null, options)).toBe(false);
  });
});
