import { describe, expect, it } from "vitest";

import type { ModelTargetSnapshot } from "./modelTargets";
import { buildComparisonExecutionPlan, isComparisonExecutionPlan } from "./comparisonPlan";

function target(
  key: string,
  kind: "local" | "ollama" | "provider",
  estimatedMemoryBytes?: number,
  isCloud = false,
): ModelTargetSnapshot {
  const common = {
    key,
    label: kind,
    displayName: key,
    capabilities: {
      toolCalling: { state: "unknown" as const, evidence: "test" },
      vision: { state: "unknown" as const, evidence: "test" },
    },
    availability: { status: "available" as const, evidence: "test" },
    ...(estimatedMemoryBytes === undefined ? {} : { estimatedMemoryBytes }),
  };
  if (kind === "local") return { ...common, kind, modelId: key, modelPath: `/${key}.gguf` };
  if (kind === "ollama") {
    return { ...common, kind, baseUrl: "http://127.0.0.1:11434", model: key, isCloud };
  }
  return {
    ...common,
    kind,
    providerId: "provider",
    endpoint: "https://provider.test/v1",
    model: key,
    credentialRefId: "keychain:com.littlemonkey.app:provider",
  };
}

describe("buildComparisonExecutionPlan", () => {
  it("keeps provider, cloud Ollama, and a single local target concurrent", () => {
    const plan = buildComparisonExecutionPlan(
      [target("provider:a", "provider", 0), target("cloud", "ollama", 0, true), target("local", "local", 4)],
      { totalBytes: 32, availableBytes: 16 },
    );
    expect(plan).toMatchObject({ mode: "concurrent", localTargetKeys: ["local"], reason: "within_budget" });
    expect(isComparisonExecutionPlan(plan)).toBe(true);
  });

  it("queues local execution when the combined estimate exceeds the safe available-memory budget", () => {
    const plan = buildComparisonExecutionPlan(
      [target("ollama:a", "ollama", 8), target("ollama:b", "ollama", 7), target("remote", "provider", 0)],
      { totalBytes: 32, availableBytes: 16 },
    );
    expect(plan).toMatchObject({
      mode: "local_sequential",
      reason: "memory_pressure",
      estimatedLocalBytes: 15,
      budgetMemoryBytes: 12,
    });
  });

  it("queues multiple local targets when either model or system memory is unknown", () => {
    expect(
      buildComparisonExecutionPlan([target("a", "ollama"), target("b", "ollama", 4)], null),
    ).toMatchObject({ mode: "local_sequential", reason: "memory_unknown" });
  });

  it("allows multiple local targets concurrently when the estimate fits", () => {
    expect(
      buildComparisonExecutionPlan(
        [target("a", "ollama", 3), target("b", "ollama", 4)],
        { totalBytes: 32, availableBytes: 20 },
      ),
    ).toMatchObject({ mode: "concurrent", reason: "within_budget", estimatedLocalBytes: 7 });
  });
});

describe("isComparisonExecutionPlan", () => {
  const validPlan = {
    version: 1 as const,
    mode: "local_sequential" as const,
    strategy: "memory_queue" as const,
    localTargetKeys: ["ollama:a", "ollama:b"],
    branches: [
      {
        sessionId: "branch-a",
        targetKey: "ollama:a",
        mode: "queued" as const,
        queuePosition: 0,
        estimatedResidentBytes: 8,
      },
      {
        sessionId: "branch-b",
        targetKey: "ollama:b",
        mode: "queued" as const,
        queuePosition: 1,
        estimatedResidentBytes: 7,
      },
    ],
    estimatedLocalBytes: 15,
    availableMemoryBytes: 16,
    budgetMemoryBytes: 12,
    reason: "memory_pressure" as const,
    residentOllamaModels: ["already-running:latest"],
    cleanupWarnings: [],
  };

  it("accepts the complete persisted plan shape", () => {
    expect(isComparisonExecutionPlan(validPlan)).toBe(true);
  });

  it.each([
    ["unknown mode", { ...validPlan, mode: "burst" }],
    ["unknown schema version", { ...validPlan, version: 2 }],
    ["inconsistent strategy", { ...validPlan, strategy: "concurrent" }],
    ["non-string local key", { ...validPlan, localTargetKeys: [1] }],
    ["negative estimate", { ...validPlan, estimatedLocalBytes: -1 }],
    ["non-finite memory", { ...validPlan, availableMemoryBytes: Number.POSITIVE_INFINITY }],
    ["missing budget", { ...validPlan, budgetMemoryBytes: undefined }],
    ["unknown reason", { ...validPlan, reason: "manual" }],
  ])("rejects a plan with %s", (_label, candidate) => {
    expect(isComparisonExecutionPlan(candidate)).toBe(false);
  });
});
