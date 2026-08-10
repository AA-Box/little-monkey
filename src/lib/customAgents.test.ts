import { describe, expect, it } from "vitest";
import {
  CUSTOM_AGENT_TOOL_CEILING,
  collectCustomAgents,
  composeCustomAgentCatalog,
  customAgentBaseProfile,
  parseCustomAgentFile,
  splitFrontmatter,
  toolsForCustomAgent,
  type CustomAgentDef,
} from "./customAgents";
import { TOOLS, TASK_TOOL, WORKFLOW_TOOL } from "./tools";

const PATH = ".monkey/agents/test.md";

function def(overrides: Partial<CustomAgentDef> = {}): CustomAgentDef {
  return {
    name: "docs-writer",
    description: "Writes docs",
    tools: ["read_file", "grep"],
    addendum: "",
    sourcePath: PATH,
    ...overrides,
  };
}

function file(frontmatter: string, body = "Do the thing."): string {
  return `---\n${frontmatter}\n---\n${body}`;
}

describe("CUSTOM_AGENT_TOOL_CEILING", () => {
  it("every ceiling entry is an actual TOOLS member — a phantom name can never be granted", () => {
    const toolNames = new Set(TOOLS.map((tool) => tool.function.name));
    for (const name of CUSTOM_AGENT_TOOL_CEILING) {
      expect(toolNames.has(name), `${name} missing from TOOLS`).toBe(true);
    }
  });

  it("never contains task or workflow — the structural depth cap survives custom agents", () => {
    expect(CUSTOM_AGENT_TOOL_CEILING.has(TASK_TOOL.function.name)).toBe(false);
    expect(CUSTOM_AGENT_TOOL_CEILING.has(WORKFLOW_TOOL.function.name)).toBe(false);
  });

  it("never contains the parent-conversation-only tools", () => {
    expect(CUSTOM_AGENT_TOOL_CEILING.has("remember")).toBe(false);
    expect(CUSTOM_AGENT_TOOL_CEILING.has("spawn_task")).toBe(false);
    expect(CUSTOM_AGENT_TOOL_CEILING.has("skill")).toBe(false);
  });
});

describe("splitFrontmatter", () => {
  it("parses scalar fields and the body", () => {
    const parsed = splitFrontmatter(file("name: a\ndescription: b\ntools: read_file", "Body here."));
    expect(parsed?.fields.get("name")).toBe("a");
    expect(parsed?.body).toBe("Body here.");
  });

  it("parses a dash-list tools block", () => {
    const parsed = splitFrontmatter(file("name: a\ntools:\n  - read_file\n  - grep"));
    expect(parsed?.fields.get("tools")).toEqual(["read_file", "grep"]);
  });

  it("returns null without a leading fence", () => {
    expect(splitFrontmatter("name: a\n---\nbody")).toBeNull();
  });

  it("returns null when the fence never closes", () => {
    expect(splitFrontmatter("---\nname: a\nbody without closing fence")).toBeNull();
  });

  it("returns null on an unparseable frontmatter line", () => {
    expect(splitFrontmatter(file("name: a\n!!! not yaml"))).toBeNull();
  });
});

