import { describe, expect, it } from "vitest";

import { textToolCallFence } from "./textToolCallFence";

describe("textToolCallFence", () => {
  it("reads the command out of a json fence the model wrote instead of calling the tool", () => {
    expect(
      textToolCallFence(
        "json",
        '{\n    "name": "run_shell",\n    "arguments": {\n        "command": "cat ~/Library/Logs/bf6.log"\n    }\n}',
      ),
    ).toEqual({ lang: "bash", body: "cat ~/Library/Logs/bf6.log" });
  });

  it("accepts a fence with no language at all", () => {
    expect(textToolCallFence("", '{"name":"run_shell","arguments":{"command":"ls -l"}}')).toEqual({
      lang: "bash",
      body: "ls -l",
    });
  });

  it("turns a fence holding several commands into one line each, in order", () => {
    expect(
      textToolCallFence(
        "json",
        '{"name":"run_shell","arguments":{"command":"rm ./steam_emu.ini"}}\n\n{"name":"run_shell","arguments":{"command":"codesign --force --sign - ./BF6.app"}}',
      ),
    ).toEqual({ lang: "bash", body: "rm ./steam_emu.ini\ncodesign --force --sign - ./BF6.app" });
  });

  it("collapses a command the model restated twice in the same fence", () => {
    const one = '{"name":"run_shell","arguments":{"command":"ls -l"}}';
    expect(textToolCallFence("json", `${one}\n${one}`)).toEqual({ lang: "bash", body: "ls -l" });
  });

  it("shows a written file as its own content, labelled and highlighted by path", () => {
    expect(
      textToolCallFence(
        "json",
        '{"name":"write_file","arguments":{"path":"~/Library/Application Support/CrossOver/Bottles/BF6/bf6.env","content":"WINEPREFIX=\\"/Applications/CrossOver.app\\""}}',
      ),
    ).toEqual({ lang: "ini", body: 'WINEPREFIX="/Applications/CrossOver.app"', label: "bf6.env" });

    // Extension we don't map, and an empty file — both still render as the
    // file they write rather than as wire JSON.
    expect(textToolCallFence("json", '{"name":"write_file","arguments":{"path":"a/b.bin","content":"x"}}')).toEqual({
      lang: "",
      body: "x",
      label: "b.bin",
    });
    expect(textToolCallFence("json", '{"name":"write_file","arguments":{"path":"src/a.ts","content":""}}')).toEqual({
      lang: "typescript",
      body: "",
      label: "a.ts",
    });
  });

  it("shows an edit as a diff, so the replaced text stays visible", () => {
    expect(
      textToolCallFence(
        "json",
        '{"name":"edit_file","arguments":{"path":"MacOS/bf6","old_string":"EXE=old","new_string":"EXE=new"}}',
      ),
    ).toEqual({ lang: "diff", body: "-EXE=old\n+EXE=new", label: "bf6" });
  });

  it("keeps a fence that mixes kinds, or a call it cannot render, as JSON", () => {
    // Two file writes cannot both be one block's body without losing one.
    const write = '{"name":"write_file","arguments":{"path":"a","content":"b"}}';
    expect(textToolCallFence("json", `${write}\n{"name":"write_file","arguments":{"path":"c","content":"d"}}`))
      .toBeNull();
    // A command next to a write: rendering either drops the other.
    expect(textToolCallFence("json", `{"name":"run_shell","arguments":{"command":"ls"}}\n${write}`)).toBeNull();
    // Prose around the call, likewise.
    expect(textToolCallFence("json", 'Run this:\n{"name":"run_shell","arguments":{"command":"ls"}}')).toBeNull();
    // A tool this renderer knows nothing about.
    expect(textToolCallFence("json", '{"name":"web_fetch","arguments":{"url":"https://example.com"}}')).toBeNull();
    // Arguments missing the fields each kind is rendered from.
    expect(textToolCallFence("json", '{"name":"write_file","arguments":{"path":"a"}}')).toBeNull();
    expect(textToolCallFence("json", '{"name":"edit_file","arguments":{"path":"a","old_string":"x"}}')).toBeNull();
  });

  it("leaves ordinary JSON, documented calls, and other languages alone", () => {
    expect(textToolCallFence("json", '{"name":"little-monkey","version":"1.7.1"}')).toBeNull();
    expect(textToolCallFence("json", '{"name":"run_shell","arguments":{"command":"ls"},"note":"example"}')).toBeNull();
    expect(textToolCallFence("json", '{"name":"run_shell","arguments":{"command":"   "}}')).toBeNull();
    expect(textToolCallFence("bash", '{"name":"run_shell","arguments":{"command":"ls"}}')).toBeNull();
    expect(textToolCallFence("json", "not json at all")).toBeNull();
  });
});
