import { describe, expect, it } from "vitest";

import type {
  ModelInfo,
  OllamaModelInfo,
  ProviderConfig,
  ProviderModelInfo,
} from "../store/modelStore";
import {
  assertValidComparisonTargets,
  buildModelTargetInventory,
  findActiveModelTarget,
  isModelTargetSnapshot,
  localModelTargetKey,
  ollamaModelTargetKey,
  providerModelTargetKey,
  validateComparisonTargets,
  type ModelTargetInventoryInput,
  type ModelTargetSnapshot,
} from "./modelTargets";

function localModel(overrides: Partial<ModelInfo> = {}): ModelInfo {
  return {
    id: "qwen/7b",
    name: "Qwen 7B",
    repo: "example/qwen",
    file: "qwen.gguf",
    size_gb: 4,
    tool_calling: true,
    installed: true,
    path: "/models/qwen.gguf",
    is_external: false,
    kind: "chat",
    ...overrides,
  };
}

function ollamaModel(overrides: Partial<OllamaModelInfo> = {}): OllamaModelInfo {
  return {
    name: "llama3.2:latest",
    size_bytes: 3_000_000_000,
    is_cloud: false,
    tool_calling: true,
    vision: false,
    modified_at: "2026-07-13T00:00:00Z",
    ...overrides,
  };
}

function provider(overrides: Partial<ProviderConfig> = {}): ProviderConfig {
  return {
    id: "anthropic",
    label: "Anthropic",
    base_url: "https://api.anthropic.com",
    is_custom: false,
    has_key: true,
    ...overrides,
  };
}

function providerModel(id: string): ProviderModelInfo {
  return { id };
}

function inventoryInput(overrides: Partial<ModelTargetInventoryInput> = {}): ModelTargetInventoryInput {
  const active = localModel();
  return {
    installed: [active],
    active,
    llamaStatus: "ready",
    ollamaModels: [],
    ollamaReachable: false,
    providers: [],
    providerModels: {},
    effort: "high",
    ...overrides,
  };
}

function targetOfKind<K extends ModelTargetSnapshot["kind"]>(
  targets: readonly ModelTargetSnapshot[],
  kind: K,
): Extract<ModelTargetSnapshot, { kind: K }> {
  const target = targets.find((candidate) => candidate.kind === kind);
  if (!target) throw new Error(`Missing ${kind} target in test inventory`);
  return target as Extract<ModelTargetSnapshot, { kind: K }>;
}

