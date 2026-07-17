import { beforeEach, describe, expect, it, vi } from "vitest";

const mocks = vi.hoisted(() => ({
  attemptStream: vi.fn(),
  invoke: vi.fn(),
}));

vi.mock("@tauri-apps/api/core", () => ({
  invoke: (...args: unknown[]) => mocks.invoke(...args),
  isTauri: () => false,
}));
vi.mock("@tauri-apps/api/event", () => ({ listen: vi.fn(async () => () => {}) }));
vi.mock("@tauri-apps/api/window", () => ({ getCurrentWindow: () => ({ label: "pm-copilot-test" }) }));
vi.mock("./turnEngine", () => ({
  attemptStream: (...args: unknown[]) => mocks.attemptStream(...args),
}));

import {
  clearPmCopilotControllersForTests,
  generatePmPlan,
  isPmPlanGenerating,
  parsePmPlanResponse,
  pmCopilotGenerationKey,
  pmPlanToMarkdown,
  savePmPlanToWorkspace,
  slugifyGoal,
  type PmPlan,
} from "./pmCopilot";
import { useModelStore, type ProviderConfig, type ProviderModelInfo } from "../store/modelStore";
import { useWorkspaceStore } from "../store/workspaceStore";

const PROVIDER: ProviderConfig = {
  id: "test-provider",
  label: "Test Provider",
  base_url: "https://provider.test/v1",
  is_custom: false,
  has_key: true,
};
const PROVIDER_MODEL: ProviderModelInfo = { id: "test-model" };

function seedActiveProviderTarget(): void {
  useModelStore.setState({
    installed: [],
    active: null,
    llamaStatus: "stopped",
    ollamaModels: [],
    ollamaReachable: false,
    providers: [PROVIDER],
    providerModels: { [PROVIDER.id]: [PROVIDER_MODEL] },
    effortByTarget: {},
    activeProvider: "provider",
    activeOllamaModel: null,
    activeProviderId: PROVIDER.id,
    activeProviderModel: PROVIDER_MODEL.id,
  });
}

function seedNoWorkspace(): void {
  useWorkspaceStore.setState({ roots: [], recent: [], rootsVersion: 0 });
}

function seedWorkspace(): void {
  useWorkspaceStore.setState({
    roots: [{ id: "root-1", path: "/workspace/project", label: "project", is_primary: true }],
    recent: [],
    rootsVersion: 0,
  });
}

const VALID_JSON = JSON.stringify({
  prdSummary: "Let users export their data as CSV so they can analyze it offline.",
  userStories: [
    { asA: "user", iWant: "to export my data", soThat: "I can analyze it offline" },
    { asA: "admin", iWant: "to audit exports", soThat: "I can track data leaving the system" },
  ],
  acceptanceCriteria: ["Export produces a valid CSV file", "Export includes all visible columns"],
  risks: [{ description: "Large exports may time out", severity: "medium", mitigation: "Stream the export" }],
  milestones: [{ name: "MVP export", summary: "Ship a basic CSV export button" }],
});

beforeEach(() => {
  clearPmCopilotControllersForTests();
  mocks.attemptStream.mockReset();
  mocks.invoke.mockReset();
  seedActiveProviderTarget();
  seedNoWorkspace();
});

