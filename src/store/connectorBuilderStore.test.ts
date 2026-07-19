import { beforeEach, describe, expect, it, vi } from "vitest";

const invokeMock = vi.fn();
vi.mock("@tauri-apps/api/core", () => ({ invoke: (...args: unknown[]) => invokeMock(...args), isTauri: () => false }));
vi.mock("@tauri-apps/api/event", () => ({ listen: () => Promise.resolve(() => {}) }));

const resolveTargetMock = vi.fn();
vi.mock("../lib/agentLoop", () => ({
  resolveTarget: (...args: unknown[]) => resolveTargetMock(...args),
}));

const attemptStreamMock = vi.fn();
vi.mock("../lib/turnEngine", () => ({
  attemptStream: (...args: unknown[]) => attemptStreamMock(...args),
}));

const dialogOpenMock = vi.fn();
vi.mock("@tauri-apps/plugin-dialog", () => ({
  open: (...args: unknown[]) => dialogOpenMock(...args),
}));

const readTextFileMock = vi.fn();
const statMock = vi.fn();
vi.mock("@tauri-apps/plugin-fs", () => ({
  readTextFile: (...args: unknown[]) => readTextFileMock(...args),
  stat: (...args: unknown[]) => statMock(...args),
}));

import { useConnectorBuilderStore } from "./connectorBuilderStore";
import { useMcpStore } from "./mcpStore";

const VALID_SPEC = JSON.stringify({
  openapi: "3.0.0",
  info: { title: "Widgets API", version: "1.0.0" },
  servers: [{ url: "https://api.widgets.example.com" }],
  paths: {
    "/widgets": {
      get: { operationId: "listWidgets", summary: "List widgets" },
    },
    "/widgets/{id}": {
      parameters: [{ name: "id", in: "path", required: true, schema: { type: "string" } }],
      delete: { operationId: "deleteWidget", summary: "Delete a widget" },
    },
  },
});

function resetState() {
  useConnectorBuilderStore.setState({
    specText: "",
    specFileName: null,
    importing: false,
    definition: null,
    summary: null,
    generating: false,
    drafting: false,
    simulation: null,
    simulating: false,
    ready: false,
    registering: false,
    registeredServerId: null,
    error: null,
  });
  useMcpStore.setState({ servers: [] });
}

beforeEach(() => {
  invokeMock.mockReset();
  resolveTargetMock.mockReset();
  attemptStreamMock.mockReset();
  dialogOpenMock.mockReset();
  readTextFileMock.mockReset();
  statMock.mockReset();
  resolveTargetMock.mockResolvedValue({ kind: "local" });
  resetState();
});

describe("connectorBuilderStore.generate", () => {
  it("refuses to generate from empty input without calling the model", async () => {
    await useConnectorBuilderStore.getState().generate();
    expect(useConnectorBuilderStore.getState().error).toMatch(/load or paste/i);
    expect(attemptStreamMock).not.toHaveBeenCalled();
  });

  it("parses a valid spec into a definition and drafts a best-effort summary", async () => {
    attemptStreamMock.mockResolvedValue({ content: "A tidy connector for widgets.", toolCalls: [], streamError: null });
    useConnectorBuilderStore.getState().setSpecText(VALID_SPEC);

    await useConnectorBuilderStore.getState().generate();

    const state = useConnectorBuilderStore.getState();
    expect(state.error).toBeNull();
    expect(state.definition?.server.tools.map((t) => t.name)).toEqual(["list_widgets", "delete_widget"]);
    expect(state.summary).toBe("A tidy connector for widgets.");
    expect(state.simulation).toBeNull();
    expect(state.ready).toBe(false);
  });

  it("surfaces a parse error and never fabricates a definition", async () => {
    useConnectorBuilderStore.getState().setSpecText("{ not: valid ] json");

    await useConnectorBuilderStore.getState().generate();

    const state = useConnectorBuilderStore.getState();
    expect(state.definition).toBeNull();
    expect(state.error).toBeTruthy();
    expect(attemptStreamMock).not.toHaveBeenCalled();
  });

  it("keeps the deterministic definition even when the summary draft fails", async () => {
    attemptStreamMock.mockResolvedValue({ content: "", toolCalls: [], streamError: "model unavailable" });
    useConnectorBuilderStore.getState().setSpecText(VALID_SPEC);

    await useConnectorBuilderStore.getState().generate();

    const state = useConnectorBuilderStore.getState();
    expect(state.definition).not.toBeNull();
    expect(state.summary).toBeNull();
    expect(state.error).toMatch(/summary draft failed/i);
  });
});

