import { describe, expect, it } from "vitest";

import { BUILTIN_FIXTURES, type FixtureSourceType } from "./redTeamFixtures";

describe("redTeamFixtures", () => {
  it("has between 10 and 20 built-in fixtures", () => {
    expect(BUILTIN_FIXTURES.length).toBeGreaterThanOrEqual(10);
    expect(BUILTIN_FIXTURES.length).toBeLessThanOrEqual(20);
  });

  it("every fixture has a unique id", () => {
    const ids = BUILTIN_FIXTURES.map((f) => f.id);
    expect(new Set(ids).size).toBe(ids.length);
  });

  it("every fixture is flagged builtin", () => {
    for (const f of BUILTIN_FIXTURES) {
      expect(f.builtin).toBe(true);
    }
  });

  it("every fixture has non-empty content and a title", () => {
    for (const f of BUILTIN_FIXTURES) {
      expect(f.content.length).toBeGreaterThan(0);
      expect(f.title.length).toBeGreaterThan(0);
    }
  });

  it("every fixture declares a concrete triggered action with a tool and args", () => {
    for (const f of BUILTIN_FIXTURES) {
      expect(f.triggeredAction.tool.length).toBeGreaterThan(0);
      expect(typeof f.triggeredAction.args).toBe("object");
      expect(f.triggeredAction.description.length).toBeGreaterThan(0);
    }
  });

  it("covers the required source-type diversity (webpage, email, MCP output, repo file, connector payload)", () => {
    const required: FixtureSourceType[] = [
      "webpage",
      "email",
      "mcp_tool_output",
      "repo_file",
      "connector_payload",
    ];
    const present = new Set(BUILTIN_FIXTURES.map((f) => f.sourceType));
    for (const type of required) {
      expect(present.has(type)).toBe(true);
    }
  });

  it("MCP-sourced fixtures are flagged isMcp", () => {
    for (const f of BUILTIN_FIXTURES) {
      if (f.sourceType === "mcp_tool_output" || f.sourceType === "connector_payload") {
        expect(f.isMcp).toBe(true);
      }
    }
  });

  it("every rawControlToken (when present) actually appears in that fixture's content", () => {
    for (const f of BUILTIN_FIXTURES) {
      if (f.rawControlToken) {
        expect(f.content.includes(f.rawControlToken)).toBe(true);
      }
    }
  });

  it("the Plan-Mode fixture pins evaluationMode to 'plan' and expects 'blocked'", () => {
    const planFixture = BUILTIN_FIXTURES.find((f) => f.expectedOutcome === "blocked");
    expect(planFixture).toBeDefined();
    expect(planFixture?.evaluationMode).toBe("plan");
  });
});
