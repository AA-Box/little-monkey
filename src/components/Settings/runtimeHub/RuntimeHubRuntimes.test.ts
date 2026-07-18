import { describe, expect, it } from "vitest";

import type {
  AdvancedSettingCapability,
  ContextCacheView,
  HardwareSnapshot,
  M3InstalledModel,
  M3RuntimeCapability,
  RunningModel,
  RuntimeStatus,
} from "../../../lib/runtimeHubClient";
import type { RuntimeDetail } from "../../../store/runtimeHubStore";
import {
  buildOffloadPlanInput,
  contextCacheHeadline,
  keepAliveForRuntime,
  missingProjectorWarning,
  settingHint,
} from "./RuntimeHubRuntimes";

function capability(overrides: Partial<AdvancedSettingCapability> = {}): AdvancedSettingCapability {
  return {
    key: "flash_attention",
    label: "Flash attention",
    description: "Select llama.cpp flash-attention behavior.",
    schema: { type: "choice", options: ["auto", "on", "off"] },
    default_value: { type: "choice", value: "auto" },
    restart_required: false,
    supported: true,
    unsupported_reason: null,
    ...overrides,
  };
}

describe("settingHint", () => {
  it("renders only the description when supported and restart is not required", () => {
    expect(settingHint(capability())).toBe("Select llama.cpp flash-attention behavior.");
  });

  it("appends a restart note when restart_required is true", () => {
    expect(settingHint(capability({ restart_required: true }))).toBe(
      "Select llama.cpp flash-attention behavior. Restart required.",
    );
  });

  it("appends the unsupported reason only when the control is actually unsupported", () => {
    const gated = capability({
      supported: false,
      unsupported_reason:
        "Flash attention needs a supported GPU backend (Metal, CUDA, ROCm, or Vulkan); this machine's Hardware Compatibility report shows CPU only.",
    });
    expect(settingHint(gated)).toBe(
      "Select llama.cpp flash-attention behavior. Flash attention needs a supported GPU backend (Metal, CUDA, ROCm, or Vulkan); this machine's Hardware Compatibility report shows CPU only.",
    );
  });

  it("never appends a reason when supported is true even if one is somehow present", () => {
    expect(settingHint(capability({ supported: true, unsupported_reason: "stale reason" }))).toBe(
      "Select llama.cpp flash-attention behavior.",
    );
  });

  it("inserts extra text (e.g. byte limits) before the restart/unsupported notes", () => {
    const gated = capability({
      restart_required: true,
      supported: false,
      unsupported_reason: "Select a model to check for a compatible installed draft model.",
    });
    expect(settingHint(gated, " Up to 256 bytes.")).toBe(
      "Select llama.cpp flash-attention behavior. Up to 256 bytes. Restart required. Select a model to check for a compatible installed draft model.",
    );
  });
});

function contextCacheView(overrides: Partial<ContextCacheView> = {}): ContextCacheView {
  return {
    runtimeId: "managed-llama",
    runtimeKind: "llama_cpp",
    configured: { tokens: 4_096, source: "runtime_default", settingKey: "context_size" },
    reportedContextTokens: null,
    contextTokensInUse: null,
    contextHeadroomTokens: null,
    contextShiftDetected: null,
    totalSlots: null,
    notes: [],
    sampledAtMs: 1,
    ...overrides,
  };
}

describe("contextCacheHeadline", () => {
  it("reports unavailable when neither a live nor configured figure is known", () => {
    const view = contextCacheView({ configured: { tokens: null, source: "unavailable", settingKey: null } });
    expect(contextCacheHeadline(view)).toBe("Context size unavailable for this runtime.");
  });

  it("prefers a live-confirmed figure over the merely configured one", () => {
    const view = contextCacheView({ reportedContextTokens: 8_192, configured: { tokens: 4_096, source: "runtime_configured", settingKey: "context_size" } });
    expect(contextCacheHeadline(view)).toBe("8,192 tokens (confirmed live by the runtime)");
  });

  it("labels a persisted setting as configured by this app", () => {
    const view = contextCacheView({ configured: { tokens: 16_384, source: "runtime_configured", settingKey: "num_ctx" } });
    expect(contextCacheHeadline(view)).toBe("16,384 tokens (configured by this app)");
  });

  it("labels an unset setting as the runtime's own default", () => {
    const view = contextCacheView({ configured: { tokens: 4_096, source: "runtime_default", settingKey: "context_size" } });
    expect(contextCacheHeadline(view)).toBe("4,096 tokens (the runtime's default)");
  });
});

