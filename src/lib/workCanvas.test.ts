import { beforeAll, beforeEach, describe, expect, it } from "vitest";

import {
  addEdge,
  addNode,
  clampZoom,
  createNode,
  describeNodeForSideTask,
  loadCanvasState,
  moveNode,
  newBoard,
  removeEdge,
  removeNode,
  renameBoard,
  saveCanvasState,
  setViewport,
  updateNoteText,
  type WorkCanvasBoard,
} from "./workCanvas";

// vitest's "node" environment has no `localStorage` global — stub an
// in-memory one, same shim `workflowDraftStore.test.ts` uses.
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
});

function boardWithNodes(): WorkCanvasBoard {
  let board = newBoard("Research");
  const a = createNode({ type: "note", text: "idea", x: 0, y: 0 });
  const b = createNode({ type: "file", path: "src/App.tsx", x: 200, y: 0 });
  board = addNode(board, a);
  board = addNode(board, b);
  return board;
}

describe("workCanvas board transforms", () => {
  it("creates a board with an empty node/edge set and default viewport", () => {
    const board = newBoard("  Plan  ");
    expect(board.name).toBe("Plan");
    expect(board.nodes).toEqual([]);
    expect(board.edges).toEqual([]);
    expect(board.viewport).toEqual({ x: 0, y: 0, zoom: 1 });
  });

  it("falls back to a default name when given only whitespace", () => {
    expect(newBoard("   ").name).toBe("Untitled board");
  });

  it("adds nodes of every supported type with distinct ids", () => {
    const chat = createNode({ type: "chat", sessionId: "s1", title: "Chat", x: 0, y: 0 });
    const file = createNode({ type: "file", path: "a.ts", x: 0, y: 0 });
    const run = createNode({ type: "run", runId: "r1", title: "Run", x: 0, y: 0 });
    const note = createNode({ type: "note", text: "hi", x: 0, y: 0 });
    const ids = new Set([chat.id, file.id, run.id, note.id]);
    expect(ids.size).toBe(4);
    expect(chat.type).toBe("chat");
    expect(file.type).toBe("file");
    expect(run.type).toBe("run");
    expect(note.type).toBe("note");
  });

  it("moves a node to new coordinates without touching others", () => {
    let board = boardWithNodes();
    const [a, b] = board.nodes;
    board = moveNode(board, a.id, 50, 60);
    expect(board.nodes.find((n) => n.id === a.id)).toMatchObject({ x: 50, y: 60 });
    expect(board.nodes.find((n) => n.id === b.id)).toMatchObject({ x: 200, y: 0 });
  });

  it("edits sticky-note text only for note-type nodes", () => {
    let board = boardWithNodes();
    const [note, file] = board.nodes;
    board = updateNoteText(board, note.id, "updated");
    board = updateNoteText(board, file.id, "should not apply");
    expect(board.nodes.find((n) => n.id === note.id)).toMatchObject({ text: "updated" });
    expect(board.nodes.find((n) => n.id === file.id)).not.toHaveProperty("text");
  });

  it("removes a node and any edge attached to it", () => {
    let board = boardWithNodes();
    const [a, b] = board.nodes;
    board = addEdge(board, a.id, b.id);
    expect(board.edges).toHaveLength(1);
    board = removeNode(board, a.id);
    expect(board.nodes.map((n) => n.id)).toEqual([b.id]);
    expect(board.edges).toEqual([]);
  });

  it("connects two nodes, ignoring self-loops, unknown ids, and duplicates in either direction", () => {
    let board = boardWithNodes();
    const [a, b] = board.nodes;

    board = addEdge(board, a.id, a.id);
    expect(board.edges).toHaveLength(0);

    board = addEdge(board, a.id, "does-not-exist");
    expect(board.edges).toHaveLength(0);

    board = addEdge(board, a.id, b.id);
    expect(board.edges).toHaveLength(1);

    board = addEdge(board, b.id, a.id);
    expect(board.edges).toHaveLength(1);
  });

  it("removes an edge by id", () => {
    let board = boardWithNodes();
    const [a, b] = board.nodes;
    board = addEdge(board, a.id, b.id);
    const [edge] = board.edges;
    board = removeEdge(board, edge.id);
    expect(board.edges).toEqual([]);
  });

  it("renames a board, ignoring a blank name", () => {
    let board = newBoard("Original");
    board = renameBoard(board, "  Renamed  ");
    expect(board.name).toBe("Renamed");
    board = renameBoard(board, "   ");
    expect(board.name).toBe("Renamed");
  });

  it("sets the viewport", () => {
    let board = newBoard("Plan");
    board = setViewport(board, { x: 10, y: -20, zoom: 1.5 });
    expect(board.viewport).toEqual({ x: 10, y: -20, zoom: 1.5 });
  });

  it("clamps zoom to the configured range", () => {
    expect(clampZoom(10)).toBe(2.5);
    expect(clampZoom(0.01)).toBe(0.25);
    expect(clampZoom(1)).toBe(1);
  });
});

