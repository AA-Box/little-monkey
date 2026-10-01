import { describe, expect, it } from "vitest";
import { parseChatExport } from "./chatExportImport";

describe("parseChatExport", () => {
  it("follows ChatGPT current_node instead of merging abandoned branches", () => {
    const rows = parseChatExport([{
      title: "Edited answer",
      current_node: "new-answer",
      mapping: {
        root: { id: "root", message: null, parent: null, children: ["user"] },
        user: {
          id: "user",
          parent: "root",
          children: ["old-answer", "new-answer"],
          message: { author: { role: "user" }, content: { parts: ["Question"] } },
        },
        "old-answer": {
          id: "old-answer",
          parent: "user",
          children: [],
          message: { author: { role: "assistant" }, content: { parts: ["Abandoned answer"] } },
        },
        "new-answer": {
          id: "new-answer",
          parent: "user",
          children: [],
          message: { author: { role: "assistant" }, content: { parts: ["Visible answer"] } },
        },
      },
    }]);

    expect(rows).toHaveLength(1);
    expect(rows[0]?.markdown).toContain("Visible answer");
    expect(rows[0]?.markdown).not.toContain("Abandoned answer");
    expect(rows[0]?.platform).toBe("chatgpt");
  });

  it("uses the newest ChatGPT branch when current_node is absent", () => {
    const rows = parseChatExport([{
      title: "Fallback",
      mapping: {
        root: { id: "root", message: null, parent: null, children: ["user"] },
        user: {
          id: "user",
          parent: "root",
          children: ["first", "latest"],
          message: { author: { role: "user" }, content: { parts: ["Hi"] } },
        },
        first: {
          id: "first",
          parent: "user",
          children: [],
          message: { author: { role: "assistant" }, content: { parts: ["First"] } },
        },
        latest: {
          id: "latest",
          parent: "user",
          children: [],
          message: { author: { role: "assistant" }, content: { parts: ["Latest"] } },
        },
      },
    }]);
    expect(rows[0]?.markdown).toContain("Latest");
    expect(rows[0]?.markdown).not.toContain("First");
  });

  it("parses modern Claude content blocks and extracted attachments", () => {
    const rows = parseChatExport([{
      uuid: "thread-1",
      name: "Claude thread",
      updated_at: "2026-09-01T10:00:00Z",
      chat_messages: [
        {
          sender: "human",
          text: "",
          content: [{ type: "text", text: "Read this" }],
          attachments: [{ file_name: "notes.txt", extracted_content: "Attachment body" }],
        },
        { sender: "assistant", content: [{ type: "text", text: "Done" }] },
      ],
    }]);

    expect(rows).toHaveLength(1);
    expect(rows[0]?.markdown).toContain("Read this");
    expect(rows[0]?.markdown).toContain("[attachment: notes.txt]");
    expect(rows[0]?.markdown).toContain("Attachment body");
    expect(rows[0]?.platform).toBe("claude");
  });

  it("parses Gemini Takeout activity wrappers and content blocks", () => {
    const rows = parseChatExport({
      activities: [{
        header: "Gemini Apps",
        title: "Trip ideas",
        time: "2026-08-20T12:00:00Z",
        messages: [
          { role: "user", content: [{ text: "Plan a trip" }] },
          { role: "model", content: [{ text: "Here is a plan" }] },
        ],
      }],
    });

    expect(rows).toHaveLength(1);
    expect(rows[0]?.markdown).toContain("Plan a trip");
    expect(rows[0]?.markdown).toContain("Here is a plan");
    expect(rows[0]?.platform).toBe("gemini");
  });

  it("ignores ChatGPT system and tool-only nodes", () => {
    const rows = parseChatExport([{
      title: "Roles",
      current_node: "assistant",
      mapping: {
        root: { id: "root", message: null, parent: null, children: ["system"] },
        system: {
          id: "system",
          parent: "root",
          children: ["user"],
          message: { author: { role: "system" }, content: { parts: ["secret system text"] } },
        },
        user: {
          id: "user",
          parent: "system",
          children: ["tool"],
          message: { author: { role: "user" }, content: { parts: ["Hello"] } },
        },
        tool: {
          id: "tool",
          parent: "user",
          children: ["assistant"],
          message: { author: { role: "tool" }, content: { parts: ["tool payload"] } },
        },
        assistant: {
          id: "assistant",
          parent: "tool",
          children: [],
          message: { author: { role: "assistant" }, content: { parts: ["Hi"] } },
        },
      },
    }]);

    expect(rows[0]?.markdown).toContain("Hello");
    expect(rows[0]?.markdown).toContain("Hi");
    expect(rows[0]?.markdown).not.toContain("secret system text");
    expect(rows[0]?.markdown).not.toContain("tool payload");
  });
});
