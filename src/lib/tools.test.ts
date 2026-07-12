import { describe, expect, it } from "vitest";

import { buildTools, TOOLS } from "./tools";

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
