import { describe, expect, it } from "vitest";
import type { ChatMessage } from "./llamaClient";
import {
  buildSideQuestionWire,
  formatBtwNotice,
  formatCommandNotice,
  parseBtwNotice,
  parseBuiltInSlashCommand,
  parseCommandNotice,
  SIDE_QUESTION_SYSTEM_PROMPT,
} from "./slashCommands";

describe("built-in slash commands", () => {
  it("parses only an exact leading built-in command", () => {
    expect(parseBuiltInSlashCommand("  /model ollama:llama3.2  ")).toMatchObject({
      definition: { command: "model" },
      arguments: "ollama:llama3.2",
    });
    expect(parseBuiltInSlashCommand("please /status")).toBeNull();
    expect(parseBuiltInSlashCommand("/unknown keep this as chat")).toBeNull();
  });

  it("parses /btw with the question as arguments", () => {
    expect(parseBuiltInSlashCommand("/btw what did we decide about retries?")).toMatchObject({
      definition: { command: "btw" },
      arguments: "what did we decide about retries?",
    });
  });

  // The hyphen matters: the parser's name pattern has to accept it, or
  // `/pm-plan …` silently falls through to the model as ordinary chat text.
  it("parses /pm-plan with the product goal as arguments", () => {
    expect(parseBuiltInSlashCommand("/pm-plan let users export their data as CSV")).toMatchObject({
      definition: { command: "pm-plan" },
      arguments: "let users export their data as CSV",
    });
    expect(parseBuiltInSlashCommand("/pm-plan")).toMatchObject({ definition: { command: "pm-plan" }, arguments: "" });
  });

  it("round-trips visible host command notices", () => {
    const content = formatCommandNotice({ command: "status", text: "Ready", ok: true });
    expect(parseCommandNotice({ role: "system", content })).toEqual({ command: "status", text: "Ready", ok: true });
    expect(parseCommandNotice({ role: "assistant", content })).toBeNull();
  });
});

describe("/btw side questions", () => {
  const notice = { question: "why?", answer: "because", ok: true, done: true };

  it("round-trips btw notices and rejects other shapes", () => {
    const content = formatBtwNotice(notice);
    expect(parseBtwNotice({ role: "system", content })).toEqual(notice);
    expect(parseBtwNotice({ role: "assistant", content })).toBeNull();
    expect(parseBtwNotice({ role: "system", content: formatCommandNotice({ command: "status", text: "x", ok: true }) })).toBeNull();
  });

  it("builds a side-question wire that excludes earlier btw exchanges", () => {
    const history: ChatMessage[] = [
      { role: "user", content: "hello" },
      { role: "assistant", content: "hi" },
      { role: "system", content: formatBtwNotice(notice) },
    ];
    const wire = buildSideQuestionWire(history, "what model is this?");
    expect(wire).toEqual([
      { role: "system", content: SIDE_QUESTION_SYSTEM_PROMPT },
      { role: "user", content: "hello" },
      { role: "assistant", content: "hi" },
      { role: "user", content: "what model is this?" },
    ]);
    // The stored history itself is untouched — the notice stays for display.
    expect(history).toHaveLength(3);
  });
});
