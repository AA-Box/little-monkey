import { beforeEach, describe, expect, it, vi } from "vitest";

// `spreadsheetCopilotStore.ts` drives its one-shot proposal call through
// `agentLoop.ts`'s `resolveTarget` + `turnEngine.ts`'s `attemptStream` —
// exactly the same pair `sopCompilerStore.ts` mocks for its own one-shot
// compiler call — mocked here so these tests pin the STORE's own behavior
// (file load, propose/approve/reject lifecycle, the write-on-approve gate)
// without needing a real streaming provider.
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
const writeTextFileMock = vi.fn();
vi.mock("@tauri-apps/plugin-fs", () => ({
  readTextFile: (...args: unknown[]) => readTextFileMock(...args),
  stat: (...args: unknown[]) => statMock(...args),
  writeTextFile: (...args: unknown[]) => writeTextFileMock(...args),
}));

import { useSpreadsheetCopilotStore } from "./spreadsheetCopilotStore";

const SAMPLE_CSV = "name,quantity,price\nWidget,2,9.99\nGadget,5,4.50\n";

const WELL_FORMED_REPLY = JSON.stringify({
  kind: "derived_column",
  title: "Add line total",
  explanation: "quantity * price per row",
  citedReadRanges: ["B2:B3", "C2:C3"],
  writes: [
    { ref: "D1", value: "Line Total" },
    { ref: "D2", value: "19.98" },
    { ref: "D3", value: "22.50" },
  ],
});

