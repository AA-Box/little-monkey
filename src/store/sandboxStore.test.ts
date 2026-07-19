import { beforeEach, describe, expect, it, vi } from "vitest";

const api = vi.hoisted(() => ({
  runInSandbox: vi.fn(),
  listSandboxRuns: vi.fn(),
  sandboxDiff: vi.fn(),
  prepareSandboxPromote: vi.fn(),
  executeSandboxPromote: vi.fn(),
  discardSandboxRun: vi.fn(),
}));

vi.mock("../lib/sandbox", async (importOriginal) => ({
  ...await importOriginal<typeof import("../lib/sandbox")>(),
  ...api,
}));

const durableArtifacts = vi.hoisted(() => ({
  readDurableArtifact: vi.fn(),
}));

vi.mock("../lib/durableArtifacts", async (importOriginal) => ({
  ...await importOriginal<typeof import("../lib/durableArtifacts")>(),
  ...durableArtifacts,
}));

import type {
  SandboxDiffEntry,
  SandboxPromotePreview,
  SandboxRunListEntry,
  SandboxRunSummary,
} from "../lib/sandbox";
import { useSandboxStore } from "./sandboxStore";

const summary: SandboxRunSummary = {
  runId: "sandbox-fixture",
  isolation: "os_sandboxed",
  exitCode: 0,
  timedOut: false,
  passed: true,
  durationMs: 42,
  stdoutArtifactId: "a".repeat(64),
  stderrArtifactId: "b".repeat(64),
  stdoutExcerpt: "ok",
  stderrExcerpt: "",
  filesCopied: 3,
};

const listEntry: SandboxRunListEntry = {
  runId: "sandbox-fixture",
  status: "running",
  task: "Sandboxed shell command:\necho ok",
  createdAtMs: 1,
  updatedAtMs: 1,
};

const diff: SandboxDiffEntry[] = [
  { path: "src/lib.rs", status: "modified", sandboxSha256: "c".repeat(64), workspaceSha256: "d".repeat(64), sizeBytes: 10 },
  { path: "new-file.txt", status: "added", sandboxSha256: "e".repeat(64), workspaceSha256: null, sizeBytes: 5 },
];

const preview: SandboxPromotePreview = {
  runId: "sandbox-fixture",
  digest: "f".repeat(64),
  confirmationPhrase: `CONFIRM ${"f".repeat(12)}`,
  files: [{ path: "src/lib.rs", sha256: "c".repeat(64), sizeBytes: 10 }],
  expiresAtMs: Date.now() + 60_000,
};

beforeEach(() => {
  for (const mock of Object.values(api)) mock.mockReset();
  for (const mock of Object.values(durableArtifacts)) mock.mockReset();
  api.listSandboxRuns.mockResolvedValue([listEntry]);
  useSandboxStore.setState({
    runs: [], activeRunId: null, activeSummary: null, stdoutText: null, stderrText: null,
    diff: [], selectedFiles: [], preview: null, busy: {}, error: null, notice: null,
  });
});

describe("sandboxStore", () => {
  it("runs a command and refreshes the run list", async () => {
    api.runInSandbox.mockResolvedValue(summary);
    const result = await useSandboxStore.getState().run("echo ok");
    expect(result).toEqual(summary);
    expect(useSandboxStore.getState().activeRunId).toBe("sandbox-fixture");
    expect(useSandboxStore.getState().activeSummary).toEqual(summary);
    expect(useSandboxStore.getState().runs).toEqual([listEntry]);
  });

  it("loads stdout/stderr logs decoded from base64 artifacts", async () => {
    durableArtifacts.readDurableArtifact.mockImplementation((id: string) =>
      Promise.resolve({ blob: { id, size: 2 }, contentBase64: btoa("ok") }));
    await useSandboxStore.getState().loadLogs(summary);
    expect(useSandboxStore.getState().stdoutText).toBe("ok");
    expect(useSandboxStore.getState().stderrText).toBe("ok");
    expect(durableArtifacts.readDurableArtifact).toHaveBeenCalledWith(summary.stdoutArtifactId);
    expect(durableArtifacts.readDurableArtifact).toHaveBeenCalledWith(summary.stderrArtifactId);
  });

  it("loads a diff and preselects every changed file", async () => {
    api.sandboxDiff.mockResolvedValue(diff);
    await useSandboxStore.getState().loadDiff("sandbox-fixture");
    expect(useSandboxStore.getState().diff).toEqual(diff);
    expect(useSandboxStore.getState().selectedFiles).toEqual(["src/lib.rs", "new-file.txt"]);
  });

  it("toggles individual file selection", () => {
    useSandboxStore.setState({ selectedFiles: ["a.txt"] });
    useSandboxStore.getState().toggleFile("b.txt");
    expect(useSandboxStore.getState().selectedFiles).toEqual(["a.txt", "b.txt"]);
    useSandboxStore.getState().toggleFile("a.txt");
    expect(useSandboxStore.getState().selectedFiles).toEqual(["b.txt"]);
  });

  it("refuses to prepare a promote with no files selected", async () => {
    await expect(useSandboxStore.getState().preparePromote("sandbox-fixture", []))
      .rejects.toThrow("Select at least one file");
    expect(api.prepareSandboxPromote).not.toHaveBeenCalled();
  });

  it("executes only the exact prepared digest and phrase, then refreshes diff and runs", async () => {
    api.prepareSandboxPromote.mockResolvedValue(preview);
    api.executeSandboxPromote.mockResolvedValue({ runId: "sandbox-fixture", promotedFiles: ["src/lib.rs"] });
    api.sandboxDiff.mockResolvedValue([]);

    await useSandboxStore.getState().preparePromote("sandbox-fixture", ["src/lib.rs"]);
    expect(useSandboxStore.getState().preview).toEqual(preview);

    await useSandboxStore.getState().executePromote(preview.confirmationPhrase);
    expect(api.executeSandboxPromote).toHaveBeenCalledWith(
      preview.runId,
      preview.digest,
      preview.confirmationPhrase,
    );
    expect(useSandboxStore.getState().preview).toBeNull();
  });

  it("refuses an expired prepared promote without invoking the backend", async () => {
    useSandboxStore.setState({ preview: { ...preview, expiresAtMs: Date.now() - 1 } });
    await expect(useSandboxStore.getState().executePromote(preview.confirmationPhrase))
      .rejects.toThrow("expired");
    expect(api.executeSandboxPromote).not.toHaveBeenCalled();
  });

  it("discards a run and clears any state scoped to it", async () => {
    api.discardSandboxRun.mockResolvedValue(undefined);
    useSandboxStore.setState({
      activeRunId: "sandbox-fixture",
      activeSummary: summary,
      diff,
      preview,
    });
    await useSandboxStore.getState().discard("sandbox-fixture", "no longer needed");
    expect(api.discardSandboxRun).toHaveBeenCalledWith("sandbox-fixture", "no longer needed");
    expect(useSandboxStore.getState().activeRunId).toBeNull();
    expect(useSandboxStore.getState().activeSummary).toBeNull();
    expect(useSandboxStore.getState().diff).toEqual([]);
    expect(useSandboxStore.getState().preview).toBeNull();
  });
});