describe("parsePmPlanResponse", () => {
  it("parses a well-formed JSON reply into a typed plan", () => {
    const plan = parsePmPlanResponse(VALID_JSON, "Export data as CSV");
    expect(plan).not.toBeNull();
    expect(plan?.goal).toBe("Export data as CSV");
    expect(plan?.userStories).toHaveLength(2);
    expect(plan?.risks[0]).toEqual({
      description: "Large exports may time out",
      severity: "medium",
      mitigation: "Stream the export",
    });
    expect(plan?.milestones[0]).toEqual({ name: "MVP export", summary: "Ship a basic CSV export button" });
  });

  it("extracts JSON embedded in surrounding prose or a code fence", () => {
    const wrapped = `Sure, here's the plan:\n\`\`\`json\n${VALID_JSON}\n\`\`\`\nLet me know if you need changes.`;
    const plan = parsePmPlanResponse(wrapped, "Export data as CSV");
    expect(plan).not.toBeNull();
    expect(plan?.prdSummary).toContain("export their data as CSV");
  });

  it("drops malformed entries from arrays instead of failing the whole plan", () => {
    const partiallyBad = JSON.stringify({
      prdSummary: "A usable summary.",
      userStories: [
        { asA: "user", iWant: "a thing", soThat: "a reason" },
        { asA: "user", iWant: "" }, // missing soThat -> dropped
        "not an object", // dropped
      ],
      acceptanceCriteria: ["ok criterion", "", 42],
      risks: [{ description: "risk", severity: "not-a-level", mitigation: "fix it" }],
      milestones: [{ name: "M1", summary: "first" }],
    });
    const plan = parsePmPlanResponse(partiallyBad, "Goal");
    expect(plan?.userStories).toHaveLength(1);
    expect(plan?.acceptanceCriteria).toEqual(["ok criterion"]);
    expect(plan?.risks).toHaveLength(0);
    expect(plan?.milestones).toHaveLength(1);
  });

  it("fails closed to null when nothing usable survives", () => {
    expect(parsePmPlanResponse("not json at all", "Goal")).toBeNull();
    expect(parsePmPlanResponse(JSON.stringify({ prdSummary: "", userStories: [] }), "Goal")).toBeNull();
  });
});

describe("slugifyGoal", () => {
  it("lowercases, hyphenates, and trims a normal goal", () => {
    expect(slugifyGoal("Export Data as CSV!")).toBe("export-data-as-csv");
  });

  it("falls back to a default slug for empty or symbol-only input", () => {
    expect(slugifyGoal("   ")).toBe("product-plan");
    expect(slugifyGoal("!!!")).toBe("product-plan");
  });

  it("never produces the reserved 'roadmap' slug", () => {
    expect(slugifyGoal("Roadmap")).toBe("product-plan");
    expect(slugifyGoal("  RoadMap  ")).toBe("product-plan");
  });

  it("caps length at 60 characters", () => {
    const long = "a".repeat(200);
    expect(slugifyGoal(long).length).toBeLessThanOrEqual(60);
  });
});

describe("pmPlanToMarkdown", () => {
  const plan: PmPlan = {
    goal: "Export data as CSV",
    prdSummary: "Let users export their data.",
    userStories: [{ asA: "user", iWant: "to export data", soThat: "I can analyze it" }],
    acceptanceCriteria: ["Export produces a valid file"],
    risks: [{ description: "Large exports time out", severity: "high", mitigation: "Stream it" }],
    milestones: [{ name: "MVP", summary: "Basic export" }],
  };

  it("renders every section with the plan's content", () => {
    const markdown = pmPlanToMarkdown(plan, 0, "Test Provider · test-model");
    expect(markdown).toContain("# Export data as CSV");
    expect(markdown).toContain("## PRD summary");
    expect(markdown).toContain("Let users export their data.");
    expect(markdown).toContain("As a user, I want to export data, so that I can analyze it.");
    expect(markdown).toContain("- [ ] Export produces a valid file");
    expect(markdown).toContain("| Large exports time out | high | Stream it |");
    expect(markdown).toContain("**MVP** — Basic export");
    expect(markdown).toContain("Test Provider · test-model");
    expect(markdown).toContain("GitHub/Jira/Linear");
  });

  it("shows an explicit empty-state placeholder for a section with nothing in it", () => {
    const emptyPlan: PmPlan = { ...plan, risks: [], milestones: [] };
    const markdown = pmPlanToMarkdown(emptyPlan, 0, "Test Provider · test-model");
    expect(markdown).toContain("_No risks provided._");
    expect(markdown).toContain("_No milestones provided._");
  });
});