describe("parseCustomAgentFile", () => {
  it("accepts a valid definition with an inline comma tool list", () => {
    const result = parseCustomAgentFile(PATH, file("name: docs-writer\ndescription: Writes docs\ntools: read_file, grep\neffort: low"));
    expect(result.ok).toBe(true);
    if (result.ok) {
      expect(result.def).toEqual({
        name: "docs-writer",
        description: "Writes docs",
        tools: ["read_file", "grep"],
        effort: "low",
        addendum: "Do the thing.",
        sourcePath: PATH,
      });
    }
  });

  it("accepts a dash-list tools block and dedupes repeats", () => {
    const result = parseCustomAgentFile(PATH, file("name: a\ndescription: d\ntools:\n  - read_file\n  - read_file\n  - glob"));
    expect(result.ok && result.def.tools).toEqual(["read_file", "glob"]);
  });

  it("fails without frontmatter", () => {
    const result = parseCustomAgentFile(PATH, "just a markdown file");
    expect(!result.ok && result.error.message).toMatch(/frontmatter/i);
  });

  it.each(["name", "description", "tools"])("fails when %s is missing", (field) => {
    const fields: Record<string, string> = { name: "name: a", description: "description: d", tools: "tools: read_file" };
    delete fields[field];
    const result = parseCustomAgentFile(PATH, file(Object.values(fields).join("\n")));
    expect(!result.ok && result.error.message).toContain(field);
  });

  it.each(["UPPER", "has space", "-leading", "x".repeat(33)])("rejects invalid name %s", (name) => {
    const result = parseCustomAgentFile(PATH, file(`name: ${name}\ndescription: d\ntools: read_file`));
    expect(result.ok).toBe(false);
  });

  it.each(["explore", "code", "task", "workflow"])("rejects the reserved name %s", (name) => {
    const result = parseCustomAgentFile(PATH, file(`name: ${name}\ndescription: d\ntools: read_file`));
    expect(!result.ok && result.error.message).toContain("reserved");
  });

  it.each(["task", "workflow"])("rejects %s in the tool list with the depth-cap message", (tool) => {
    const result = parseCustomAgentFile(PATH, file(`name: a\ndescription: d\ntools: read_file, ${tool}`));
    expect(!result.ok && result.error.message).toContain("depth");
  });

  it("rejects a tool outside the ceiling, naming the allowed set", () => {
    const result = parseCustomAgentFile(PATH, file("name: a\ndescription: d\ntools: read_file, remember"));
    expect(!result.ok && result.error.message).toContain('"remember"');
    expect(!result.ok && result.error.message).toContain("read_file");
  });

  it("rejects an unknown tool name entirely", () => {
    const result = parseCustomAgentFile(PATH, file("name: a\ndescription: d\ntools: read_file, delete_everything"));
    expect(result.ok).toBe(false);
  });

  it("rejects an invalid effort", () => {
    const result = parseCustomAgentFile(PATH, file("name: a\ndescription: d\ntools: read_file\neffort: max"));
    expect(!result.ok && result.error.message).toContain("max");
  });

  it("an empty body is a valid (empty) addendum", () => {
    const result = parseCustomAgentFile(PATH, `---\nname: a\ndescription: d\ntools: read_file\n---\n`);
    expect(result.ok && result.def.addendum).toBe("");
  });
});

describe("collectCustomAgents", () => {
  it("keeps the first def on a duplicate name and errors the later file", () => {
    const first = parseCustomAgentFile("a.md", file("name: dup\ndescription: one\ntools: read_file"));
    const second = parseCustomAgentFile("b.md", file("name: dup\ndescription: two\ntools: grep"));
    const { defs, errors } = collectCustomAgents([first, second]);
    expect(defs.dup.description).toBe("one");
    expect(errors).toHaveLength(1);
    expect(errors[0].path).toBe("b.md");
  });

  it("separates errors from defs", () => {
    const good = parseCustomAgentFile("a.md", file("name: a\ndescription: d\ntools: read_file"));
    const bad = parseCustomAgentFile("b.md", "no frontmatter");
    const { defs, errors } = collectCustomAgents([good, bad]);
    expect(Object.keys(defs)).toEqual(["a"]);
    expect(errors).toHaveLength(1);
  });
});

describe("toolsForCustomAgent", () => {
  it("returns exactly the granted tools as ToolDefs", () => {
    const tools = toolsForCustomAgent(def({ tools: ["read_file", "grep"] }));
    expect(tools.map((tool) => tool.function.name).sort()).toEqual(["grep", "read_file"]);
  });

  it("re-intersects with the ceiling at dispatch — a smuggled name in a hand-built def never resolves", () => {
    const tools = toolsForCustomAgent(def({ tools: ["read_file", "remember", "task"] }));
    expect(tools.map((tool) => tool.function.name)).toEqual(["read_file"]);
  });
});

describe("customAgentBaseProfile", () => {
  it("read-only and web tools stay explore-class", () => {
    expect(customAgentBaseProfile(def({ tools: ["read_file", "web_search", "web_fetch"] }))).toBe("explore");
  });

  it.each(["write_file", "edit_file", "run_shell"])("%s makes it code-class", (tool) => {
    expect(customAgentBaseProfile(def({ tools: ["read_file", tool] }))).toBe("code");
  });

  it("shell_output alone does not — observation is not mutation", () => {
    expect(customAgentBaseProfile(def({ tools: ["read_file", "shell_output"] }))).toBe("explore");
  });
});

describe("composeCustomAgentCatalog", () => {
  it("is empty with no defs", () => {
    expect(composeCustomAgentCatalog([])).toBe("");
  });

  it("lists each agent with description and tools", () => {
    const catalog = composeCustomAgentCatalog([def(), def({ name: "reviewer", description: "Reviews", tools: ["grep"] })]);
    expect(catalog).toContain("## Custom agents");
    expect(catalog).toContain("- docs-writer — Writes docs (tools: read_file, grep)");
    expect(catalog).toContain("- reviewer — Reviews (tools: grep)");
  });
});
