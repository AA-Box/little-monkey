import { beforeEach, describe, expect, it, vi } from "vitest";

// `apiContractDiffStore.ts` drives its one-shot client-impact-note call
// through `agentLoop.ts`'s `resolveTarget` + `turnEngine.ts`'s
// `attemptStream` — the same pair `sopCompilerStore.ts` uses for its own
// one-shot compiler call — mocked here so these tests pin the STORE's own
// behavior (file loading, diffing, note drafting) without needing a real
// streaming provider.
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

import { useApiContractDiffStore } from "./apiContractDiffStore";

const OLD_SPEC = JSON.stringify({
  openapi: "3.0.0",
  info: { title: "Widgets API", version: "1.0.0" },
  paths: {
    "/widgets": {
      get: {
        operationId: "listWidgets",
        parameters: [{ name: "limit", in: "query", required: false, schema: { type: "integer" } }],
        responses: {
          "200": {
            content: {
              "application/json": {
                schema: { type: "object", properties: { name: { type: "string" } }, required: ["name"] },
              },
            },
          },
        },
      },
    },
  },
});

const NEW_SPEC = JSON.stringify({
  openapi: "3.0.0",
  info: { title: "Widgets API", version: "2.0.0" },
  paths: {
    "/widgets": {
      get: {
        operationId: "listWidgets",
        parameters: [{ name: "limit", in: "query", required: true, schema: { type: "integer" } }],
        responses: {
          "200": {
            content: {
              "application/json": {
                schema: { type: "object", properties: {}, required: [] },
              },
            },
          },
        },
      },
    },
  },
});