describe("generatePmPlan", () => {
  it("generates a plan from the active model target on a tool-less, one-shot attempt", async () => {
    mocks.attemptStream.mockImplementation(async (...args: unknown[]) => {
      const tools = args[2] as unknown[];
      const useTools = args[7];
      expect(tools).toEqual([]);
      expect(useTools).toBe(false);
      return {
        content: VALID_JSON,
        toolCalls: [],
        streamError: null,
        contentStarted: true,
        usage: { promptTokens: 100, completionTokens: 50, totalTokens: 150 },
      };
    });

    const result = await generatePmPlan("draft-1", "Export data as CSV");
    expect(result.plan.userStories).toHaveLength(2);
    expect(result.target.kind).toBe("provider");
    expect(mocks.attemptStream).toHaveBeenCalledTimes(1);
  });

  it("rejects an empty goal without calling the model", async () => {
    await expect(generatePmPlan("draft-1", "   ")).rejects.toThrow(/goal/i);
    expect(mocks.attemptStream).not.toHaveBeenCalled();
  });

  it("rejects when the model returns a tool call instead of a plan", async () => {
    mocks.attemptStream.mockResolvedValue({
      content: "",
      toolCalls: [{ id: "1", type: "function", function: { name: "write_file", arguments: "{}" } }],
      streamError: null,
      contentStarted: true,
    });
    await expect(generatePmPlan("draft-1", "Export data as CSV")).rejects.toThrow(/tool call/i);
  });

  it("rejects when the model's reply can't be parsed into a usable plan", async () => {
    mocks.attemptStream.mockResolvedValue({
      content: "not usable json",
      toolCalls: [],
      streamError: null,
      contentStarted: true,
    });
    await expect(generatePmPlan("draft-1", "Export data as CSV")).rejects.toThrow(/didn't return a usable plan/i);
  });

  it("cancels an in-flight generation and rejects with AbortError", async () => {
    mocks.attemptStream.mockImplementation((...args: unknown[]) => {
      const signal = args[3] as AbortSignal;
      return new Promise((resolve) => {
        signal.addEventListener("abort", () => resolve({
          content: "",
          toolCalls: [],
          streamError: "aborted",
          contentStarted: false,
        }), { once: true });
      });
    });

    const key = pmCopilotGenerationKey("draft-1");
    const promise = generatePmPlan("draft-1", "Export data as CSV");
    await vi.waitFor(() => expect(isPmPlanGenerating(key)).toBe(true));
    const { cancelPmPlanGeneration } = await import("./pmCopilot");
    expect(cancelPmPlanGeneration(key)).toBe(true);
    await expect(promise).rejects.toMatchObject({ name: "AbortError" });
    expect(isPmPlanGenerating(key)).toBe(false);
  });

  it("rejects a second concurrent generation for the same draft", async () => {
    mocks.attemptStream.mockImplementation((...args: unknown[]) => {
      const signal = args[3] as AbortSignal;
      return new Promise((resolve) => {
        signal.addEventListener("abort", () => resolve({
          content: "",
          toolCalls: [],
          streamError: "aborted",
          contentStarted: false,
        }), { once: true });
      });
    });
    const first = generatePmPlan("draft-1", "Export data as CSV");
    await vi.waitFor(() => expect(isPmPlanGenerating(pmCopilotGenerationKey("draft-1"))).toBe(true));
    await expect(generatePmPlan("draft-1", "Export data as CSV")).rejects.toThrow(/already being generated/i);
    clearPmCopilotControllersForTests();
    await expect(first).rejects.toBeDefined();
  });
});

describe("savePmPlanToWorkspace", () => {
  it("rejects when no workspace folder is open", async () => {
    await expect(savePmPlanToWorkspace("# doc", "my-plan")).rejects.toThrow(/workspace/i);
    expect(mocks.invoke).not.toHaveBeenCalled();
  });

  it("rejects a slug with unsafe characters", async () => {
    seedWorkspace();
    await expect(savePmPlanToWorkspace("# doc", "not a slug!")).rejects.toThrow(/filename/i);
    expect(mocks.invoke).not.toHaveBeenCalled();
  });

  it("rejects the reserved 'roadmap' slug even with a workspace open", async () => {
    seedWorkspace();
    await expect(savePmPlanToWorkspace("# doc", "roadmap")).rejects.toThrow(/filename/i);
    expect(mocks.invoke).not.toHaveBeenCalled();
  });

  it("writes the markdown via the existing tool_write_file command at docs/product/<slug>.md", async () => {
    seedWorkspace();
    mocks.invoke.mockResolvedValue("Wrote 42 bytes to docs/product/my-plan.md");
    const path = await savePmPlanToWorkspace("# My plan", "my-plan");
    expect(path).toBe("docs/product/my-plan.md");
    expect(mocks.invoke).toHaveBeenCalledWith("tool_write_file", {
      path: "docs/product/my-plan.md",
      content: "# My plan",
    });
  });
});
