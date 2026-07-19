import { describe, expect, it } from "vitest";

import { parseModelJsonCandidates } from "./modelJson";

describe("parseModelJsonCandidates", () => {
  it("parses raw objects and arrays with root filtering", () => {
    expect(parseModelJsonCandidates('{"ok":true}', "object")).toEqual([{ ok: true }]);
    expect(parseModelJsonCandidates('[{"id":1}]', "array")).toEqual([[{ id: 1 }]]);
    expect(parseModelJsonCandidates('[{"id":1}]', "object")).toEqual([]);
    expect(parseModelJsonCandidates('{"ok":true}', "array")).toEqual([]);
  });

  it("extracts fenced JSON without treating nested brackets as separate replies", () => {
    const content = [
      "Here is the requested result:",
      "```json",
      '{"items":[{"label":"A"}],"note":"literal } and ] stay in this string"}',
      "```",
    ].join("\n");

    expect(parseModelJsonCandidates(content, "object")).toEqual([
      { items: [{ label: "A" }], note: "literal } and ] stay in this string" },
    ]);
  });

  it("finds complete objects or arrays in surrounding prose", () => {
    const content = 'First {not valid JSON}; then {"level":"high"}; finally [1,2,3].';

    expect(parseModelJsonCandidates(content)).toEqual([{ level: "high" }, [1, 2, 3]]);
    expect(parseModelJsonCandidates(content, "object")).toEqual([{ level: "high" }]);
    expect(parseModelJsonCandidates(content, "array")).toEqual([[1, 2, 3]]);
  });

  it("returns no candidates for malformed or truncated output", () => {
    expect(parseModelJsonCandidates("not JSON", "object")).toEqual([]);
    expect(parseModelJsonCandidates('{"outer":{"valid":true}', "object")).toEqual([]);
    expect(parseModelJsonCandidates('```json\n[{"id":1}\n```', "array")).toEqual([]);
  });

  it("rejects primitive JSON roots", () => {
    expect(parseModelJsonCandidates('"hello"')).toEqual([]);
    expect(parseModelJsonCandidates("42")).toEqual([]);
    expect(parseModelJsonCandidates("null")).toEqual([]);
  });
});
