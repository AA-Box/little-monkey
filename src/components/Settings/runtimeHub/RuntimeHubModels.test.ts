import { describe, expect, it } from "vitest";

import type { HardwareProfile, HardwareSnapshot, M3InstalledModel, M3RuntimeCapability } from "../../../lib/runtimeHubClient";
import { buildSchedulingInput } from "./RuntimeHubModels";

describe("Runtime Hub capacity planner input", () => {
  it("uses live memory, advertised estimates, and bounded runtime slots", () => {
    const gib = 1024 ** 3;
    const hardware: HardwareSnapshot = {
      captured_at_ms: 1,
      total_ram_bytes: 16 * gib,
      available_ram_bytes: 10 * gib,
      logical_cpu_count: 8,
      platform: {
        os: "linux",
        arch: "x86_64",
        supported_runtimes: ["ollama", "llama_cpp"],
        accelerators: [{ kind: "cpu", available: true, device_names: [], total_memory_bytes: null, available_memory_bytes: null }],
      },
    };
    const profile: HardwareProfile = {
      tier: "balanced",
      recommended_process_slots: 1,
      recommended_ram_reserve_bytes: 2 * gib,
      preferred_accelerator: "cpu",
    };
    const runtime: M3RuntimeCapability = {
      descriptor: { runtimeId: "ollama", kind: "ollama", label: "Ollama", managed: false, apiBackend: "ollama" },
      canLoad: true,
      canUnload: true,
      canLogs: false,
      canMetrics: true,
      canInfer: true,
      canEmbed: false,
      settings: [],
    };
    const model = (id: string): M3InstalledModel => ({
      assetId: `ollama:${id}:q4`,
      modelId: id,
      displayName: id,
      runtime: "ollama",
      variantId: "q4",
      capabilities: { chat: true, embeddings: false, toolCalling: false, vision: false, structuredOutput: false },
      estimatedRamBytes: 4 * gib,
      estimatedVramBytes: 0,
      requiredAccelerator: "cpu",
      activeVersionKey: "a".repeat(64),
      versions: [],
    });
    const models = [model("one"), model("two")];
    const input = buildSchedulingInput(hardware, profile, [runtime], {}, models, models.map((entry) => entry.assetId));

    expect(input.memory.available_ram_bytes).toBe(10 * gib);
    expect(input.memory.reserve_ram_bytes).toBe(2 * gib);
    expect(input.process_slots).toHaveLength(1);
    expect(input.targets.map((target) => target.model_id)).toEqual(["one", "two"]);
    expect(input.targets.every((target) => target.accelerator === "cpu")).toBe(true);
  });

  it("keeps MLX outside the shared Ollama/llama.cpp scheduler", () => {
    const hardware = {
      captured_at_ms: 1,
      total_ram_bytes: 8,
      available_ram_bytes: 8,
      logical_cpu_count: 4,
      platform: { os: "macos", arch: "aarch64", supported_runtimes: ["ollama", "llama_cpp"], accelerators: [] },
    } as HardwareSnapshot;
    const profile = { tier: "constrained", recommended_process_slots: 1, recommended_ram_reserve_bytes: 1, preferred_accelerator: "cpu" } as HardwareProfile;
    const mlx = {
      assetId: "mlx:model:q4",
      modelId: "model",
      displayName: "MLX model",
      runtime: "mlx",
      variantId: "q4",
      estimatedRamBytes: 4,
      estimatedVramBytes: 0,
      requiredAccelerator: "metal",
      activeVersionKey: "a".repeat(64),
      capabilities: { chat: true, embeddings: false, toolCalling: false, vision: false, structuredOutput: false },
      versions: [],
    } as M3InstalledModel;
    expect(buildSchedulingInput(hardware, profile, [], {}, [mlx], [mlx.assetId]).targets).toEqual([]);
  });
});