describe("connectorBuilderStore.runSimulator + registerWithMcp gating", () => {
  it("gates ready strictly on simulation.clean, and blocks registration until then", async () => {
    useConnectorBuilderStore.getState().setSpecText(VALID_SPEC);
    resolveTargetMock.mockResolvedValue({ kind: "local" });
    attemptStreamMock.mockResolvedValue({ content: "summary", toolCalls: [], streamError: null });
    await useConnectorBuilderStore.getState().generate();

    expect(useConnectorBuilderStore.getState().ready).toBe(false);
    await expect(useConnectorBuilderStore.getState().registerWithMcp()).rejects.toThrow(/simulator/i);
    expect(invokeMock).not.toHaveBeenCalled();

    useConnectorBuilderStore.getState().runSimulator();
    const afterSim = useConnectorBuilderStore.getState();
    expect(afterSim.simulation?.clean).toBe(true);
    expect(afterSim.ready).toBe(true);
  });

  it("registers a clean connector via the existing mcpStore.addServer path", async () => {
    useConnectorBuilderStore.getState().setSpecText(VALID_SPEC);
    attemptStreamMock.mockResolvedValue({ content: "summary", toolCalls: [], streamError: null });
    await useConnectorBuilderStore.getState().generate();
    useConnectorBuilderStore.getState().runSimulator();
    expect(useConnectorBuilderStore.getState().ready).toBe(true);

    invokeMock.mockImplementation((cmd: string) => {
      if (cmd === "mcp_add_server") return Promise.resolve();
      if (cmd === "mcp_list_servers") return Promise.resolve([]);
      return Promise.resolve();
    });

    const id = await useConnectorBuilderStore.getState().registerWithMcp();

    expect(id).toBe("widgets-api");
    expect(invokeMock).toHaveBeenCalledWith(
      "mcp_add_server",
      expect.objectContaining({
        entry: expect.objectContaining({
          id: "widgets-api",
          transport: { type: "http", url: "https://api.widgets.example.com" },
          enabled: true,
        }),
      }),
    );
    expect(useConnectorBuilderStore.getState().registeredServerId).toBe("widgets-api");
  });

  it("de-dupes the registered id against already-configured servers", async () => {
    useMcpStore.setState({
      servers: [
        {
          id: "widgets-api",
          label: "existing",
          transport: { type: "http", url: "https://existing.example.com" },
          enabled: true,
          toolAllowlist: null,
          timeoutSecs: null,
          status: "disconnected",
          error: null,
          tools: [],
          instructions: null,
          hasHttpToken: false,
          hasOauth: false,
        },
      ],
    });
    useConnectorBuilderStore.getState().setSpecText(VALID_SPEC);
    attemptStreamMock.mockResolvedValue({ content: "summary", toolCalls: [], streamError: null });
    await useConnectorBuilderStore.getState().generate();
    useConnectorBuilderStore.getState().runSimulator();

    invokeMock.mockImplementation((cmd: string) => {
      if (cmd === "mcp_add_server") return Promise.resolve();
      if (cmd === "mcp_list_servers") return Promise.resolve([]);
      return Promise.resolve();
    });

    const id = await useConnectorBuilderStore.getState().registerWithMcp();
    expect(id).toBe("widgets-api-2");
  });
});

describe("connectorBuilderStore.importFromFile", () => {
  it("loads spec text from the chosen file and clears any prior generation", async () => {
    dialogOpenMock.mockResolvedValue("/tmp/spec.yaml");
    statMock.mockResolvedValue({ size: 100 });
    readTextFileMock.mockResolvedValue("openapi: 3.0.0");

    await useConnectorBuilderStore.getState().importFromFile();

    const state = useConnectorBuilderStore.getState();
    expect(state.specText).toBe("openapi: 3.0.0");
    expect(state.specFileName).toBe("spec.yaml");
    expect(state.importing).toBe(false);
  });

  it("rejects an oversized file", async () => {
    dialogOpenMock.mockResolvedValue("/tmp/huge.json");
    statMock.mockResolvedValue({ size: 999_999_999 });

    await useConnectorBuilderStore.getState().importFromFile();

    expect(useConnectorBuilderStore.getState().error).toMatch(/larger than/i);
    expect(readTextFileMock).not.toHaveBeenCalled();
  });

  it("does nothing when the dialog is cancelled", async () => {
    dialogOpenMock.mockResolvedValue(null);

    await useConnectorBuilderStore.getState().importFromFile();

    const state = useConnectorBuilderStore.getState();
    expect(state.specText).toBe("");
    expect(state.importing).toBe(false);
    expect(state.error).toBeNull();
  });
});

describe("connectorBuilderStore.reset", () => {
  it("clears all generated state back to defaults", async () => {
    useConnectorBuilderStore.getState().setSpecText(VALID_SPEC);
    attemptStreamMock.mockResolvedValue({ content: "summary", toolCalls: [], streamError: null });
    await useConnectorBuilderStore.getState().generate();
    useConnectorBuilderStore.getState().runSimulator();

    useConnectorBuilderStore.getState().reset();

    const state = useConnectorBuilderStore.getState();
    expect(state.specText).toBe("");
    expect(state.definition).toBeNull();
    expect(state.simulation).toBeNull();
    expect(state.ready).toBe(false);
    expect(state.registeredServerId).toBeNull();
  });
});
