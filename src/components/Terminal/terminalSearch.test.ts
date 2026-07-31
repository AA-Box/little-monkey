import { describe, expect, it } from "vitest";

import {
  findTerminalSearchMatches,
  nextTerminalSearchIndex,
  type TerminalSearchBuffer,
} from "./terminalSearch";

function buffer(...lines: string[]): TerminalSearchBuffer {
  return {
    length: lines.length,
    getLine: (row) => lines[row] === undefined
      ? undefined
      : { translateToString: () => lines[row].trimEnd() },
  };
}

describe("terminal output search", () => {
  it("finds literal rendered matches case-insensitively", () => {
    expect(findTerminalSearchMatches(
      buffer("Build passed", "build failed: BUILD target"),
      "build",
    )).toEqual([
      { row: 0, column: 0, length: 5 },
      { row: 1, column: 0, length: 5 },
      { row: 1, column: 14, length: 5 },
    ]);
  });

  it("returns no matches for blank or absent queries", () => {
    expect(findTerminalSearchMatches(buffer("output"), "   ")).toEqual([]);
    expect(findTerminalSearchMatches(buffer("output"), "missing")).toEqual([]);
  });

  it("wraps next and previous navigation", () => {
    expect(nextTerminalSearchIndex(2, 3, "next")).toBe(0);
    expect(nextTerminalSearchIndex(0, 3, "previous")).toBe(2);
    expect(nextTerminalSearchIndex(-1, 3, "previous")).toBe(2);
    expect(nextTerminalSearchIndex(0, 0, "next")).toBe(-1);
  });
});
