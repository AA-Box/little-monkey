import { describe, expect, it } from "vitest";

import { buildTools, TASK_TOOL, toolsForProfile, TOOLS } from "./tools";

describe("buildTools", () => {
  it("returns the base TOOLS list unchanged when no stacks are attached", () => {
    const tools = buildTools([]);
    expect(tools).toBe(TOOLS);
    expect(tools.some((tool) => tool.function.name === "search_docs")).toBe(false);
  });

  it("appends a search_docs tool when at least one stack is attached", () => {
    const tools = buildTools(["Docs"]);
    expect(tools).toHaveLength(TOOLS.length + 1);
    const searchDocs = tools.find((tool) => tool.function.name === "search_docs");
    expect(searchDocs).toBeDefined();
  });

  it("embeds the actual attached stack names in the tool's description", () => {
    const tools = buildTools(["Docs", "Release Notes"]);
    const searchDocs = tools.find((tool) => tool.function.name === "search_docs");
    expect(searchDocs?.function.description).toContain("Docs");
    expect(searchDocs?.function.description).toContain("Release Notes");
  });

  it("never mutates the base TOOLS array", () => {
    const before = TOOLS.length;
    buildTools(["Docs"]);
    expect(TOOLS).toHaveLength(before);
    expect(TOOLS.some((tool) => tool.function.name === "search_docs")).toBe(false);
  });
});

// The depth-cap-of-1 invariant for subagents (docs/roadmap/p3-subagents.md):
// a subagent can never spawn another subagent because `task` is never among
// the tools its own loop is offered. Enforced by construction — `TOOLS`
// (which `toolsForProfile` filters down from) never contains `task` at all,
// and `TASK_TOOL` is a separate constant never folded into `TOOLS` — but
// pinned here explicitly so a future refactor that merges the two arrays
// would fail this test instead of silently reintroducing recursion.
describe("toolsForProfile", () => {
  it("never includes the task tool for the explore profile", () => {
    const tools = toolsForProfile("explore");
    expect(tools.some((tool) => tool.function.name === "task")).toBe(false);
  });

  it("never includes the task tool for the code profile", () => {
    const tools = toolsForProfile("code");
    expect(tools.some((tool) => tool.function.name === "task")).toBe(false);
  });

  it("the base TOOLS array itself never contains the task tool", () => {
    // Belt-and-suspenders: even if `toolsForProfile`'s own filter logic were
    // ever broken, there is still no `task` entry in `TOOLS` for it to let
    // through.
    expect(TOOLS.some((tool) => tool.function.name === "task")).toBe(false);
  });

  it("explore profile is exactly the four read-only tools", () => {
    expect(toolsForProfile("explore").map((t) => t.function.name).sort()).toEqual(["glob", "grep", "list_dir", "read_file"]);
  });

  it("code profile is explore plus the three mutating tools", () => {
    expect(toolsForProfile("code").map((t) => t.function.name).sort()).toEqual([
      "edit_file",
      "glob",
      "grep",
      "list_dir",
      "read_file",
      "run_shell",
      "write_file",
    ]);
  });
});

describe("TASK_TOOL", () => {
  it("is kept out of the base TOOLS array (only offered when subagentsEnabled, via agentLoop.ts's toolsForSettings)", () => {
    expect(TOOLS).not.toContain(TASK_TOOL);
  });

  it("only allows the explore profile in this slice", () => {
    const profileParam = (TASK_TOOL.function.parameters as { properties: { profile: { enum: string[] } } }).properties.profile;
    expect(profileParam.enum).toEqual(["explore"]);
  });
});