describe("buildModelTargetInventory", () => {
  it("offers only the current ready, installed chat model for local llama.cpp", () => {
    const active = localModel();
    const inventory = buildModelTargetInventory(
      inventoryInput({
        active,
        installed: [
          active,
          localModel({ id: "other", name: "Other", path: "/models/other.gguf" }),
          localModel({ id: "embed", name: "Embed", path: "/models/embed.gguf", kind: "embedding" }),
        ],
      }),
    );

    expect(inventory.groups).toHaveLength(1);
    expect(inventory.groups[0]).toMatchObject({ key: "local", kind: "local", label: "Local" });
    expect(inventory.targets).toHaveLength(1);
    expect(inventory.targets[0]).toMatchObject({
      kind: "local",
      key: localModelTargetKey(active.id),
      modelId: active.id,
      modelPath: active.path,
      displayName: active.name,
      capabilities: {
        toolCalling: { state: "yes" },
        vision: { state: "unknown" },
      },
      availability: { status: "available" },
    });
  });

  it.each([
    ["server stopped", { llamaStatus: "stopped" as const }],
    ["active model is an embedding model", { active: localModel({ kind: "embedding" }) }],
    ["active model has no path", { active: localModel({ path: null }) }],
    ["active model is not installed", { active: localModel({ installed: false }) }],
    ["active model is absent", { active: null }],
  ])("does not offer local when %s", (_label, override) => {
    const inventory = buildModelTargetInventory(inventoryInput(override));
    expect(inventory.targets.filter((target) => target.kind === "local")).toEqual([]);
  });

  it("groups models for connected providers and snapshots effort only for Anthropic", () => {
    const providers = [
      provider(),
      provider({ id: "openai", label: "OpenAI" }),
      provider({ id: "disconnected", label: "Disconnected", has_key: false }),
    ];
    const inventory = buildModelTargetInventory(
      inventoryInput({
        active: null,
        installed: [],
        providers,
        providerModels: {
          anthropic: [providerModel("claude-sonnet"), providerModel("claude-sonnet")],
          openai: [providerModel("gpt-5")],
          disconnected: [providerModel("hidden")],
        },
        effort: "xhigh",
      }),
    );

    expect(inventory.groups.map((group) => [group.key, group.label])).toEqual([
      ["provider:anthropic", "Anthropic"],
      ["provider:openai", "OpenAI"],
    ]);
    expect(inventory.targets).toHaveLength(2);
    expect(inventory.targets[0]).toMatchObject({
      kind: "provider",
      providerId: "anthropic",
      model: "claude-sonnet",
      effort: "xhigh",
      capabilities: {
        toolCalling: { state: "unknown" },
        vision: { state: "unknown" },
      },
      availability: { status: "available" },
    });
    expect(inventory.targets[1]).not.toHaveProperty("effort");
    expect(inventory.targets.some((target) => target.displayName === "hidden")).toBe(false);
  });

  it("groups every Ollama tag, preserving reported capabilities and daemon availability", () => {
    const inventory = buildModelTargetInventory(
      inventoryInput({
        active: null,
        installed: [],
        ollamaModels: [
          ollamaModel(),
          ollamaModel({ name: "llava:13b", tool_calling: false, vision: true }),
        ],
        ollamaReachable: false,
        ollamaBaseUrl: "http://192.168.1.10:11434///",
      }),
    );

    expect(inventory.groups).toHaveLength(1);
    expect(inventory.groups[0]).toMatchObject({ key: "ollama", label: "Ollama" });
    expect(inventory.targets).toHaveLength(2);
    const visionTarget = inventory.targets[1];
    expect(visionTarget).toMatchObject({
      kind: "ollama",
      baseUrl: "http://192.168.1.10:11434",
      model: "llava:13b",
      capabilities: {
        toolCalling: { state: "no" },
        vision: { state: "yes" },
      },
      availability: { status: "unavailable" },
    });
    expect(visionTarget.availability.evidence).toMatch(/not reachable/i);
  });

  it("returns deeply immutable inventory structures", () => {
    const inventory = buildModelTargetInventory(
      inventoryInput({ ollamaModels: [ollamaModel()], ollamaReachable: true }),
    );
    const target = inventory.targets[0];

    expect(Object.isFrozen(inventory)).toBe(true);
    expect(Object.isFrozen(inventory.groups)).toBe(true);
    expect(Object.isFrozen(inventory.targets)).toBe(true);
    expect(Object.isFrozen(inventory.groups[0])).toBe(true);
    expect(Object.isFrozen(inventory.groups[0].targets)).toBe(true);
    expect(Object.isFrozen(target)).toBe(true);
    expect(Object.isFrozen(target.capabilities)).toBe(true);
    expect(Object.isFrozen(target.capabilities.toolCalling)).toBe(true);
    expect(Object.isFrozen(target.availability)).toBe(true);
  });
});

describe("stable target keys", () => {
  it("namespaces and URL-encodes identity segments to avoid collisions", () => {
    expect(localModelTargetKey("org/model:7b")).toBe("local:org%2Fmodel%3A7b");
    expect(ollamaModelTargetKey("org/model:7b")).toBe("ollama:org%2Fmodel%3A7b");
    expect(providerModelTargetKey("custom:one", "org/model:7b")).toBe(
      "provider:custom%3Aone:org%2Fmodel%3A7b",
    );
  });
});