describe("runtime load keep-alive policy", () => {
  it("never sends keep_alive to managed llama.cpp", () => {
    expect(keepAliveForRuntime("llama_cpp", "forever", 10)).toBeNull();
    expect(keepAliveForRuntime("llama_cpp", "duration", 10)).toBeNull();
  });

  it("preserves supported forever and duration modes", () => {
    expect(keepAliveForRuntime("ollama", "forever", 10)).toEqual({ mode: "forever" });
    expect(keepAliveForRuntime("mlx", "duration", 5)).toEqual({
      mode: "duration_ms",
      milliseconds: 300_000,
    });
  });

  it("bounds malformed duration input before IPC", () => {
    expect(keepAliveForRuntime("ollama", "duration", Number.NaN)).toEqual({
      mode: "duration_ms",
      milliseconds: 600_000,
    });
    expect(keepAliveForRuntime("ollama", "duration", 99_999)).toEqual({
      mode: "duration_ms",
      milliseconds: 86_400_000,
    });
  });
});

const GIB = 1024 ** 3;

function hardware(): HardwareSnapshot {
  return {
    captured_at_ms: 1,
    total_ram_bytes: 32 * GIB,
    available_ram_bytes: 20 * GIB,
    logical_cpu_count: 8,
    platform: {
      os: "linux",
      arch: "x86_64",
      supported_runtimes: ["ollama", "llama_cpp"],
      accelerators: [
        { kind: "cpu", available: true, device_names: [], total_memory_bytes: null, available_memory_bytes: null },
      ],
    },
  };
}

function installedModel(overrides: Partial<M3InstalledModel> = {}): M3InstalledModel {
  return {
    assetId: "ollama:target:q4",
    modelId: "target",
    displayName: "Target model",
    runtime: "ollama",
    variantId: "q4",
    capabilities: { chat: true, embeddings: false, toolCalling: false, vision: false, structuredOutput: false },
    estimatedRamBytes: 5 * GIB,
    estimatedVramBytes: 0,
    requiredAccelerator: null,
    activeVersionKey: "a".repeat(64),
    versions: [
      {
        versionKey: "a".repeat(64),
        revision: "1",
        sha256: "b".repeat(64),
        sizeBytes: 4 * GIB,
        artifactPath: "/models/target",
        installedAtMs: 1,
        active: true,
        license: {
          name: "MIT",
          spdxId: "MIT",
          sourceUrl: "https://example.com/license",
          revision: "1",
          retrievedAtMs: 1,
          rawDeclaration: "MIT",
        },
        sourceId: "test-source",
        template: null,
        projector: null,
        catalogRetrievedAtMs: null,
        projectorVerification: "not_required",
        projectorVerifiedAtMs: null,
        estimatedProjectorMemoryBytes: null,
        visionReady: false,
      },
    ],
    ...overrides,
  };
}

function runtimeCapability(runtimeId: string, kind: M3RuntimeCapability["descriptor"]["kind"]): M3RuntimeCapability {
  return {
    descriptor: {
      runtimeId,
      kind,
      label: runtimeId,
      managed: false,
      apiBackend: kind === "mlx" ? "mlx" : kind === "ollama" ? "ollama" : "managed_local",
    },
    canLoad: true,
    canUnload: true,
    canLogs: false,
    canMetrics: true,
    canInfer: true,
    canEmbed: false,
    settings: [],
  };
}

function runtimeStatus(runtimeId: string, kind: "ollama" | "llama_cpp"): RuntimeStatus {
  return {
    runtime: {
      schema_version: 1,
      runtime_id: runtimeId,
      kind,
      label: runtimeId,
      endpoint: null,
      managed: false,
    },
    state: "ready",
    version: null,
    process: null,
    message: null,
    checked_at_ms: 1,
  };
}

function resident(modelId: string, memoryBytes: number, vramBytes: number): RunningModel {
  return {
    runtime_id: "other",
    model_id: modelId,
    size_bytes: memoryBytes,
    memory_bytes: memoryBytes,
    vram_bytes: vramBytes,
    digest: null,
    expires_at: null,
    ownership: "app_managed",
  };
}

