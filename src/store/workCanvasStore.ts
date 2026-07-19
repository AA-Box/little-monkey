import { create } from "zustand";

import {
  addEdge as addEdgeToBoard,
  addNode as addNodeToBoard,
  createNode,
  loadCanvasState,
  moveNode as moveNodeInBoard,
  newBoard,
  removeEdge as removeEdgeFromBoard,
  removeNode as removeNodeFromBoard,
  renameBoard as renameBoardWith,
  saveCanvasState,
  setViewport as setViewportOnBoard,
  updateNoteText as updateNoteTextOnBoard,
  type NewNodeInput,
  type WorkCanvasBoard,
  type WorkCanvasViewport,
} from "../lib/workCanvas";

/**
 * Infinite Work Canvas store (ROADMAP.md Phase 7): zustand wiring around
 * `workCanvas.ts`'s pure board transforms, persisted to localStorage on
 * every mutation — same "no debounce, just persist synchronously since a
 * board write is cheap and infrequent (user-driven drags/clicks, not a
 * per-keystroke stream)" stance as `workflowDraftStore.ts`. Every board a
 * user has created lives here; `activeBoardId` is the one currently shown by
 * `WorkCanvasPanel.tsx`. `selectedNodeId`/`connectingFromNodeId` are
 * transient UI-only state (never persisted) for the currently-selected card
 * and an in-progress drag-to-connect gesture.
 */
interface WorkCanvasStoreState {
  boards: WorkCanvasBoard[];
  activeBoardId: string | null;
  selectedNodeId: string | null;
  connectingFromNodeId: string | null;

  createBoard: (name: string) => string;
  deleteBoard: (id: string) => void;
  renameBoard: (id: string, name: string) => void;
  selectBoard: (id: string) => void;
  selectNode: (id: string | null) => void;

  addNode: (input: NewNodeInput) => string | null;
  moveNode: (nodeId: string, x: number, y: number) => void;
  removeNode: (nodeId: string) => void;
  updateNoteText: (nodeId: string, text: string) => void;
  setViewport: (viewport: WorkCanvasViewport) => void;

  startConnecting: (nodeId: string) => void;
  cancelConnecting: () => void;
  completeConnecting: (targetNodeId: string) => void;

  addEdge: (fromNodeId: string, toNodeId: string) => void;
  removeEdge: (edgeId: string) => void;
}

function persist(boards: WorkCanvasBoard[], activeBoardId: string | null): void {
  saveCanvasState(boards, activeBoardId);
}

function updateBoard(
  boards: WorkCanvasBoard[],
  id: string,
  update: (board: WorkCanvasBoard) => WorkCanvasBoard,
): WorkCanvasBoard[] {
  return boards.map((board) => (board.id === id ? update(board) : board));
}

const initial = (() => {
  try {
    return loadCanvasState();
  } catch {
    return { boards: [], activeBoardId: null };
  }
})();

export const useWorkCanvasStore = create<WorkCanvasStoreState>((set, get) => ({
  boards: initial.boards,
  activeBoardId: initial.activeBoardId,
  selectedNodeId: null,
  connectingFromNodeId: null,

  createBoard: (name) => {
    const board = newBoard(name);
    const boards = [board, ...get().boards];
    persist(boards, board.id);
    set({ boards, activeBoardId: board.id, selectedNodeId: null, connectingFromNodeId: null });
    return board.id;
  },

  deleteBoard: (id) => {
    const boards = get().boards.filter((board) => board.id !== id);
    const activeBoardId = get().activeBoardId === id ? boards[0]?.id ?? null : get().activeBoardId;
    persist(boards, activeBoardId);
    set({ boards, activeBoardId, selectedNodeId: null, connectingFromNodeId: null });
  },

  renameBoard: (id, name) => {
    const boards = updateBoard(get().boards, id, (board) => renameBoardWith(board, name));
    persist(boards, get().activeBoardId);
    set({ boards });
  },

  selectBoard: (id) => set({ activeBoardId: id, selectedNodeId: null, connectingFromNodeId: null }),
  selectNode: (id) => set({ selectedNodeId: id }),

  addNode: (input) => {
    const boardId = get().activeBoardId;
    if (!boardId) return null;
    const node = createNode(input);
    const boards = updateBoard(get().boards, boardId, (board) => addNodeToBoard(board, node));
    persist(boards, boardId);
    set({ boards, selectedNodeId: node.id });
    return node.id;
  },

  moveNode: (nodeId, x, y) => {
    const boardId = get().activeBoardId;
    if (!boardId) return;
    const boards = updateBoard(get().boards, boardId, (board) => moveNodeInBoard(board, nodeId, x, y));
    persist(boards, boardId);
    set({ boards });
  },

  removeNode: (nodeId) => {
    const boardId = get().activeBoardId;
    if (!boardId) return;
    const boards = updateBoard(get().boards, boardId, (board) => removeNodeFromBoard(board, nodeId));
    persist(boards, boardId);
    set({
      boards,
      selectedNodeId: get().selectedNodeId === nodeId ? null : get().selectedNodeId,
      connectingFromNodeId: get().connectingFromNodeId === nodeId ? null : get().connectingFromNodeId,
    });
  },

  updateNoteText: (nodeId, text) => {
    const boardId = get().activeBoardId;
    if (!boardId) return;
    const boards = updateBoard(get().boards, boardId, (board) => updateNoteTextOnBoard(board, nodeId, text));
    persist(boards, boardId);
    set({ boards });
  },

  setViewport: (viewport) => {
    const boardId = get().activeBoardId;
    if (!boardId) return;
    const boards = updateBoard(get().boards, boardId, (board) => setViewportOnBoard(board, viewport));
    persist(boards, boardId);
    set({ boards });
  },

  startConnecting: (nodeId) => set({ connectingFromNodeId: nodeId }),
  cancelConnecting: () => set({ connectingFromNodeId: null }),
  completeConnecting: (targetNodeId) => {
    const fromNodeId = get().connectingFromNodeId;
    if (!fromNodeId) return;
    get().addEdge(fromNodeId, targetNodeId);
    set({ connectingFromNodeId: null });
  },

  addEdge: (fromNodeId, toNodeId) => {
    const boardId = get().activeBoardId;
    if (!boardId) return;
    const boards = updateBoard(get().boards, boardId, (board) => addEdgeToBoard(board, fromNodeId, toNodeId));
    persist(boards, boardId);
    set({ boards });
  },

  removeEdge: (edgeId) => {
    const boardId = get().activeBoardId;
    if (!boardId) return;
    const boards = updateBoard(get().boards, boardId, (board) => removeEdgeFromBoard(board, edgeId));
    persist(boards, boardId);
    set({ boards });
  },
}));

/** The board currently shown by `WorkCanvasPanel.tsx`, or `null` before any
 * board has been created / after the last one was deleted. */
export function selectActiveBoard(state: WorkCanvasStoreState): WorkCanvasBoard | null {
  return state.boards.find((board) => board.id === state.activeBoardId) ?? null;
}

export default useWorkCanvasStore;
