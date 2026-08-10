import { describe, expect, it } from "vitest";

import { chapterTitle, formatMessageTime } from "./messageChapters";
import { toWireMessages } from "../../lib/llamaClient";

describe("chapterTitle", () => {
  it("uses the answer's first non-empty line, stripped of Markdown", () => {
    expect(chapterTitle("\n\n## Deploying the **worker**\n\nBody text")).toBe("Deploying the worker");
    expect(chapterTitle("- run `pnpm build` first")).toBe("run pnpm build first");
    expect(chapterTitle("See [the docs](https://example.com) for details")).toBe("See the docs for details");
  });

  it("truncates a long first line", () => {
    const title = chapterTitle("x".repeat(80));
    expect(title).toHaveLength(49);
    expect(title.endsWith("…")).toBe(true);
  });

  it("falls back to a generic label when there is no usable text", () => {
    expect(chapterTitle("   \n\n  ")).toBe("Chapter");
    expect(chapterTitle("###   ")).toBe("Chapter");
  });
});

describe("formatMessageTime", () => {
  const now = Date.UTC(2026, 7, 9, 12, 0, 0);
  const minute = 60_000;

  it("reads as 'just now' under a minute, including a clock skewed ahead", () => {
    expect(formatMessageTime(now - 30_000, now, "en-US")).toBe("just now");
    expect(formatMessageTime(now + 5_000, now, "en-US")).toBe("just now");
  });

  it("counts up through minutes, hours, and days", () => {
    expect(formatMessageTime(now - 15 * minute, now, "en-US")).toBe("15 minutes ago");
    expect(formatMessageTime(now - 3 * 60 * minute, now, "en-US")).toBe("3 hours ago");
    expect(formatMessageTime(now - 26 * 60 * minute, now, "en-US")).toBe("yesterday");
  });

  it("shows a date once the relative form stops being readable", () => {
    expect(formatMessageTime(Date.UTC(2026, 5, 19, 12, 0, 0), now, "en-US")).toBe("Jun 19");
  });
});

describe("toWireMessages", () => {
  it("drops the local-only fields the footer renders from", () => {
    expect(
      toWireMessages([
        { role: "assistant", content: "hi", at: 1, chapter: "Intro" },
        { role: "user", content: "hey", at: 2 },
      ]),
    ).toEqual([
      { role: "assistant", content: "hi" },
      { role: "user", content: "hey" },
    ]);
  });

  it("keeps every field the endpoint actually needs", () => {
    const toolCall = { id: "call_1", type: "function" as const, function: { name: "read", arguments: "{}" } };
    expect(
      toWireMessages([
        { role: "assistant", content: "", tool_calls: [toolCall], at: 1 },
        { role: "tool", content: "ok", tool_call_id: "call_1", at: 2 },
      ]),
    ).toEqual([
      { role: "assistant", content: "", tool_calls: [toolCall] },
      { role: "tool", content: "ok", tool_call_id: "call_1" },
    ]);
  });
});
