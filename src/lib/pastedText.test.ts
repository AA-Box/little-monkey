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

  it("reconstructs exact pasted blocks before visible composer text", () => {
    const prompt = composePromptWithPastedText("please implement this", [
      { path: "pasted://one", content: "# Spec\nA" },
      { path: "/workspace/file.ts", content: "must not be folded in" },
      { path: "pasted://two", content: "## More\nB" },
    ]);
    expect(prompt).toBe("# Spec\nA\n\n## More\nB\n\nplease implement this");
  });

  it("creates opaque local-only pasted paths", () => {
    vi.stubGlobal("crypto", { randomUUID: () => "paste-id" });
    const path = pastedTextPath("Pasted text (1).md");
    expect(path).toBe("pasted://paste-id/Pasted%20text%20(1).md");
    expect(isPastedTextPath(path)).toBe(true);
    vi.unstubAllGlobals();
  });
});
