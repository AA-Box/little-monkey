import { describe, expect, it } from "vitest";

import type { M3CompatibilityMatrixRow } from "../../../lib/runtimeHubClient";
import { groupByRuntime } from "./RuntimeHubCompatibilityMatrix";

function row(overrides: Partial<M3CompatibilityMatrixRow> = {}): M3CompatibilityMatrixRow {
  return {
    method: "POST",
    route: "/v1/chat/completions",
    backend: "ollama",
    runtimeId: "ollama-local",
    modelId: null,
    status: "pass",
    reason: "runtime driver supports inference",
    ...overrides,
  };
}

describe("groupByRuntime", () => {
  it("groups rows by runtimeId while preserving row order within a group", () => {
    const rows = [
      row({ runtimeId: "ollama-local", route: "/v1/models" }),
      row({ runtimeId: "managed-llama", route: "/v1/models", backend: "managed_local" }),
      row({ runtimeId: "ollama-local", route: "/v1/embeddings" }),
    ];
    const groups = groupByRuntime(rows);
    expect([...groups.keys()]).toEqual(["ollama-local", "managed-llama"]);
    expect(groups.get("ollama-local")?.map((entry) => entry.route)).toEqual([
      "/v1/models",
      "/v1/embeddings",
    ]);
    expect(groups.get("managed-llama")?.map((entry) => entry.route)).toEqual(["/v1/models"]);
  });

  it("returns an empty map for no rows", () => {
    expect(groupByRuntime([]).size).toBe(0);
  });
});
