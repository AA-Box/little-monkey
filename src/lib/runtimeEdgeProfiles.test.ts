import { describe, expect, it } from "vitest";
import type { HardwareProfile, HardwareSnapshot, M3HardwareCompatibilityReport } from "./runtimeHubClient";
import { resolveEdgeRuntimeProfile } from "./runtimeEdgeProfiles";

const GIB = 1024 ** 3;

function snapshot(overrides: Partial<HardwareSnapshot> = {}): HardwareSnapshot {
  return {
    captured_at_ms: 1,
    total_ram_bytes: 16 * GIB,
    available_ram_bytes: 12 * GIB,
    logical_cpu_count: 8,
    platform: {
      os: "linux",
      arch: "x86_64",
      supported_runtimes: ["ollama", "llama_cpp"],
      accelerators: [{ kind: "cpu", available: true, device_names: [], total_memory_bytes: null, available_memory_bytes: null }],
    },
    ...overrides,
  };
}

const profile: HardwareProfile = {
  tier: "balanced",
  recommended_process_slots: 2,
  recommended_ram_reserve_bytes: 2 * GIB,
  preferred_accelerator: "cpu",
};

function compatibility(overrides: Partial<M3HardwareCompatibilityReport> = {}): M3HardwareCompatibilityReport {
  return {
    capturedAtMs: 1,
    os: "linux",
    arch: "x86_64",
    accelerators: [],
    jetson: { detected: false, model: null },
    hybridGraphicsDetected: false,
    notes: [],
    ...overrides,
  };
}

describe("resolveEdgeRuntimeProfile", () => {
  it("prioritizes confirmed Jetson evidence and never recommends desktop CUDA", () => {
    const result = resolveEdgeRuntimeProfile(
      snapshot(),
      profile,
      compatibility({ jetson: { detected: true, model: "Jetson AGX Orin" } }),
    );
    expect(result.kind).toBe("jetson");
    expect(result.confidence).toBe("confirmed");
    expect(result.requiredComponents.join(" ")).toContain("Jetson/L4T");
  });

  it("recognizes Apple Silicon from platform plus confirmed Metal", () => {
    const result = resolveEdgeRuntimeProfile(snapshot({
      total_ram_bytes: 32 * GIB,
      platform: {
        os: "macos",
        arch: "aarch64",
        supported_runtimes: ["ollama", "llama_cpp"],
        accelerators: [{ kind: "metal", available: true, device_names: ["Apple M3"], total_memory_bytes: 32 * GIB, available_memory_bytes: 20 * GIB }],
      },
    }), profile, compatibility({ os: "macos", arch: "aarch64" }));
    expect(result.kind).toBe("apple_silicon");
    expect(result.contextTokens).toBe(16384);
    expect(result.processSlots).toBeLessThanOrEqual(2);
  });

  it("fails over legacy CUDA to an honest CPU profile", () => {
    const result = resolveEdgeRuntimeProfile(snapshot(), profile, compatibility({
      accelerators: [{
        kind: "cuda",
        status: "driver_too_old",
        summary: "Driver 390 is below the supported floor.",
        deviceNames: ["GTX 750"],
        driverVersion: "390",
        computeCapability: "5.0",
        confirmed: true,
      }],
    }));
    expect(result.kind).toBe("legacy_cuda");
    expect(result.expectedSpeed).toContain("CPU");
    expect(result.fallbacks).toContain("CPU-only llama.cpp");
  });

  it("uses a bounded low-memory profile before generic mini-PC inference", () => {
    const result = resolveEdgeRuntimeProfile(snapshot({ total_ram_bytes: 4 * GIB }), profile, compatibility());
    expect(result.kind).toBe("low_memory_homelab");
    expect(result.contextTokens).toBe(2048);
    expect(result.processSlots).toBe(1);
  });

  it("uses an explicitly inferred Raspberry Pi-safe profile for low-power Linux ARM", () => {
    const result = resolveEdgeRuntimeProfile(snapshot({
      total_ram_bytes: 8 * GIB,
      platform: {
        os: "linux",
        arch: "aarch64",
        supported_runtimes: ["ollama", "llama_cpp"],
        accelerators: [{ kind: "cpu", available: true, device_names: [], total_memory_bytes: null, available_memory_bytes: null }],
      },
    }), profile, compatibility({ arch: "aarch64" }));
    expect(result.kind).toBe("raspberry_pi");
    expect(result.confidence).toBe("inferred");
    expect(result.processSlots).toBe(1);
  });
});
