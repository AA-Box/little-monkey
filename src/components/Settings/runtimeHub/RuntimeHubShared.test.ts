import { describe, expect, it } from "vitest";

import type { M3HardwareCompatibilityReport } from "../../../lib/runtimeHubClient";
import { riskyAccelerators } from "./RuntimeHubShared";

function reportWith(
  accelerators: M3HardwareCompatibilityReport["accelerators"],
): M3HardwareCompatibilityReport {
  return {
    capturedAtMs: 1,
    os: "macos",
    arch: "aarch64",
    accelerators,
    jetson: { detected: false, model: null },
    hybridGraphicsDetected: false,
    notes: [],
  };
}

describe("riskyAccelerators", () => {
  it("excludes available and not_detected backends", () => {
    const report = reportWith([
      {
        kind: "metal",
        status: "available",
        summary: "Metal is available.",
        deviceNames: ["Apple GPU"],
        driverVersion: null,
        computeCapability: null,
        confirmed: true,
      },
      {
        kind: "cuda",
        status: "not_detected",
        summary: "nvidia-smi ran but reported no NVIDIA GPU; falls back to CPU.",
        deviceNames: [],
        driverVersion: null,
        computeCapability: null,
        confirmed: true,
      },
    ]);
    expect(riskyAccelerators(report)).toEqual([]);
  });

  it("surfaces driver_too_old, tool_missing, and unsupported backends", () => {
    const report = reportWith([
      {
        kind: "cuda",
        status: "driver_too_old",
        summary: "NVIDIA GPU detected, but the driver version is below what this app expects.",
        deviceNames: ["NVIDIA RTX 2060"],
        driverVersion: "410.10",
        computeCapability: "7.5",
        confirmed: true,
      },
      {
        kind: "rocm",
        status: "tool_missing",
        summary: "rocm-smi was not found on PATH.",
        deviceNames: [],
        driverVersion: null,
        computeCapability: null,
        confirmed: true,
      },
      {
        kind: "direct_ml",
        status: "unsupported",
        summary: "DirectML is a Windows-only backend.",
        deviceNames: [],
        driverVersion: null,
        computeCapability: null,
        confirmed: true,
      },
    ]);
    expect(riskyAccelerators(report).map((accelerator) => accelerator.kind)).toEqual([
      "cuda",
      "rocm",
      "direct_ml",
    ]);
  });
});