describe("spreadsheetCopilotStore", () => {
  beforeEach(() => {
    resolveTargetMock.mockReset();
    attemptStreamMock.mockReset();
    dialogOpenMock.mockReset();
    readTextFileMock.mockReset();
    statMock.mockReset();
    writeTextFileMock.mockReset();
    resolveTargetMock.mockResolvedValue({ kind: "local", baseUrl: "http://localhost:8090" });
    useSpreadsheetCopilotStore.setState({
      filePath: null,
      fileName: null,
      table: null,
      requestText: "",
      proposal: null,
      loadingFile: false,
      proposing: false,
      approving: false,
      error: null,
    });
  });

  it("loads a CSV file's contents into a parsed table via the dialog + fs plugins", async () => {
    dialogOpenMock.mockResolvedValue("/Users/me/orders.csv");
    statMock.mockResolvedValue({ size: 1024 });
    readTextFileMock.mockResolvedValue(SAMPLE_CSV);

    await useSpreadsheetCopilotStore.getState().loadFromFile();

    const state = useSpreadsheetCopilotStore.getState();
    expect(state.filePath).toBe("/Users/me/orders.csv");
    expect(state.fileName).toBe("orders.csv");
    expect(state.table).toEqual({
      headers: ["name", "quantity", "price"],
      rows: [
        ["Widget", "2", "9.99"],
        ["Gadget", "5", "4.50"],
      ],
    });
    expect(state.loadingFile).toBe(false);
    expect(state.error).toBeNull();
  });

  it("rejects a CSV import above the file size limit without reading it", async () => {
    dialogOpenMock.mockResolvedValue("/Users/me/huge.csv");
    statMock.mockResolvedValue({ size: 50 * 1024 * 1024 });

    await useSpreadsheetCopilotStore.getState().loadFromFile();

    expect(readTextFileMock).not.toHaveBeenCalled();
    expect(useSpreadsheetCopilotStore.getState().error).toMatch(/larger than/i);
  });

  it("does nothing when the user cancels the file picker", async () => {
    dialogOpenMock.mockResolvedValue(null);
    await useSpreadsheetCopilotStore.getState().loadFromFile();
    expect(statMock).not.toHaveBeenCalled();
    expect(useSpreadsheetCopilotStore.getState().table).toBeNull();
  });

  it("refuses to propose without a loaded table, never calling the model", async () => {
    useSpreadsheetCopilotStore.getState().setRequestText("add a total column");
    await useSpreadsheetCopilotStore.getState().propose();
    expect(attemptStreamMock).not.toHaveBeenCalled();
    expect(useSpreadsheetCopilotStore.getState().error).toMatch(/load a csv file/i);
  });

  it("refuses to propose with a blank request, never calling the model", async () => {
    useSpreadsheetCopilotStore.setState({ table: { headers: ["a"], rows: [["1"]] } });
    await useSpreadsheetCopilotStore.getState().propose();
    expect(attemptStreamMock).not.toHaveBeenCalled();
    expect(useSpreadsheetCopilotStore.getState().error).toMatch(/describe the operation/i);
  });

  it("propose() sets a citing proposal and does NOT write the file", async () => {
    attemptStreamMock.mockResolvedValue({ content: WELL_FORMED_REPLY, streamError: null, toolCalls: [], contentStarted: true });
    useSpreadsheetCopilotStore.setState({
      filePath: "/Users/me/orders.csv",
      fileName: "orders.csv",
      table: parseSample(),
    });
    useSpreadsheetCopilotStore.getState().setRequestText("add a line total column");

    await useSpreadsheetCopilotStore.getState().propose();

    const state = useSpreadsheetCopilotStore.getState();
    expect(state.proposing).toBe(false);
    expect(state.error).toBeNull();
    expect(state.proposal).not.toBeNull();
    expect(state.proposal!.citedRanges.length).toBeGreaterThan(0);
    expect(writeTextFileMock).not.toHaveBeenCalled();

    // `recordUsage` (8th positional arg) threaded through as `false` — this
    // one-shot proposal call is not a chat turn.
    const call = attemptStreamMock.mock.calls[0];
    expect(call[7]).toBe(false);
  });

  it("surfaces a proposal error and never fabricates one", async () => {
    attemptStreamMock.mockResolvedValue({ content: "not json", streamError: null, toolCalls: [], contentStarted: true });
    useSpreadsheetCopilotStore.setState({ filePath: "/Users/me/orders.csv", fileName: "orders.csv", table: parseSample() });
    useSpreadsheetCopilotStore.getState().setRequestText("add a line total column");

    await useSpreadsheetCopilotStore.getState().propose();

    expect(useSpreadsheetCopilotStore.getState().proposal).toBeNull();
    expect(useSpreadsheetCopilotStore.getState().error).toMatch(/did not return a usable/i);
  });

  it("approve() writes the proposed table back to the loaded file path and clears the proposal", async () => {
    attemptStreamMock.mockResolvedValue({ content: WELL_FORMED_REPLY, streamError: null, toolCalls: [], contentStarted: true });
    useSpreadsheetCopilotStore.setState({
      filePath: "/Users/me/orders.csv",
      fileName: "orders.csv",
      table: parseSample(),
    });
    useSpreadsheetCopilotStore.getState().setRequestText("add a line total column");
    await useSpreadsheetCopilotStore.getState().propose();
    const proposedTable = useSpreadsheetCopilotStore.getState().proposal!.proposedTable;

    await useSpreadsheetCopilotStore.getState().approve();

    expect(writeTextFileMock).toHaveBeenCalledTimes(1);
    expect(writeTextFileMock.mock.calls[0][0]).toBe("/Users/me/orders.csv");
    expect(writeTextFileMock.mock.calls[0][1]).toContain("Line Total");

    const state = useSpreadsheetCopilotStore.getState();
    expect(state.proposal).toBeNull();
    expect(state.table).toEqual(proposedTable);
    expect(state.approving).toBe(false);
  });

  it("approve() is a no-op without a pending proposal", async () => {
    useSpreadsheetCopilotStore.setState({ filePath: "/Users/me/orders.csv", table: parseSample() });
    await useSpreadsheetCopilotStore.getState().approve();
    expect(writeTextFileMock).not.toHaveBeenCalled();
  });

  it("reject() clears the pending proposal without touching the file", async () => {
    attemptStreamMock.mockResolvedValue({ content: WELL_FORMED_REPLY, streamError: null, toolCalls: [], contentStarted: true });
    useSpreadsheetCopilotStore.setState({
      filePath: "/Users/me/orders.csv",
      fileName: "orders.csv",
      table: parseSample(),
    });
    useSpreadsheetCopilotStore.getState().setRequestText("add a line total column");
    await useSpreadsheetCopilotStore.getState().propose();
    expect(useSpreadsheetCopilotStore.getState().proposal).not.toBeNull();

    useSpreadsheetCopilotStore.getState().reject();

    expect(useSpreadsheetCopilotStore.getState().proposal).toBeNull();
    expect(writeTextFileMock).not.toHaveBeenCalled();
  });

  it("reset() clears the whole store back to its initial state", () => {
    useSpreadsheetCopilotStore.setState({
      filePath: "/Users/me/orders.csv",
      fileName: "orders.csv",
      table: parseSample(),
      requestText: "add a column",
      error: "some error",
    });
    useSpreadsheetCopilotStore.getState().reset();
    const state = useSpreadsheetCopilotStore.getState();
    expect(state.filePath).toBeNull();
    expect(state.table).toBeNull();
    expect(state.requestText).toBe("");
    expect(state.error).toBeNull();
  });
});

function parseSample() {
  return {
    headers: ["name", "quantity", "price"],
    rows: [
      ["Widget", "2", "9.99"],
      ["Gadget", "5", "4.50"],
    ],
  };
}
