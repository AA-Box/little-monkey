import { beforeEach, describe, expect, it, vi } from "vitest";

const invokeMock = vi.fn();
vi.mock("@tauri-apps/api/core", () => ({ invoke: (...args: unknown[]) => invokeMock(...args) }));

import { modelfileClient, type ModelfileDryRunReport, type ParsedModelfile } from "./modelfileClient";

beforeEach(() => invokeMock.mockReset());

describe("modelfileClient command contracts", () => {
  it("parse passes the raw text through to modelfile_parse and returns the parsed shape", async () => {
    const parsed: ParsedModelfile = {
      from: "llama3.2:latest",
      requires: null,
      template: null,
      system: null,
      parameters: [{ key: "temperature", value: "0.7" }],
      adapters: [],
      licenses: [],
      messages: [],
    };
    invokeMock.mockResolvedValue(parsed);

    const result = await modelfileClient.parse("FROM llama3.2:latest\nPARAMETER temperature 0.7\n");

    expect(invokeMock).toHaveBeenCalledWith("modelfile_parse", {
      text: "FROM llama3.2:latest\nPARAMETER temperature 0.7\n",
    });
    expect(result).toEqual(parsed);
  });

  it("dryRun forwards {shortName, modelfileText} as a single `request` object", async () => {
    const report: ModelfileDryRunReport = {
      shortName: "my-model",
      from: "llama3.2:latest",
      source: null,
      requires: null,
      templatePresent: false,
      systemPresent: false,
      parameters: [],
      licensePresent: false,
      licenses: [],
      adapters: [],
      messagesCount: 0,
      warnings: [],
    };
    invokeMock.mockResolvedValue(report);

    const result = await modelfileClient.dryRun({
      shortName: "my-model",
      modelfileText: "FROM llama3.2:latest\n",
    });

    expect(invokeMock).toHaveBeenCalledWith("modelfile_dry_run", {
      request: { shortName: "my-model", modelfileText: "FROM llama3.2:latest\n" },
    });
    expect(result).toEqual(report);
  });

  it("readTextFile passes the path through and returns the file's text content", async () => {
    invokeMock.mockResolvedValue("MIT License text");

    const result = await modelfileClient.readTextFile("/tmp/LICENSE.txt");

    expect(invokeMock).toHaveBeenCalledWith("modelfile_read_text_file", { path: "/tmp/LICENSE.txt" });
    expect(result).toBe("MIT License text");
  });

  it("propagates a rejected invoke (e.g. a parse/validation error) rather than swallowing it", async () => {
    invokeMock.mockRejectedValueOnce(new Error("line 2: unknown instruction 'BOGUS'"));

    await expect(modelfileClient.parse("FROM x\nBOGUS y\n")).rejects.toThrow(
      "line 2: unknown instruction 'BOGUS'",
    );
  });
});
