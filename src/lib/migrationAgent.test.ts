import { beforeEach, describe, expect, it, vi } from "vitest";

const mocks = vi.hoisted(() => ({
  invoke: vi.fn(),
  resolveTarget: vi.fn(),
  effortForTarget: vi.fn(),
  attemptStream: vi.fn(),
}));

vi.mock("@tauri-apps/api/core", () => ({
  invoke: (...args: unknown[]) => mocks.invoke(...args),
}));

vi.mock("./agentLoop", () => ({
  resolveTarget: (...args: unknown[]) => mocks.resolveTarget(...args),
}));

vi.mock("../store/modelStore", () => ({
  effortForTarget: (...args: unknown[]) => mocks.effortForTarget(...args),
}));

vi.mock("./turnEngine", () => ({
  attemptStream: (...args: unknown[]) => mocks.attemptStream(...args),
}));

import {
  buildMigrationPlanMessages,
  fallbackHeuristicPlan,
  generateMigrationPlan,
  parseMigrationPlanJson,
  readManifestExcerpts,
} from "./migrationAgent";

beforeEach(() => {
  mocks.invoke.mockReset();
  mocks.resolveTarget.mockReset();
  mocks.effortForTarget.mockReset();
  mocks.attemptStream.mockReset();
  mocks.resolveTarget.mockResolvedValue({ kind: "local", baseUrl: "http://127.0.0.1:8080", modelLabel: "Local" });
  mocks.effortForTarget.mockReturnValue(undefined);
});

describe("readManifestExcerpts", () => {
  it("reads both manifests when present", async () => {
    mocks.invoke.mockImplementation(async (_cmd: string, args: { path: string }) => {
      if (args.path === "package.json") return '{"name":"app"}';
      if (args.path === "Cargo.toml") return "[package]\nname = \"app\"";
      throw new Error("unexpected path");
    });
    const result = await readManifestExcerpts();
    expect(result.packageJson).toContain("app");
    expect(result.cargoToml).toContain("package");
  });

  it("resolves to null for a missing manifest instead of throwing", async () => {
    mocks.invoke.mockImplementation(async (_cmd: string, args: { path: string }) => {
      if (args.path === "package.json") return '{"name":"app"}';
      throw new Error("not found");
    });
    const result = await readManifestExcerpts();
    expect(result.packageJson).toContain("app");
    expect(result.cargoToml).toBeNull();
  });
});

describe("buildMigrationPlanMessages", () => {
  it("includes the goal and available manifest context", () => {
    const messages = buildMigrationPlanMessages("Upgrade React to v19", {
      packageJson: '{"dependencies":{"react":"18.0.0"}}',
      cargoToml: null,
    });
    expect(messages).toHaveLength(2);
    expect(messages[0].role).toBe("system");
    expect(messages[1].role).toBe("user");
    expect(String(messages[1].content)).toContain("Upgrade React to v19");
    expect(String(messages[1].content)).toContain("react");
  });

  it("notes when no manifest could be read", () => {
    const messages = buildMigrationPlanMessages("Upgrade React to v19", { packageJson: null, cargoToml: null });
    expect(String(messages[1].content)).toContain("No package.json or Cargo.toml");
  });
});

describe("parseMigrationPlanJson", () => {
  it("parses a well-formed response", () => {
    const raw = JSON.stringify({
      summary: "Bump React across two slices",
      slices: [
        {
          title: "Bump the dependency",
          description: "Update package.json to react@19",
          riskLevel: "medium",
          riskNotes: ["Peer dependency conflicts possible"],
          rollbackNotes: "Revert package.json",
          filesLikely: ["package.json"],
        },
      ],
    });
    const plan = parseMigrationPlanJson(raw, "Upgrade React to v19");
    expect(plan.usedFallback).toBe(false);
    expect(plan.goal).toBe("Upgrade React to v19");
    expect(plan.summary).toBe("Bump React across two slices");
    expect(plan.slices).toHaveLength(1);
    expect(plan.slices[0]).toMatchObject({
      id: "slice-1",
      order: 1,
      title: "Bump the dependency",
      riskLevel: "medium",
    });
  });

  it("strips ```json fences before parsing", () => {
    const raw = "```json\n" + JSON.stringify({ summary: "s", slices: [{ title: "A" }] }) + "\n```";
    const plan = parseMigrationPlanJson(raw, "goal");
    expect(plan.slices).toHaveLength(1);
  });

  it("coerces an invalid riskLevel to medium and fills in defaults", () => {
    const raw = JSON.stringify({ slices: [{ title: "A" }] });
    const plan = parseMigrationPlanJson(raw, "goal");
    expect(plan.slices[0].riskLevel).toBe("medium");
    expect(plan.slices[0].riskNotes).toEqual([]);
    expect(plan.slices[0].rollbackNotes).toContain("Revert");
  });

  it("caps slices at MAX_MIGRATION_SLICES", () => {
    const raw = JSON.stringify({
      summary: "s",
      slices: Array.from({ length: 10 }, (_, i) => ({ title: `Slice ${i}` })),
    });
    const plan = parseMigrationPlanJson(raw, "goal");
    expect(plan.slices.length).toBeLessThanOrEqual(6);
  });

  it("throws on empty content", () => {
    expect(() => parseMigrationPlanJson("", "goal")).toThrow();
  });

  it("throws on invalid JSON", () => {
    expect(() => parseMigrationPlanJson("not json", "goal")).toThrow();
  });

  it("throws when slices is missing or empty", () => {
    expect(() => parseMigrationPlanJson(JSON.stringify({ summary: "s", slices: [] }), "goal")).toThrow();
  });
});

describe("fallbackHeuristicPlan", () => {
  it("always returns a usable multi-slice plan", () => {
    const plan = fallbackHeuristicPlan("Upgrade React to v19");
    expect(plan.usedFallback).toBe(true);
    expect(plan.slices.length).toBeGreaterThan(0);
    for (const slice of plan.slices) {
      expect(slice.description.length).toBeGreaterThan(0);
      expect(slice.rollbackNotes.length).toBeGreaterThan(0);
    }
  });
});

describe("generateMigrationPlan", () => {
  it("rejects an empty goal", async () => {
    await expect(generateMigrationPlan("   ")).rejects.toThrow();
  });

  it("parses a valid model response into a plan", async () => {
    mocks.invoke.mockRejectedValue(new Error("no file"));
    mocks.attemptStream.mockResolvedValue({
      content: JSON.stringify({ summary: "s", slices: [{ title: "A" }] }),
      toolCalls: [],
      streamError: null,
      contentStarted: true,
    });
    const plan = await generateMigrationPlan("Upgrade React to v19");
    expect(plan.usedFallback).toBe(false);
    expect(plan.slices).toHaveLength(1);
  });

  it("falls back to the heuristic plan when the model response can't be parsed", async () => {
    mocks.invoke.mockRejectedValue(new Error("no file"));
    mocks.attemptStream.mockResolvedValue({
      content: "not json at all",
      toolCalls: [],
      streamError: null,
      contentStarted: true,
    });
    const plan = await generateMigrationPlan("Upgrade React to v19");
    expect(plan.usedFallback).toBe(true);
  });

  it("throws when the stream itself errors", async () => {
    mocks.invoke.mockRejectedValue(new Error("no file"));
    mocks.attemptStream.mockResolvedValue({
      content: "",
      toolCalls: [],
      streamError: "No model available",
      contentStarted: false,
    });
    await expect(generateMigrationPlan("Upgrade React to v19")).rejects.toThrow("No model available");
  });
});
