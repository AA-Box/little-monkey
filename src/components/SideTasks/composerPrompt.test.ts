import { describe, expect, it } from "vitest";

import { appendAttachmentContext, attachmentSourceLabel, deriveSideTaskTitle } from "./composerPrompt";

describe("deriveSideTaskTitle", () => {
  it("uses the first non-empty line", () => {
    expect(deriveSideTaskTitle("\n\nExplain the auth flow\nthen list the callers")).toBe("Explain the auth flow");
  });

  it("strips markdown lead-ins and collapses whitespace", () => {
    expect(deriveSideTaskTitle("## - Explain   the   auth flow")).toBe("Explain the auth flow");
  });

  it("cuts long prompts on a word boundary", () => {
    const title = deriveSideTaskTitle(
      "Review every permission gate in the Rust backend and report which ones bypass the prompt",
    );
    expect(title.endsWith("…")).toBe(true);
    expect(title.length).toBeLessThanOrEqual(61);
    expect(title).not.toMatch(/\s…$/);
    expect(title.startsWith("Review every permission gate in the Rust backend")).toBe(true);
  });

  it("cuts mid-word rather than returning a stub when the first break is too early", () => {
    const title = deriveSideTaskTitle(`${"a".repeat(70)} tail`);
    expect(title).toBe(`${"a".repeat(60)}…`);
  });

  it("falls back to a generic title for empty input", () => {
    expect(deriveSideTaskTitle("   \n  ")).toBe("Side task");
    expect(deriveSideTaskTitle("###")).toBe("Side task");
  });
});

describe("appendAttachmentContext", () => {
  it("returns the prompt untouched with no attachments", () => {
    expect(appendAttachmentContext("Explain this", [])).toBe("Explain this");
  });

  it("lists paths and marks directories", () => {
    expect(
      appendAttachmentContext("Explain this", [
        { path: "/repo/src/lib.rs", isDir: false },
        { path: "/repo/src", isDir: true },
      ]),
    ).toBe("Explain this\n\nFiles in scope:\n- /repo/src/lib.rs\n- /repo/src (directory)");
  });

  it("emits just the block when nothing was typed", () => {
    expect(appendAttachmentContext("  ", [{ path: "/repo/a.ts", isDir: false }])).toBe(
      "Files in scope:\n- /repo/a.ts",
    );
  });
});

describe("attachmentSourceLabel", () => {
  it("singularises one path", () => {
    expect(attachmentSourceLabel([{ path: "/a", isDir: false }])).toBe("1 attached path");
  });

  it("counts several", () => {
    expect(
      attachmentSourceLabel([
        { path: "/a", isDir: false },
        { path: "/b", isDir: true },
      ]),
    ).toBe("2 attached paths");
  });
});
