import { describe, expect, it } from "vitest";

import { textToolCallShellCommand } from "./textToolCallFence";

describe("textToolCallShellCommand", () => {
  it("reads the command out of a json fence the model wrote instead of calling the tool", () => {
    expect(
      textToolCallShellCommand(
        "json",
        '{\n    "name": "run_shell",\n    "arguments": {\n        "command": "cat ~/Library/Logs/bf6.log"\n    }\n}',
      ),
    ).toBe("cat ~/Library/Logs/bf6.log");
  });

  it("accepts a fence with no language at all", () => {
    expect(textToolCallShellCommand("", '{"name":"run_shell","arguments":{"command":"ls -l"}}')).toBe("ls -l");
  });

  it("turns a fence holding several calls into one command line each, in order", () => {
    expect(
      textToolCallShellCommand(
        "json",
        '{"name":"run_shell","arguments":{"command":"rm ./steam_emu.ini"}}\n\n{"name":"run_shell","arguments":{"command":"codesign --force --sign - ./BF6.app"}}',
      ),
    ).toBe("rm ./steam_emu.ini\ncodesign --force --sign - ./BF6.app");
  });

  it("collapses a call the model restated twice in the same fence", () => {
    const one = '{"name":"run_shell","arguments":{"command":"ls -l"}}';
    expect(textToolCallShellCommand("json", `${one}\n${one}`)).toBe("ls -l");
  });

  it("keeps a fence that mixes a command with anything else as JSON", () => {
    // A non-shell call alongside a shell one: converting would delete the
    // `write_file` half, so the whole fence stays as it was written.
    expect(
      textToolCallShellCommand(
        "json",
        '{"name":"run_shell","arguments":{"command":"ls"}}\n{"name":"write_file","arguments":{"path":"a","content":"b"}}',
      ),
    ).toBeNull();
    // Prose around the call, likewise.
    expect(
      textToolCallShellCommand("json", 'Run this:\n{"name":"run_shell","arguments":{"command":"ls"}}'),
    ).toBeNull();
  });

  it("leaves a non-shell tool call as the JSON it is", () => {
    const body = '{"name":"edit_file","arguments":{"path":"a.ts","old_string":"x","new_string":"y"}}';
    expect(textToolCallShellCommand("json", body)).toBeNull();
  });

  it("leaves ordinary JSON, documented calls, and other languages alone", () => {
    expect(textToolCallShellCommand("json", '{"name":"little-monkey","version":"1.7.1"}')).toBeNull();
    expect(
      textToolCallShellCommand("json", '{"name":"run_shell","arguments":{"command":"ls"},"note":"example"}'),
    ).toBeNull();
    expect(textToolCallShellCommand("json", '{"name":"run_shell","arguments":{"command":"   "}}')).toBeNull();
    expect(textToolCallShellCommand("bash", '{"name":"run_shell","arguments":{"command":"ls"}}')).toBeNull();
    expect(textToolCallShellCommand("json", "not json at all")).toBeNull();
  });
});
