import { describe, expect, it } from "vitest";
import type {
  BenchmarkHistoryEntry,
  BenchmarkReport,
  HardwareProfile,
  HardwareSnapshot,
  M3HardwareCompatibilityReport,
} from "./runtimeHubClient";
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
        execution: { state: "detectionOnly", reason: "fixture" },
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

  describe("measured throughput", () => {
    function entry(
      overrides: {
        median?: number;
        n?: number;
        model?: string;
        decodeTokensPerSecond?: BenchmarkReport["decodeTokensPerSecond"];
        freshness?: BenchmarkHistoryEntry["freshness"];
      } = {},
    ): BenchmarkHistoryEntry {
      const median = overrides.median ?? 42;
      return {
        report: {
          schemaVersion: 1,
          runtimeId: "llama-local",
          model: overrides.model ?? "qwen3-4b",
          quantization: null,
          maxOutputTokens: 128,
          repeatsRequested: 5,
          warmupDiscarded: 1,
          samples: [],
          timeToFirstTokenMs: null,
          decodeTokensPerSecond:
            overrides.decodeTokensPerSecond !== undefined
              ? overrides.decodeTokensPerSecond
              : { median, min: median, max: median, stddev: null, n: overrides.n ?? 4 },
          peakMemory: {
            processLifetimePeakBytes: null,
            beforeBytes: null,
            runPeakBytes: null,
            unavailable: [],
          },
          unavailable: [],
        },
        machine: {
          os: "linux",
          arch: "x86_64",
          totalRamBytes: 16 * GIB,
          logicalCpuCount: 8,
          accelerators: [],
        },
        measuredAtMs: 1,
        freshness: overrides.freshness ?? { state: "thisMachine" },
      };
    }

    it("keeps deferring to the benchmark when nothing has been measured here", () => {
      const result = resolveEdgeRuntimeProfile(snapshot(), profile, compatibility(), []);
      expect(result.expectedSpeed).not.toContain("Measured here");
    });

    it("reports the number once this machine has one", () => {
      const result = resolveEdgeRuntimeProfile(snapshot(), profile, compatibility(), [
        entry({ median: 37.5, n: 4, model: "qwen3-4b" }),
      ]);
      expect(result.expectedSpeed).toContain("Measured here: 37.5 tok/s");
      expect(result.expectedSpeed).toContain("qwen3-4b");
      expect(result.expectedSpeed).toContain("n=4");
      expect(result.evidence).toContain("Benchmarked on this machine: 1 model");
    });

    it("reports the fastest measured pair, not the most recent one", () => {
      const result = resolveEdgeRuntimeProfile(snapshot(), profile, compatibility(), [
        entry({ median: 12, model: "slow-model" }),
        entry({ median: 90, model: "fast-model" }),
      ]);
      expect(result.expectedSpeed).toContain("90 tok/s");
      expect(result.expectedSpeed).toContain("fast-model");
    });

    /** The whole claim this surface makes is "measured on the machine displaying it". */
    it("ignores a report measured on different hardware", () => {
      const result = resolveEdgeRuntimeProfile(snapshot(), profile, compatibility(), [
        entry({
          median: 900,
          freshness: { state: "differentMachine", changed: ["installed RAM 64 → 16 bytes"] },
        }),
      ]);
      expect(result.expectedSpeed).not.toContain("900");
      expect(result.expectedSpeed).not.toContain("Measured here");
    });

    it("ignores a report that produced no decode rate", () => {
      const result = resolveEdgeRuntimeProfile(snapshot(), profile, compatibility(), [
        entry({ decodeTokensPerSecond: null }),
      ]);
      expect(result.expectedSpeed).not.toContain("Measured here");
    });
  });
});