describe("describeNodeForSideTask", () => {
  it("builds a title/prompt referencing each node type's own context", () => {
    const chat = createNode({ type: "chat", sessionId: "s1", title: "Refactor plan", x: 0, y: 0 });
    const file = createNode({ type: "file", path: "src/lib/foo.ts", x: 0, y: 0 });
    const run = createNode({ type: "run", runId: "r1", title: "Nightly build", x: 0, y: 0 });
    const note = createNode({ type: "note", text: "remember to check X", x: 0, y: 0 });

    expect(describeNodeForSideTask(chat).prompt).toContain("Refactor plan");
    expect(describeNodeForSideTask(chat).prompt).toContain("s1");
    expect(describeNodeForSideTask(file).prompt).toContain("src/lib/foo.ts");
    expect(describeNodeForSideTask(run).prompt).toContain("Nightly build");
    expect(describeNodeForSideTask(run).prompt).toContain("r1");
    expect(describeNodeForSideTask(note).prompt).toBe("remember to check X");
  });

  it("falls back to a generic prompt for an empty sticky note", () => {
    const note = createNode({ type: "note", text: "   ", x: 0, y: 0 });
    expect(describeNodeForSideTask(note).prompt).toBe("Follow up on this work-canvas sticky note.");
  });
});

describe("workCanvas persistence", () => {
  it("round-trips boards through localStorage", () => {
    const board = boardWithNodes();
    saveCanvasState([board], board.id);
    const loaded = loadCanvasState();
    expect(loaded.activeBoardId).toBe(board.id);
    expect(loaded.boards).toHaveLength(1);
    expect(loaded.boards[0]).toEqual(board);
  });

  it("returns empty state when nothing has been saved", () => {
    expect(loadCanvasState()).toEqual({ boards: [], activeBoardId: null });
  });

  it("ignores a corrupt blob instead of throwing", () => {
    localStorage.setItem("little-monkey-work-canvas-v1", "{not json");
    expect(loadCanvasState()).toEqual({ boards: [], activeBoardId: null });
  });

  it("drops an activeBoardId that no longer matches any saved board", () => {
    const board = boardWithNodes();
    localStorage.setItem(
      "little-monkey-work-canvas-v1",
      JSON.stringify({ version: 1, boards: [board], activeBoardId: "missing" }),
    );
    expect(loadCanvasState().activeBoardId).toBeNull();
  });

  it("filters out malformed boards while keeping valid ones", () => {
    const board = boardWithNodes();
    localStorage.setItem(
      "little-monkey-work-canvas-v1",
      JSON.stringify({ version: 1, boards: [board, { id: "bad" }], activeBoardId: board.id }),
    );
    const loaded = loadCanvasState();
    expect(loaded.boards).toHaveLength(1);
    expect(loaded.boards[0].id).toBe(board.id);
  });
});
