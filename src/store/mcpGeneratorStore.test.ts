import { beforeAll, beforeEach, describe, expect, it, vi } from "vitest";

const mocks = vi.hoisted(() => ({
  generateMcpServerCode: vi.fn(),
  probeGeneratedMcpArtifact: vi.fn(),
  resolveGeneratorTarget: vi.fn(),
  save: vi.fn(),
  writeTextFile: vi.fn(),
}));

vi.mock("@tauri-apps/plugin-dialog", () => ({ save: (...args: unknown[]) => mocks.save(...args) }));
vi.mock("@tauri-apps/plugin-fs", () => ({ writeTextFile: (...args: unknown[]) => mocks.writeTextFile(...args) }));
vi.mock("../lib/mcpGenerator", async () => {
  const actual = await vi.importActual<typeof import("../lib/mcpGenerator")>("../lib/mcpGenerator");
  return {
    ...actual,
    generateMcpServerCode: (...args: unknown[]) => mocks.generateMcpServerCode(...args),
    probeGeneratedMcpArtifact: (...args: unknown[]) => mocks.probeGeneratedMcpArtifact(...args),
    resolveGeneratorTarget: (...args: unknown[]) => mocks.resolveGeneratorTarget(...args),
  };
});

import { emptyServerDraft, useMcpGeneratorStore } from "./mcpGeneratorStore";

beforeAll(() => {
  const values = new Map<string, string>();
  Object.defineProperty(globalThis, "localStorage", {
    configurable: true,
    value: {
      clear: () => values.clear(),
      getItem: (key: string) => values.get(key) ?? null,
      setItem: (key: string, value: string) => values.set(key, value),
      removeItem: (key: string) => values.delete(key),
    },
  });
  Object.defineProperty(globalThis, "crypto", {
    configurable: true,
    value: { ...globalThis.crypto, randomUUID: () => "generated-id" },
  });
});

function validDraft() {
  return {
    name: "weather-cli",
    description: "Wraps the local weather CLI.",
    sourceKind: "cli" as const,
    target: "/usr/local/bin/weather",
    tools: [
      {
        name: "get_forecast",
        description: "Get a forecast for a city.",
        requiresAuth: false,
        params: [{ name: "city", type: "string" as const, required: true, description: "" }],
      },
    ],
  };
}

beforeEach(() => {
  localStorage.clear();
  mocks.generateMcpServerCode.mockReset();
  mocks.probeGeneratedMcpArtifact.mockReset();
  mocks.resolveGeneratorTarget.mockReset();
  mocks.save.mockReset();
  mocks.writeTextFile.mockReset();
  mocks.probeGeneratedMcpArtifact.mockResolvedValue({
    clean: true,
    runId: "probe-run",
    isolation: "os_sandboxed",
    typechecked: true,
    executed: true,
    probedToolCount: 1,
    summary: "verified",
    stdoutExcerpt: "LITTLE_MONKEY_MCP_TYPECHECK_OK\nLITTLE_MONKEY_MCP_PROBE_OK:1",
    stderrExcerpt: "",
  });
  useMcpGeneratorStore.setState({
    draft: emptyServerDraft(),
    entries: [],
    selectedEntryId: null,
    generating: false,
    simulating: false,
    saving: false,
    error: null,
  });
});

describe("draft editing", () => {
  it("adds and removes tools and params", () => {
    const store = useMcpGeneratorStore.getState();
    expect(store.draft.tools).toHaveLength(1);
    store.addTool();
    expect(useMcpGeneratorStore.getState().draft.tools).toHaveLength(2);
    useMcpGeneratorStore.getState().removeTool(1);
    expect(useMcpGeneratorStore.getState().draft.tools).toHaveLength(1);

    useMcpGeneratorStore.getState().updateTool(0, { name: "get_forecast" });
    expect(useMcpGeneratorStore.getState().draft.tools[0].name).toBe("get_forecast");

    useMcpGeneratorStore.getState().addParam(0);
    expect(useMcpGeneratorStore.getState().draft.tools[0].params).toHaveLength(1);
    useMcpGeneratorStore.getState().updateParam(0, 0, { name: "city" });
    expect(useMcpGeneratorStore.getState().draft.tools[0].params[0].name).toBe("city");
    useMcpGeneratorStore.getState().removeParam(0, 0);
    expect(useMcpGeneratorStore.getState().draft.tools[0].params).toHaveLength(0);
  });
});

