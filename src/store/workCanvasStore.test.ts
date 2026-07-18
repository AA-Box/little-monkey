import { beforeAll, beforeEach, describe, expect, it } from "vitest";

import { loadCanvasState } from "../lib/workCanvas";
import { selectActiveBoard, useWorkCanvasStore } from "./workCanvasStore";

// Same in-memory localStorage shim as workflowDraftStore.test.ts /
// workCanvas.test.ts — vitest's "node" environment has no real one.
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
});

beforeEach(() => {
  localStorage.clear();
  useWorkCanvasStore.setState({ boards: [], activeBoardId: null, selectedNodeId: null, connectingFromNodeId: null });
});

describe("workCanvasStore", () => {
  it("creates a board and makes it active", () => {
    const id = useWorkCanvasStore.getState().createBoard("Architecture map");
    const state = useWorkCanvasStore.getState();
    expect(state.activeBoardId).toBe(id);
    expect(selectActiveBoard(state)?.name).toBe("Architecture map");
  });

  it("persists boards to localStorage on every mutation", () => {
    const id = useWorkCanvasStore.getState().createBoard("Board");
    useWorkCanvasStore.getState().addNode({ type: "note", text: "hello", x: 10, y: 10 });
    const persisted = loadCanvasState();
    expect(persisted.activeBoardId).toBe(id);
    expect(persisted.boards[0].nodes).toHaveLength(1);
    expect(persisted.boards[0].nodes[0]).toMatchObject({ type: "note", text: "hello" });
  });

  it("refuses to add a node with no active board", () => {
    const result = useWorkCanvasStore.getState().addNode({ type: "note", text: "x", x: 0, y: 0 });
    expect(result).toBeNull();
    expect(useWorkCanvasStore.getState().boards).toEqual([]);
  });

  it("selects the newly added node", () => {
    useWorkCanvasStore.getState().createBoard("Board");
    const nodeId = useWorkCanvasStore.getState().addNode({ type: "note", text: "x", x: 0, y: 0 });
    expect(useWorkCanvasStore.getState().selectedNodeId).toBe(nodeId);
  });

  it("moves a node within the active board", () => {
    useWorkCanvasStore.getState().createBoard("Board");
    const nodeId = useWorkCanvasStore.getState().addNode({ type: "note", text: "x", x: 0, y: 0 })!;
    useWorkCanvasStore.getState().moveNode(nodeId, 40, 80);
    const board = selectActiveBoard(useWorkCanvasStore.getState())!;
    expect(board.nodes[0]).toMatchObject({ x: 40, y: 80 });
  });

  it("connects two nodes via startConnecting/completeConnecting", () => {
    useWorkCanvasStore.getState().createBoard("Board");
    const a = useWorkCanvasStore.getState().addNode({ type: "note", text: "a", x: 0, y: 0 })!;
    const b = useWorkCanvasStore.getState().addNode({ type: "note", text: "b", x: 100, y: 0 })!;
    useWorkCanvasStore.getState().startConnecting(a);
    expect(useWorkCanvasStore.getState().connectingFromNodeId).toBe(a);
    useWorkCanvasStore.getState().completeConnecting(b);
    expect(useWorkCanvasStore.getState().connectingFromNodeId).toBeNull();
    const board = selectActiveBoard(useWorkCanvasStore.getState())!;
    expect(board.edges).toHaveLength(1);
    expect([board.edges[0].fromNodeId, board.edges[0].toNodeId].sort()).toEqual([a, b].sort());
  });

  it("cancelConnecting clears the in-progress gesture without creating an edge", () => {
    useWorkCanvasStore.getState().createBoard("Board");
    const a = useWorkCanvasStore.getState().addNode({ type: "note", text: "a", x: 0, y: 0 })!;
    useWorkCanvasStore.getState().startConnecting(a);
    useWorkCanvasStore.getState().cancelConnecting();
    expect(useWorkCanvasStore.getState().connectingFromNodeId).toBeNull();
    expect(selectActiveBoard(useWorkCanvasStore.getState())!.edges).toEqual([]);
  });

  it("removing a node clears selection and any pending connect gesture referencing it", () => {
    useWorkCanvasStore.getState().createBoard("Board");
    const a = useWorkCanvasStore.getState().addNode({ type: "note", text: "a", x: 0, y: 0 })!;
    useWorkCanvasStore.getState().startConnecting(a);
    useWorkCanvasStore.getState().removeNode(a);
    expect(useWorkCanvasStore.getState().selectedNodeId).toBeNull();
    expect(useWorkCanvasStore.getState().connectingFromNodeId).toBeNull();
    expect(selectActiveBoard(useWorkCanvasStore.getState())!.nodes).toEqual([]);
  });

  it("deleteBoard falls back to another board or null activeBoardId", () => {
    const first = useWorkCanvasStore.getState().createBoard("First");
    const second = useWorkCanvasStore.getState().createBoard("Second");
    useWorkCanvasStore.getState().deleteBoard(second);
    expect(useWorkCanvasStore.getState().activeBoardId).toBe(first);
    useWorkCanvasStore.getState().deleteBoard(first);
    expect(useWorkCanvasStore.getState().activeBoardId).toBeNull();
    expect(useWorkCanvasStore.getState().boards).toEqual([]);
  });

  it("renames a board", () => {
    const id = useWorkCanvasStore.getState().createBoard("Old name");
    useWorkCanvasStore.getState().renameBoard(id, "New name");
    expect(selectActiveBoard(useWorkCanvasStore.getState())?.name).toBe("New name");
  });

  it("updates sticky-note text", () => {
    useWorkCanvasStore.getState().createBoard("Board");
    const nodeId = useWorkCanvasStore.getState().addNode({ type: "note", text: "a", x: 0, y: 0 })!;
    useWorkCanvasStore.getState().updateNoteText(nodeId, "b");
    const board = selectActiveBoard(useWorkCanvasStore.getState())!;
    expect(board.nodes[0]).toMatchObject({ text: "b" });
  });

  it("sets the viewport", () => {
    useWorkCanvasStore.getState().createBoard("Board");
    useWorkCanvasStore.getState().setViewport({ x: 5, y: 5, zoom: 1.2 });
    expect(selectActiveBoard(useWorkCanvasStore.getState())?.viewport).toEqual({ x: 5, y: 5, zoom: 1.2 });
  });

  it("removes an edge", () => {
    useWorkCanvasStore.getState().createBoard("Board");
    const a = useWorkCanvasStore.getState().addNode({ type: "note", text: "a", x: 0, y: 0 })!;
    const b = useWorkCanvasStore.getState().addNode({ type: "note", text: "b", x: 0, y: 0 })!;
    useWorkCanvasStore.getState().addEdge(a, b);
    const edgeId = selectActiveBoard(useWorkCanvasStore.getState())!.edges[0].id;
    useWorkCanvasStore.getState().removeEdge(edgeId);
    expect(selectActiveBoard(useWorkCanvasStore.getState())!.edges).toEqual([]);
  });
});