describe("buildOffloadPlanInput", () => {
  it("uses the active installed version's exact size as weights and reports no residents", () => {
    const model = installedModel();
    const input = buildOffloadPlanInput(hardware(), model, [], {});

    expect(input.model.weights_bytes).toBe(4 * GIB);
    expect(input.model.estimated_ram_bytes).toBe(5 * GIB);
    expect(input.model.estimated_vram_bytes).toBe(0);
    expect(input.model.required_accelerator).toBeNull();
    expect(input.model.has_vision_projector).toBe(false);
    expect(input.reserved).toEqual({ ram_bytes: 0, vram_bytes: 0 });
    expect(input.other_resident_count).toBe(0);
    expect(input.requested_context_tokens).toBeNull();
  });

  it("sums memory from every other resident model across runtimes but excludes the target model itself", () => {
    const model = installedModel();
    const runtimeA = runtimeCapability("ollama-main", "ollama");
    const runtimeB = runtimeCapability("managed-llama", "llama_cpp");
    const runtimeDetails: Record<string, RuntimeDetail> = {
      "ollama-main": {
        status: {
          runtimeType: "adapter",
          status: runtimeStatus("ollama-main", "ollama"),
          running_models: [resident("target", 4 * GIB, 0), resident("other-model", 2 * GIB, 1 * GIB)],
        },
      },
      "managed-llama": {
        status: {
          runtimeType: "adapter",
          status: runtimeStatus("managed-llama", "llama_cpp"),
          running_models: [resident("third-model", 1 * GIB, 0)],
        },
      },
    };

    const input = buildOffloadPlanInput(hardware(), model, [runtimeA, runtimeB], runtimeDetails);

    // "target" is already resident under the same model id (e.g. re-loading
    // with a fresh setting); it must not count against itself as a reserve.
    expect(input.other_resident_count).toBe(2);
    expect(input.reserved).toEqual({ ram_bytes: 3 * GIB, vram_bytes: 1 * GIB });
  });

  it("falls back to the estimated RAM figure when no active version is recorded", () => {
    const model = installedModel({ versions: [] });
    const input = buildOffloadPlanInput(hardware(), model, [], {});
    expect(input.model.weights_bytes).toBe(5 * GIB);
  });

  it("forwards a real projector's memory estimate and requested context tokens", () => {
    const model = installedModel({
      capabilities: { chat: true, embeddings: false, toolCalling: false, vision: true, structuredOutput: false },
      requiredAccelerator: "cuda",
      versions: [
        {
          ...installedModel().versions[0],
          projector: { kind: "clip", sha256: "c".repeat(64), sizeBytes: 512 * 1024 * 1024 },
          projectorVerification: "unverified",
          estimatedProjectorMemoryBytes: 512 * 1024 * 1024,
        },
      ],
    });
    const input = buildOffloadPlanInput(hardware(), model, [], {}, 16_384);
    expect(input.model.has_vision_projector).toBe(true);
    expect(input.model.projector_memory_bytes).toBe(512 * 1024 * 1024);
    expect(input.model.required_accelerator).toBe("cuda");
    expect(input.requested_context_tokens).toBe(16_384);
  });

  it("never reports a vision projector for the offload plan when no projector reference is attached, even if capabilities.vision is declared true", () => {
    // ROADMAP Phase 8 item 12: a declared-vision model with no projector at
    // all must not make the offload planner reserve/place a phantom
    // component — the missing-projector warning near the load flow is what
    // surfaces that gap instead.
    const model = installedModel({
      capabilities: { chat: true, embeddings: false, toolCalling: false, vision: true, structuredOutput: false },
    });
    const input = buildOffloadPlanInput(hardware(), model, [], {});
    expect(input.model.has_vision_projector).toBe(false);
    expect(input.model.projector_memory_bytes).toBe(0);
  });
});

describe("missingProjectorWarning", () => {
  it("returns null when the active version needs no projector", () => {
    expect(missingProjectorWarning(installedModel())).toBeNull();
  });

  it("warns when vision is declared but no projector reference exists at all", () => {
    const model = installedModel({
      versions: [{ ...installedModel().versions[0], projectorVerification: "missing_reference" }],
    });
    expect(missingProjectorWarning(model)).toContain("no associated multimodal projector");
  });

  it("warns (differently) when a projector is declared but not yet verified", () => {
    const model = installedModel({
      versions: [
        {
          ...installedModel().versions[0],
          projector: { kind: "clip", sha256: "c".repeat(64), sizeBytes: 1024 },
          projectorVerification: "unverified",
        },
      ],
    });
    expect(missingProjectorWarning(model)).toContain("has not been verified locally yet");
  });

  it("returns null once the projector is verified", () => {
    const model = installedModel({
      versions: [
        {
          ...installedModel().versions[0],
          projector: { kind: "clip", sha256: "c".repeat(64), sizeBytes: 1024 },
          projectorVerification: "verified",
          visionReady: true,
        },
      ],
    });
    expect(missingProjectorWarning(model)).toBeNull();
  });

  it("returns null when there is no active version at all", () => {
    expect(missingProjectorWarning(installedModel({ versions: [] }))).toBeNull();
  });
});