describe("generate", () => {
  it("rejects an invalid draft without calling the model", async () => {
    await expect(useMcpGeneratorStore.getState().generate()).rejects.toThrow();
    expect(mocks.resolveGeneratorTarget).not.toHaveBeenCalled();
    expect(useMcpGeneratorStore.getState().error).toBeTruthy();
  });

  it("generates code and stores a new not-yet-simulated entry", async () => {
    useMcpGeneratorStore.setState({ draft: validDraft() });
    mocks.resolveGeneratorTarget.mockResolvedValue({ kind: "provider", providerId: "openai", model: "gpt" });
    mocks.generateMcpServerCode.mockResolvedValue("// generated code");

    const entry = await useMcpGeneratorStore.getState().generate();
    expect(entry.code).toBe("// generated code");
    expect(entry.ready).toBe(false);
    expect(entry.simulation).toBeNull();
    expect(entry.artifactProbe).toBeNull();
    expect(useMcpGeneratorStore.getState().entries).toHaveLength(1);
    expect(useMcpGeneratorStore.getState().selectedEntryId).toBe(entry.id);
    expect(JSON.parse(localStorage.getItem("little-monkey-mcp-generator-v1")!).entries).toHaveLength(1);
  });

  it("surfaces a generation error", async () => {
    useMcpGeneratorStore.setState({ draft: validDraft() });
    mocks.resolveGeneratorTarget.mockResolvedValue({ kind: "provider", providerId: "openai", model: "gpt" });
    mocks.generateMcpServerCode.mockRejectedValue(new Error("model unavailable"));

    await expect(useMcpGeneratorStore.getState().generate()).rejects.toThrow("model unavailable");
    expect(useMcpGeneratorStore.getState().error).toBe("model unavailable");
    expect(useMcpGeneratorStore.getState().generating).toBe(false);
  });
});

describe("runSimulator", () => {
  async function seedEntry() {
    useMcpGeneratorStore.setState({ draft: validDraft() });
    mocks.resolveGeneratorTarget.mockResolvedValue({ kind: "provider", providerId: "openai", model: "gpt" });
    mocks.generateMcpServerCode.mockResolvedValue("// generated code");
    return useMcpGeneratorStore.getState().generate();
  }

  it("marks an entry ready only after simulating and probing generated code", async () => {
    const entry = await seedEntry();
    await useMcpGeneratorStore.getState().runSimulator(entry.id);
    const updated = useMcpGeneratorStore.getState().entries.find((e) => e.id === entry.id)!;
    expect(updated.simulation?.clean).toBe(true);
    expect(updated.artifactProbe?.clean).toBe(true);
    expect(updated.ready).toBe(true);
  });

  it("never marks spec-only simulation ready when generated artifact execution fails", async () => {
    const entry = await seedEntry();
    mocks.probeGeneratedMcpArtifact.mockResolvedValue({
      clean: false,
      runId: "probe-failed",
      isolation: "os_sandboxed",
      typechecked: true,
      executed: false,
      probedToolCount: 0,
      summary: "runtime failed",
      stdoutExcerpt: "LITTLE_MONKEY_MCP_TYPECHECK_OK",
      stderrExcerpt: "boom",
    });
    await useMcpGeneratorStore.getState().runSimulator(entry.id);
    const updated = useMcpGeneratorStore.getState().entries.find((candidate) => candidate.id === entry.id)!;
    expect(updated.simulation?.clean).toBe(true);
    expect(updated.artifactProbe?.clean).toBe(false);
    expect(updated.ready).toBe(false);
  });

  it("invalidates an earlier ready verdict before a probe retry that throws", async () => {
    const entry = await seedEntry();
    await useMcpGeneratorStore.getState().runSimulator(entry.id);
    expect(useMcpGeneratorStore.getState().entries.find((candidate) => candidate.id === entry.id)?.ready).toBe(true);

    mocks.probeGeneratedMcpArtifact.mockRejectedValue(new Error("sandbox unavailable"));
    await useMcpGeneratorStore.getState().runSimulator(entry.id);

    const updated = useMcpGeneratorStore.getState().entries.find((candidate) => candidate.id === entry.id)!;
    expect(updated.ready).toBe(false);
    expect(updated.artifactProbe).toBeNull();
    expect(useMcpGeneratorStore.getState().error).toBe("sandbox unavailable");
  });

  it("does not mark an entry ready when the underlying spec would fail simulation", async () => {
    useMcpGeneratorStore.setState({
      draft: { ...validDraft(), tools: [{ name: "bad tool name", description: "x", requiresAuth: false, params: [] }] },
    });
    // Bypass the store's own pre-generate validation to seed an entry with an
    // invalid spec directly, so runSimulator's own guard is what's under test.
    useMcpGeneratorStore.setState({
      entries: [{
        id: "bad-entry",
        spec: { ...validDraft(), tools: [{ name: "bad tool name", description: "x", requiresAuth: false, params: [] }] },
        code: "// code",
        simulation: null,
        artifactProbe: null,
        ready: false,
        savedPath: null,
        createdAt: 1,
        updatedAt: 1,
      }],
    });
    await useMcpGeneratorStore.getState().runSimulator("bad-entry");
    const updated = useMcpGeneratorStore.getState().entries.find((e) => e.id === "bad-entry")!;
    expect(updated.ready).toBe(false);
    expect(useMcpGeneratorStore.getState().error).toBeTruthy();
  });
});

