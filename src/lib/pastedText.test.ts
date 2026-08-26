import { describe, expect, it, vi } from "vitest";

import {
  composePromptWithPastedText,
  estimatePastedTextTokens,
  formatEstimatedTokens,
  isPastedTextPath,
  nextPastedTextName,
  pastedTextPath,
  shouldCollapsePastedText,
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

  it("returns a sole pasted prompt byte-for-byte when no separate instruction exists", () => {
    const content = "# Spec\n\nImplement this exactly.\n";
    expect(composePromptWithPastedText("", [
      { path: "pasted://one", label: "Pasted text (1).md", content },
    ])).toBe(content);
  });

  it("keeps the typed instruction first and names multiple pasted blocks", () => {
    const prompt = composePromptWithPastedText("/review please compare these", [
      { path: "pasted://one", label: "Pasted text (1).md", content: "# Spec\nA" },
      { path: "/workspace/file.ts", content: "must not be folded in" },
      { path: "pasted://two", label: "Pasted text (2).md", content: "## More\nB" },
    ]);
    expect(prompt).toBe(
      "/review please compare these\n\n" +
      "### Pasted text (1).md\n\n# Spec\nA\n\n" +
      "### Pasted text (2).md\n\n## More\nB",
    );
  });

  it("creates opaque local-only pasted paths", () => {
    vi.stubGlobal("crypto", { randomUUID: () => "paste-id" });
    const path = pastedTextPath("Pasted text (1).md");
    expect(path).toBe("pasted://paste-id/Pasted%20text%20(1).md");
    expect(isPastedTextPath(path)).toBe(true);
    vi.unstubAllGlobals();
  });
});
