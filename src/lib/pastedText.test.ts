import { describe, expect, it, vi } from "vitest";

import {
  composePromptWithPastedText,
  estimatePastedTextTokens,
  formatEstimatedTokens,
  isPastedTextPath,
  nextPastedTextName,
  nextPastedTextOrder,
  pastedTextPath,
  rebasePastedTextPlacements,
  shouldCollapsePastedText,
  type PastedTextPlacement,
} from "./pastedText";

describe("pastedText", () => {
  it("keeps ordinary clipboard text in the textarea", () => {
    expect(shouldCollapsePastedText("small paste\nwith a few lines")).toBe(false);
  });

  it("collapses a long paste by character count without calling a model", () => {
    expect(shouldCollapsePastedText("x".repeat(8_000))).toBe(true);
  });

  it("collapses a line-heavy paste even when character count is modest", () => {
    expect(shouldCollapsePastedText(Array.from({ length: 80 }, () => "x").join("\n"))).toBe(true);
  });

  it("uses a deterministic provider-independent token estimate", () => {
    expect(estimatePastedTextTokens("")).toBe(0);
    expect(estimatePastedTextTokens("x".repeat(4_000))).toBe(1_000);
    expect(formatEstimatedTokens("x".repeat(4_000))).toBe("~1.0k tokens");
  });

  it("numbers pasted markdown cards independently of other attachments", () => {
    expect(nextPastedTextName([
      { path: "/tmp/a.txt", label: "a.txt" },
      { path: "pasted://one", label: "Pasted text (1).md" },
      { path: "pasted://four", label: "Pasted text (4).md" },
    ])).toBe("Pasted text (5).md");
  });

  it("assigns stable ordering to consecutive zero-width pasted anchors", () => {
    expect(nextPastedTextOrder([])).toBe(0);
    expect(nextPastedTextOrder([
      { path: "pasted://a", offset: 0, order: 0 },
      { path: "pasted://b", offset: 0, order: 3 },
    ])).toBe(4);
  });

  it("returns a sole pasted prompt byte-for-byte, including boundary whitespace", () => {
    const content = "\n  # Spec\n\nImplement this exactly.\n\n";
    expect(composePromptWithPastedText("", [
      { path: "pasted://one", label: "Pasted text (1).md", content },
    ], [
      { path: "pasted://one", offset: 0, order: 0 },
    ])).toBe(content);
  });

  it("reconstructs a paste at the exact middle position without synthetic headings or separators", () => {
    const visible = "before  after";
    const pasted = "<PASTE>\n";
    expect(composePromptWithPastedText(visible, [
      { path: "pasted://one", label: "Pasted text (1).md", content: pasted },
    ], [
      { path: "pasted://one", offset: 7, order: 0 },
    ])).toBe(`before ${pasted} after`);
  });

  it("keeps multiple consecutive pastes in event order without adding prompt text", () => {
    expect(composePromptWithPastedText("tail", [
      { path: "pasted://one", content: "FIRST" },
      { path: "pasted://two", content: "SECOND" },
    ], [
      { path: "pasted://one", offset: 0, order: 0 },
      { path: "pasted://two", offset: 0, order: 1 },
    ])).toBe("FIRSTSECONDtail");
  });

  it("rebases anchors when the user types before and after a collapsed paste", () => {
    let visible = "AB";
    let placements: PastedTextPlacement[] = [{ path: "pasted://one", offset: 1, order: 0 }];

    const typedBefore = "xAB";
    placements = rebasePastedTextPlacements(visible, typedBefore, placements);
    visible = typedBefore;
    expect(placements[0].offset).toBe(2);

    const typedAtAnchor = "xAyB";
    placements = rebasePastedTextPlacements(visible, typedAtAnchor, placements);
    visible = typedAtAnchor;
    // An insertion exactly at the zero-width anchor follows the paste.
    expect(placements[0].offset).toBe(2);

    expect(composePromptWithPastedText(visible, [
      { path: "pasted://one", content: "PASTE" },
    ], placements)).toBe("xAPASTEyB");
  });

  it("rebases an anchor across deletion/replacement of surrounding visible text", () => {
    const placements: PastedTextPlacement[] = [{ path: "pasted://one", offset: 6, order: 0 }];
    expect(rebasePastedTextPlacements("hello world", "hi world", placements)).toEqual([
      { path: "pasted://one", offset: 3, order: 0 },
    ]);
    expect(rebasePastedTextPlacements("abcDEFghi", "abcXghi", [
      { path: "pasted://one", offset: 5, order: 0 },
    ])).toEqual([
      { path: "pasted://one", offset: 3, order: 0 },
    ]);
  });

  it("creates opaque local-only pasted paths", () => {
    vi.stubGlobal("crypto", { randomUUID: () => "paste-id" });
    const path = pastedTextPath("Pasted text (1).md");
    expect(path).toBe("pasted://paste-id/Pasted%20text%20(1).md");
    expect(isPastedTextPath(path)).toBe(true);
    vi.unstubAllGlobals();
  });
});
