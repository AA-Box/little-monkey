import { beforeEach, describe, expect, it, vi } from "vitest";

import type { WorkspaceRootInfo } from "../store/workspaceStore";

const mocks = vi.hoisted(() => ({
  resolveTarget: vi.fn(),
  attemptStream: vi.fn(),
  effortForTarget: vi.fn(),
}));

vi.mock("@tauri-apps/api/core", () => ({
  invoke: vi.fn(),
  isTauri: () => false,
}));
vi.mock("./agentLoop", () => ({
  resolveTarget: (...args: unknown[]) => mocks.resolveTarget(...args),
}));
vi.mock("./turnEngine", () => ({
  attemptStream: (...args: unknown[]) => mocks.attemptStream(...args),
}));
vi.mock("../store/modelStore", () => ({
  effortForTarget: (...args: unknown[]) => mocks.effortForTarget(...args),
}));

import {
  MAX_CROSS_REPO_STEPS,
  buildPlanningMessages,
  generateCrossRepoPlan,
  parsePlanEnvelope,
} from "./crossRepoChangePlanner";

const ROOTS: WorkspaceRootInfo[] = [
  { id: "root-api", path: "/work/api", label: "api", is_primary: true },
  { id: "root-web", path: "/work/web", label: "web", is_primary: false },
  { id: "root-docs", path: "/work/docs", label: "docs", is_primary: false },
];

function envelope(overrides: Partial<Record<string, unknown>> = {}): string {
  return JSON.stringify({
    notes: "Ship the API first so the client has something to call.",
    steps: [
      {
        rootId: "root-web",
        order: 2,
        summary: "Update client to call the new endpoint.",
        changes: "Point the fetch call at /v2/widgets.",
        risks: "Old clients break if the API ships without a compat shim.",
        rollback: "Revert the fetch URL change.",
        dependsOnRootIds: ["root-api"],
      },
      {
        rootId: "root-api",
        order: 1,
        summary: "Add the new /v2/widgets endpoint.",
        changes: "New handler, additive route.",
        risks: "None, purely additive.",
        rollback: "Delete the route.",
        dependsOnRootIds: [],
      },
    ],
    ...overrides,
  });
}

