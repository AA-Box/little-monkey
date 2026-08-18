import { describe, expect, it } from "vitest";

import { buildTools, GENERATE_IMAGE_TOOL, READ_SKILL_RESOURCE_TOOL, SKILL_INVOKE_TOOL, TASK_TOOL, toolsForProfile, toolsForWorkspace, TOOLS } from "./tools";

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

describe("toolsForWorkspace", () => {
  it("removes workspace and subagent tools when no workspace is attached", () => {
    const tools = toolsForWorkspace([...TOOLS, TASK_TOOL], false);
    const names = tools.map((tool) => tool.function.name);
    expect(names).toContain("web_search");
    expect(names).not.toContain("read_file");
    expect(names).not.toContain("run_shell");
    expect(names).not.toContain("task");
  });

  it("preserves the normal tool list when a workspace is attached", () => {
    expect(toolsForWorkspace(TOOLS, true)).toBe(TOOLS);
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

  it("code profile is explore plus the mutating tools and run_shell's background companions", () => {
    expect(toolsForProfile("code").map((t) => t.function.name).sort()).toEqual([
      "edit_file",
      "glob",
      "grep",
      "list_dir",
      "read_file",
      "run_shell",
      "shell_kill",
      "shell_output",
      "write_file",
    ]);
  });

  it("never offers spawn_task to a delegated profile — a chip belongs to the user's own conversation", () => {
    expect(toolsForProfile("code").some((tool) => tool.function.name === "spawn_task")).toBe(false);
    expect(toolsForProfile("explore").some((tool) => tool.function.name === "spawn_task")).toBe(false);
  });
});

describe("GENERATE_IMAGE_TOOL", () => {
  it("is kept out of the base TOOLS array (appended by agentLoop.ts's runAgentTurnBody — see its doc comment on the wire-shape/monkey-cli asymmetry)", () => {
    expect(TOOLS).not.toContain(GENERATE_IMAGE_TOOL);
    expect(TOOLS.some((tool) => tool.function.name === "generate_image")).toBe(false);
  });

  it("is never offered to subagent profiles", () => {
    for (const profile of ["explore", "code"] as const) {
      expect(toolsForProfile(profile).some((tool) => tool.function.name === "generate_image")).toBe(false);
    }
  });

  it("requires both a suggested filename and svg", () => {
    const params = GENERATE_IMAGE_TOOL.function.parameters as { required: string[] };
    expect(params.required).toEqual(["filename", "svg"]);
  });

  it("tells the model that filenames are timestamped automatically", () => {
    const description = GENERATE_IMAGE_TOOL.function.description.toLowerCase();
    expect(description).toContain("timestamp");
    expect(description).toContain("never retry");
  });
});

describe("TASK_TOOL", () => {
  it("is kept out of the base TOOLS array (only offered when subagentsEnabled, via agentLoop.ts's toolsForSettings)", () => {
    expect(TOOLS).not.toContain(TASK_TOOL);
  });

  it("allows both the explore and code profiles as of slice 3", () => {
    const profileParam = (TASK_TOOL.function.parameters as { properties: { profile: { enum: string[] } } }).properties.profile;
    expect(profileParam.enum).toEqual(["explore", "code"]);
  });
});

describe("SKILL_INVOKE_TOOL", () => {
  it("is kept out of the base TOOLS array (only offered when skillAutoInvokeEnabled, via agentLoop.ts's toolsForSettings)", () => {
    expect(TOOLS).not.toContain(SKILL_INVOKE_TOOL);
  });

  it("requires only command, leaving arguments optional", () => {
    const params = SKILL_INVOKE_TOOL.function.parameters as { required: string[] };
    expect(params.required).toEqual(["command"]);
  });
});

describe("READ_SKILL_RESOURCE_TOOL", () => {
  it("is kept out of the base TOOLS array (appended by agentLoop.ts's toolsForSettings only when a resource file is available)", () => {
    expect(TOOLS).not.toContain(READ_SKILL_RESOURCE_TOOL);
  });

  it("requires both command and path", () => {
    const params = READ_SKILL_RESOURCE_TOOL.function.parameters as { required: string[] };
    expect(params.required).toEqual(["command", "path"]);
  });
});
