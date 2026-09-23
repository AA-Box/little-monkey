import { describe, expect, it } from "vitest";
import { parseChatExport } from "./chatExportImport";

describe("parseChatExport", () => {
  it("parses ChatGPT mapping exports", () => {
    const rows = parseChatExport([{ title: "Hello", mapping: { a: { message: { author: { role: "user" }, content: { parts: ["Hi"] }, create_time: 1 } }, b: { message: { author: { role: "assistant" }, content: { parts: ["Hello"] }, create_time: 2 } } } }]);
    expect(rows).toHaveLength(1);
    expect(rows[0]?.markdown).toContain("## assistant");
  });
  it("parses Claude/Gemini message arrays", () => {
    const rows = parseChatExport([{ name: "Thread", chat_messages: [{ sender: "human", text: "A" }, { sender: "assistant", text: "B" }] }]);
    expect(rows[0]?.title).toBe("Thread");
    expect(rows[0]?.markdown).toContain("## human");
  });
});