describe("apiContractDiffStore", () => {
  beforeEach(() => {
    resolveTargetMock.mockReset();
    attemptStreamMock.mockReset();
    dialogOpenMock.mockReset();
    readTextFileMock.mockReset();
    statMock.mockReset();
    resolveTargetMock.mockResolvedValue({ kind: "local", baseUrl: "http://localhost:8090" });
    useApiContractDiffStore.setState({
      oldSpec: null,
      newSpec: null,
      loadingSlot: null,
      loadError: null,
      changes: [],
      mocks: [],
      testStub: "",
      hasRun: false,
      diffError: null,
      drafting: false,
      draftError: null,
      impactNotes: [],
    });
  });

  it("loads a spec file into the requested slot via the dialog + fs plugins", async () => {
    dialogOpenMock.mockResolvedValue("/Users/me/old.json");
    statMock.mockResolvedValue({ size: 1024 });
    readTextFileMock.mockResolvedValue(OLD_SPEC);

    await useApiContractDiffStore.getState().loadFile("old");

    const state = useApiContractDiffStore.getState();
    expect(state.oldSpec?.fileName).toBe("old.json");
    expect(state.oldSpec?.doc.title).toBe("Widgets API");
    expect(state.loadingSlot).toBeNull();
    expect(state.loadError).toBeNull();
  });

  it("does nothing when the user cancels the file picker", async () => {
    dialogOpenMock.mockResolvedValue(null);
    await useApiContractDiffStore.getState().loadFile("new");
    expect(statMock).not.toHaveBeenCalled();
    expect(useApiContractDiffStore.getState().newSpec).toBeNull();
  });

  it("rejects an import above the file size limit", async () => {
    dialogOpenMock.mockResolvedValue("/Users/me/huge.json");
    statMock.mockResolvedValue({ size: 50 * 1024 * 1024 });

    await useApiContractDiffStore.getState().loadFile("old");

    expect(readTextFileMock).not.toHaveBeenCalled();
    expect(useApiContractDiffStore.getState().loadError).toMatch(/larger than/i);
  });

  it("surfaces a parse error via loadError rather than an unhandled rejection", async () => {
    dialogOpenMock.mockResolvedValue("/Users/me/broken.json");
    statMock.mockResolvedValue({ size: 10 });
    readTextFileMock.mockResolvedValue("not an openapi document");

    await useApiContractDiffStore.getState().loadFile("old");

    expect(useApiContractDiffStore.getState().loadError).toBeTruthy();
    expect(useApiContractDiffStore.getState().oldSpec).toBeNull();
  });

  it("refuses to diff until both specs are loaded", () => {
    useApiContractDiffStore.getState().runDiff();
    expect(useApiContractDiffStore.getState().diffError).toMatch(/load both/i);
    expect(useApiContractDiffStore.getState().changes).toHaveLength(0);
  });

  it("runs a diff, generates mocks and a test stub once both specs are loaded", async () => {
    dialogOpenMock.mockResolvedValueOnce("/Users/me/old.json").mockResolvedValueOnce("/Users/me/new.json");
    statMock.mockResolvedValue({ size: 10 });
    readTextFileMock.mockResolvedValueOnce(OLD_SPEC).mockResolvedValueOnce(NEW_SPEC);

    await useApiContractDiffStore.getState().loadFile("old");
    await useApiContractDiffStore.getState().loadFile("new");
    useApiContractDiffStore.getState().runDiff();

    const state = useApiContractDiffStore.getState();
    expect(state.changes.length).toBeGreaterThan(0);
    expect(state.changes.some((c) => c.severity === "breaking")).toBe(true);
    expect(state.mocks.length).toBeGreaterThan(0);
    expect(state.testStub).toContain("vitest");
    expect(state.hasRun).toBe(true);
  });

  it("loading a new file after a diff clears the stale report", async () => {
    dialogOpenMock.mockResolvedValueOnce("/Users/me/old.json").mockResolvedValueOnce("/Users/me/new.json");
    statMock.mockResolvedValue({ size: 10 });
    readTextFileMock.mockResolvedValueOnce(OLD_SPEC).mockResolvedValueOnce(NEW_SPEC);
    await useApiContractDiffStore.getState().loadFile("old");
    await useApiContractDiffStore.getState().loadFile("new");
    useApiContractDiffStore.getState().runDiff();
    expect(useApiContractDiffStore.getState().changes.length).toBeGreaterThan(0);

    dialogOpenMock.mockResolvedValueOnce("/Users/me/new2.json");
    readTextFileMock.mockResolvedValueOnce(NEW_SPEC);
    await useApiContractDiffStore.getState().loadFile("new");

    expect(useApiContractDiffStore.getState().changes).toHaveLength(0);
  });

  it("refuses to draft impact notes without ever calling the model when there are no breaking changes", async () => {
    useApiContractDiffStore.setState({ changes: [{ id: "c1", severity: "non-breaking", kind: "endpoint-added", operationLabel: "GET /x", detail: "added" }] });
    await useApiContractDiffStore.getState().draftImpactNotes();
    expect(attemptStreamMock).not.toHaveBeenCalled();
    expect(useApiContractDiffStore.getState().draftError).toMatch(/no breaking changes/i);
  });

  it("drafts client-impact notes for breaking changes via the local-model call", async () => {
    useApiContractDiffStore.setState({
      changes: [{ id: "c1", severity: "breaking", kind: "field-removed", operationLabel: "GET /widgets", detail: "field `name` removed" }],
    });
    attemptStreamMock.mockResolvedValue({
      content: JSON.stringify([{ id: "c1", impact: "Clients reading `name` will break.", migration: "Stop reading `name`." }]),
      streamError: null,
      toolCalls: [],
      contentStarted: true,
    });

    await useApiContractDiffStore.getState().draftImpactNotes();

    const state = useApiContractDiffStore.getState();
    expect(state.drafting).toBe(false);
    expect(state.draftError).toBeNull();
    expect(state.impactNotes).toHaveLength(1);
    expect(state.impactNotes[0].impact).toMatch(/break/i);

    // `recordUsage` (8th positional arg) must be `false` — this one-shot
    // note-drafting call is not a chat turn and must never pollute a real
    // session's usage ledger.
    expect(attemptStreamMock).toHaveBeenCalledTimes(1);
    expect(attemptStreamMock.mock.calls[0][7]).toBe(false);
  });

  it("surfaces a drafting error rather than fabricating notes", async () => {
    useApiContractDiffStore.setState({
      changes: [{ id: "c1", severity: "breaking", kind: "field-removed", operationLabel: "GET /widgets", detail: "field `name` removed" }],
    });
    attemptStreamMock.mockResolvedValue({ content: "not json", streamError: null, toolCalls: [], contentStarted: true });

    await useApiContractDiffStore.getState().draftImpactNotes();

    expect(useApiContractDiffStore.getState().impactNotes).toHaveLength(0);
    expect(useApiContractDiffStore.getState().draftError).toMatch(/did not return/i);
  });

  it("reset clears every field back to its initial state", async () => {
    dialogOpenMock.mockResolvedValue("/Users/me/old.json");
    statMock.mockResolvedValue({ size: 10 });
    readTextFileMock.mockResolvedValue(OLD_SPEC);
    await useApiContractDiffStore.getState().loadFile("old");

    useApiContractDiffStore.getState().reset();

    const state = useApiContractDiffStore.getState();
    expect(state.oldSpec).toBeNull();
    expect(state.newSpec).toBeNull();
    expect(state.changes).toHaveLength(0);
  });
});