describe("crossRepoChangePlanner", () => {
  describe("buildPlanningMessages", () => {
    it("includes the description and every root's id/label/path", () => {
      const [system, user] = buildPlanningMessages("Rename the widget field", ROOTS);
      expect(system.role).toBe("system");
      expect(user.role).toBe("user");
      expect(user.content).toContain("Rename the widget field");
      for (const root of ROOTS) {
        expect(user.content).toContain(root.id);
        expect(user.content).toContain(root.label);
        expect(user.content).toContain(root.path);
      }
    });
  });

  describe("parsePlanEnvelope", () => {
    it("parses, sorts by order, and renumbers contiguously from 1", () => {
      const { notes, steps } = parsePlanEnvelope(envelope(), ROOTS);
      expect(notes).toContain("Ship the API first");
      expect(steps.map((s) => s.rootId)).toEqual(["root-api", "root-web"]);
      expect(steps.map((s) => s.order)).toEqual([1, 2]);
      expect(steps[1].dependsOnRootIds).toEqual(["root-api"]);
    });

    it("strips a ```json fence before parsing", () => {
      const fenced = "```json\n" + envelope() + "\n```";
      const { steps } = parsePlanEnvelope(fenced, ROOTS);
      expect(steps).toHaveLength(2);
    });

    it("throws on unparseable JSON", () => {
      expect(() => parsePlanEnvelope("not json at all", ROOTS)).toThrow(/JSON plan envelope/);
    });

    it("throws when a step references an unknown root id", () => {
      const raw = JSON.stringify({
        notes: "",
        steps: [{ rootId: "root-does-not-exist", summary: "x", changes: "", risks: "", rollback: "" }],
      });
      expect(() => parsePlanEnvelope(raw, ROOTS)).toThrow(/unknown root id/);
    });

    it("throws when the same root appears in more than one step", () => {
      const raw = JSON.stringify({
        notes: "",
        steps: [
          { rootId: "root-api", order: 1, summary: "a", changes: "", risks: "", rollback: "" },
          { rootId: "root-api", order: 2, summary: "b", changes: "", risks: "", rollback: "" },
        ],
      });
      expect(() => parsePlanEnvelope(raw, ROOTS)).toThrow(/more than one step/);
    });

    it("throws when a step is missing a summary", () => {
      const raw = JSON.stringify({
        notes: "",
        steps: [{ rootId: "root-api", order: 1, summary: "", changes: "", risks: "", rollback: "" }],
      });
      expect(() => parsePlanEnvelope(raw, ROOTS)).toThrow(/missing a summary/);
    });

    it("throws when there are no steps", () => {
      const raw = JSON.stringify({ notes: "", steps: [] });
      expect(() => parsePlanEnvelope(raw, ROOTS)).toThrow(/did not include any steps/);
    });

    it("throws when the step count exceeds the safety cap", () => {
      const manyRoots: WorkspaceRootInfo[] = Array.from({ length: MAX_CROSS_REPO_STEPS + 1 }, (_, i) => ({
        id: `root-${i}`,
        path: `/work/${i}`,
        label: `pkg-${i}`,
        is_primary: i === 0,
      }));
      const raw = JSON.stringify({
        notes: "",
        steps: manyRoots.map((root, i) => ({
          rootId: root.id,
          order: i + 1,
          summary: `change ${i}`,
          changes: "",
          risks: "",
          rollback: "",
        })),
      });
      expect(() => parsePlanEnvelope(raw, manyRoots)).toThrow(/step limit/);
    });

    it("drops a dependsOnRootIds entry that references itself or an unknown root", () => {
      const raw = JSON.stringify({
        notes: "",
        steps: [
          {
            rootId: "root-api",
            order: 1,
            summary: "x",
            changes: "",
            risks: "",
            rollback: "",
            dependsOnRootIds: ["root-api", "root-unknown", "root-web"],
          },
          { rootId: "root-web", order: 2, summary: "y", changes: "", risks: "", rollback: "" },
        ],
      });
      const { steps } = parsePlanEnvelope(raw, ROOTS);
      expect(steps[0].dependsOnRootIds).toEqual(["root-web"]);
    });
  });

  describe("generateCrossRepoPlan", () => {
    beforeEach(() => {
      mocks.resolveTarget.mockReset();
      mocks.attemptStream.mockReset();
      mocks.effortForTarget.mockReset();
    });

    it("rejects an empty description without calling the model", async () => {
      await expect(generateCrossRepoPlan("   ", ROOTS)).rejects.toThrow(/Describe the change/);
      expect(mocks.resolveTarget).not.toHaveBeenCalled();
    });

    it("rejects when there are no workspace roots", async () => {
      await expect(generateCrossRepoPlan("Rename a field", [])).rejects.toThrow(/Attach at least one/);
    });

    it("calls attemptStream with no tools and returns a normalized plan", async () => {
      mocks.resolveTarget.mockResolvedValue({ kind: "local", baseUrl: "http://127.0.0.1:1", modelLabel: "local" });
      mocks.effortForTarget.mockReturnValue(undefined);
      mocks.attemptStream.mockResolvedValue({
        content: envelope(),
        toolCalls: [],
        streamError: null,
        contentStarted: true,
      });

      const plan = await generateCrossRepoPlan("Add a new field end to end", ROOTS);

      expect(mocks.attemptStream).toHaveBeenCalledTimes(1);
      const [, , tools, , , , , recordUsage] = mocks.attemptStream.mock.calls[0];
      expect(tools).toEqual([]);
      expect(recordUsage).toBe(false);

      expect(plan.description).toBe("Add a new field end to end");
      expect(plan.steps).toHaveLength(2);
      expect(plan.steps.every((step) => typeof step.stepId === "string" && step.stepId.length > 0)).toBe(true);
      expect(plan.steps.map((s) => s.rootId)).toEqual(["root-api", "root-web"]);
    });

    it("surfaces a stream error instead of trying to parse content", async () => {
      mocks.resolveTarget.mockResolvedValue({ kind: "local", baseUrl: "http://127.0.0.1:1", modelLabel: "local" });
      mocks.effortForTarget.mockReturnValue(undefined);
      mocks.attemptStream.mockResolvedValue({
        content: "",
        toolCalls: [],
        streamError: "provider unreachable",
        contentStarted: false,
      });

      await expect(generateCrossRepoPlan("Add a new field", ROOTS)).rejects.toThrow(/provider unreachable/);
    });
  });
});
