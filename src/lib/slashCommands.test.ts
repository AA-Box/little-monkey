import { describe, expect, it } from "vitest";
import {
  formatCommandNotice,
  parseBuiltInSlashCommand,
  parseCommandNotice,
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

  it("round-trips visible host command notices", () => {
    const content = formatCommandNotice({ command: "status", text: "Ready", ok: true });
    expect(parseCommandNotice({ role: "system", content })).toEqual({ command: "status", text: "Ready", ok: true });
    expect(parseCommandNotice({ role: "assistant", content })).toBeNull();
  });
});
