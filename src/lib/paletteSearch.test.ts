import { describe, expect, it } from "vitest";

import { fuzzyScore, paletteItemScore, searchPaletteItems, type PaletteItem } from "./paletteSearch";

function item(overrides: Partial<PaletteItem> & Pick<PaletteItem, "id" | "kind" | "title">): PaletteItem {
  return { sensitive: false, ...overrides };
}

describe("fuzzyScore", () => {
  it("matches an empty query against anything with score 0", () => {
    expect(fuzzyScore("", "Summarize")).toBe(0);
  });

  it("ranks an exact substring match above a scattered subsequence match", () => {
    const substring = fuzzyScore("sum", "Summarize");
    const subsequence = fuzzyScore("sze", "Summarize");
    expect(substring).not.toBeNull();
    expect(subsequence).not.toBeNull();
    expect(substring! > subsequence!).toBe(true);
  });

  it("scores a word-start substring match higher than a mid-word one", () => {
    const wordStart = fuzzyScore("work", "Start workflow");
    const midWord = fuzzyScore("art", "Start workflow");
    expect(wordStart).not.toBeNull();
    expect(midWord).not.toBeNull();
    expect(wordStart! > midWord!).toBe(true);
  });

  it("returns null when a query character never appears in order", () => {
    expect(fuzzyScore("xyz", "Summarize")).toBeNull();
    expect(fuzzyScore("mus", "Summarize")).toBeNull(); // 'u' before 'm' — out of order
  });

  it("is case-insensitive", () => {
    expect(fuzzyScore("SUM", "summarize")).toEqual(fuzzyScore("sum", "SUMMARIZE"));
  });
});

describe("paletteItemScore", () => {
  it("matches against subtitle and keywords, not just title", () => {
    const target = item({ id: "1", kind: "recipe", title: "Nightly audit", subtitle: "Checks dependencies", keywords: ["deps", "security"] });
    expect(paletteItemScore("dependencies", target)).not.toBeNull();
    expect(paletteItemScore("security", target)).not.toBeNull();
    expect(paletteItemScore("zzz-no-match", target)).toBeNull();
  });

  it("weights the title match above an equally-good subtitle/keyword match", () => {
    const titleMatch = item({ id: "1", kind: "recipe", title: "deploy", subtitle: "unrelated" });
    const subtitleMatch = item({ id: "2", kind: "recipe", title: "unrelated", subtitle: "deploy" });
    expect(paletteItemScore("deploy", titleMatch)!).toBeGreaterThan(paletteItemScore("deploy", subtitleMatch)!);
  });
});

describe("searchPaletteItems", () => {
  const items: PaletteItem[] = [
    item({ id: "quick:summarize", kind: "quickAction", title: "Summarize" }),
    item({ id: "quick:rewrite", kind: "quickAction", title: "Rewrite" }),
    item({ id: "session:1", kind: "session", title: "Refactor the parser" }),
    item({ id: "model:1", kind: "model", title: "qwen2.5:14b" }),
  ];

  it("returns every item in original order for an empty query", () => {
    expect(searchPaletteItems(items, "").map((r) => r.item.id)).toEqual(items.map((i) => i.id));
  });

  it("filters out non-matching items and ranks the best match first", () => {
    const results = searchPaletteItems(items, "re");
    expect(results.map((r) => r.item.id)).toContain("quick:rewrite");
    expect(results.map((r) => r.item.id)).toContain("session:1");
    expect(results.map((r) => r.item.id)).not.toContain("model:1");
    // "Rewrite" starts with "re" (word-start substring) — should outrank
    // "Refactor the parser", which only matches "re" mid-title... actually
    // also word-start ("Refactor" starts with "Re") — both are word-start
    // substring matches, so the tie is broken by original array order,
    // which places "quick:rewrite" first.
    expect(results[0].item.id).toBe("quick:rewrite");
  });

  it("returns an empty array when nothing matches", () => {
    expect(searchPaletteItems(items, "zzzzzz")).toEqual([]);
  });

  it("caps results at the given limit", () => {
    const many = Array.from({ length: 10 }, (_, index) =>
      item({ id: `x${index}`, kind: "file", title: `file-${index}.ts` }),
    );
    expect(searchPaletteItems(many, "file", 3)).toHaveLength(3);
  });
});