describe("findActiveModelTarget", () => {
  const active = localModel();
  const inventory = buildModelTargetInventory(
    inventoryInput({
      active,
      ollamaModels: [ollamaModel()],
      ollamaReachable: true,
      providers: [provider()],
      providerModels: { anthropic: [providerModel("claude-sonnet")] },
    }),
  );

  it("finds the active local, provider, or Ollama snapshot without consulting a store", () => {
    expect(
      findActiveModelTarget(inventory, {
        activeProvider: "local",
        active,
        activeOllamaModel: null,
        activeProviderId: null,
        activeProviderModel: null,
      })?.kind,
    ).toBe("local");
    expect(
      findActiveModelTarget(inventory.targets, {
        activeProvider: "ollama",
        active,
        activeOllamaModel: "llama3.2:latest",
        activeProviderId: null,
        activeProviderModel: null,
      })?.kind,
    ).toBe("ollama");
    expect(
      findActiveModelTarget(inventory, {
        activeProvider: "provider",
        active,
        activeOllamaModel: null,
        activeProviderId: "anthropic",
        activeProviderModel: "claude-sonnet",
      })?.kind,
    ).toBe("provider");
  });

  it("returns null for incomplete or absent selections", () => {
    expect(
      findActiveModelTarget(inventory, {
        activeProvider: "provider",
        active,
        activeOllamaModel: null,
        activeProviderId: "anthropic",
        activeProviderModel: null,
      }),
    ).toBeNull();
    expect(
      findActiveModelTarget(inventory, {
        activeProvider: "ollama",
        active,
        activeOllamaModel: "missing",
        activeProviderId: null,
        activeProviderModel: null,
      }),
    ).toBeNull();
  });
});

describe("comparison target validation", () => {
  const inventory = buildModelTargetInventory(
    inventoryInput({
      ollamaModels: [ollamaModel(), ollamaModel({ name: "llava:13b" })],
      ollamaReachable: true,
      providers: [provider()],
      providerModels: { anthropic: [providerModel("claude-sonnet"), providerModel("claude-haiku")] },
    }),
  );
  const local = targetOfKind(inventory.targets, "local");
  const ollama = targetOfKind(inventory.targets, "ollama");
  const providerTarget = targetOfKind(inventory.targets, "provider");

  it("accepts two through four unique targets", () => {
    expect(validateComparisonTargets([local, ollama])).toEqual({ valid: true, errors: [] });
    expect(validateComparisonTargets(inventory.targets.slice(0, 4)).valid).toBe(true);
    expect(() => assertValidComparisonTargets([local, providerTarget])).not.toThrow();
  });

  it("rejects fewer than two and more than four targets", () => {
    expect(validateComparisonTargets([local]).errors.map((error) => error.code)).toContain("too_few_targets");
    const five = inventory.targets.slice(0, 5);
    expect(five).toHaveLength(5);
    expect(validateComparisonTargets(five).errors.map((error) => error.code)).toContain("too_many_targets");
  });

  it("rejects duplicate target identities", () => {
    const result = validateComparisonTargets([ollama, ollama]);
    expect(result.valid).toBe(false);
    expect(result.errors.map((error) => error.code)).toContain("duplicate_target");
    expect(() => assertValidComparisonTargets([ollama, ollama])).toThrow(/unique/i);
  });

  it("rejects more than one local llama.cpp target", () => {
    const secondLocal = {
      ...local,
      key: localModelTargetKey("second-local"),
      modelId: "second-local",
      modelPath: "/models/second.gguf",
    } satisfies ModelTargetSnapshot;
    const result = validateComparisonTargets([local, secondLocal]);
    expect(result.valid).toBe(false);
    expect(result.errors.map((error) => error.code)).toContain("multiple_local_targets");
  });
});

describe("isModelTargetSnapshot", () => {
  const inventory = buildModelTargetInventory(
    inventoryInput({
      ollamaModels: [ollamaModel()],
      providers: [provider()],
      providerModels: { anthropic: [providerModel("claude-sonnet")] },
    }),
  );

  it("accepts every snapshot emitted by the inventory builder", () => {
    expect(inventory.targets.every((target) => isModelTargetSnapshot(target))).toBe(true);
  });

  it.each([
    ["null", null],
    ["missing capabilities", { ...inventory.targets[0], capabilities: undefined }],
    ["bad capability state", {
      ...inventory.targets[0],
      capabilities: {
        ...inventory.targets[0].capabilities,
        vision: { state: "maybe", evidence: "bad" },
      },
    }],
    ["bad effort", { ...inventory.targets[0], effort: "extreme" }],
    ["corrupt stable key", { ...inventory.targets[0], key: "local:wrong" }],
  ])("rejects %s", (_label, value) => {
    expect(isModelTargetSnapshot(value)).toBe(false);
  });
});