describe("saveToDisk", () => {
  async function seedReadyEntry() {
    useMcpGeneratorStore.setState({ draft: validDraft() });
    mocks.resolveGeneratorTarget.mockResolvedValue({ kind: "provider", providerId: "openai", model: "gpt" });
    mocks.generateMcpServerCode.mockResolvedValue("// generated code");
    const entry = await useMcpGeneratorStore.getState().generate();
    await useMcpGeneratorStore.getState().runSimulator(entry.id);
    return entry.id;
  }

  it("blocks saving a server that has not passed the simulator", async () => {
    useMcpGeneratorStore.setState({ draft: validDraft() });
    mocks.resolveGeneratorTarget.mockResolvedValue({ kind: "provider", providerId: "openai", model: "gpt" });
    mocks.generateMcpServerCode.mockResolvedValue("// generated code");
    const entry = await useMcpGeneratorStore.getState().generate();

    await expect(useMcpGeneratorStore.getState().saveToDisk(entry.id)).rejects.toThrow(/simulator/i);
    expect(mocks.save).not.toHaveBeenCalled();
  });

  it("saves a simulator-clean server via the save dialog and writeTextFile", async () => {
    const entryId = await seedReadyEntry();
    mocks.save.mockResolvedValue("/Users/test/weather-cli.mcp.ts");
    mocks.writeTextFile.mockResolvedValue(undefined);

    const path = await useMcpGeneratorStore.getState().saveToDisk(entryId);
    expect(path).toBe("/Users/test/weather-cli.mcp.ts");
    expect(mocks.writeTextFile).toHaveBeenCalledWith("/Users/test/weather-cli.mcp.ts", "// generated code");
    expect(useMcpGeneratorStore.getState().entries.find((e) => e.id === entryId)?.savedPath).toBe(
      "/Users/test/weather-cli.mcp.ts",
    );
  });

  it("returns null without writing when the user cancels the save dialog", async () => {
    const entryId = await seedReadyEntry();
    mocks.save.mockResolvedValue(null);

    const path = await useMcpGeneratorStore.getState().saveToDisk(entryId);
    expect(path).toBeNull();
    expect(mocks.writeTextFile).not.toHaveBeenCalled();
  });
});

describe("selectEntry and removeEntry", () => {
  it("selects and removes entries, clearing selection if the selected one is removed", async () => {
    useMcpGeneratorStore.setState({ draft: validDraft() });
    mocks.resolveGeneratorTarget.mockResolvedValue({ kind: "provider", providerId: "openai", model: "gpt" });
    mocks.generateMcpServerCode.mockResolvedValue("// generated code");
    const entry = await useMcpGeneratorStore.getState().generate();

    useMcpGeneratorStore.getState().selectEntry(entry.id);
    expect(useMcpGeneratorStore.getState().selectedEntryId).toBe(entry.id);

    useMcpGeneratorStore.getState().removeEntry(entry.id);
    expect(useMcpGeneratorStore.getState().entries).toHaveLength(0);
    expect(useMcpGeneratorStore.getState().selectedEntryId).toBeNull();
  });
});
