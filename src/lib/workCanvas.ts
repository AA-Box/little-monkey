/**
 * Infinite Work Canvas (ROADMAP.md "Phase 7: Market-Defining Differentiators"
 * -> "Infinite Work Canvas"): a spatial board of nodes — references to chat
 * sessions, files, runs/tasks, and freeform sticky notes — connected by
 * edges into plans, research boards, architecture maps, or task flows.
 *
 * Pure data model + localStorage persistence only, no React here (mirrors
 * `workflowRecorder.ts`/`workflowDraftStore.ts`'s split: this file owns the
 * shape and the small pure transforms, `workCanvasStore.ts` owns the zustand
 * wiring + persistence calls). A board is intentionally a plain reference
 * container — it never stores a copy of the chat transcript, file content,
 * or run detail it points to, only enough (a stable id, plus a short label
 * frozen at the moment the node was added) to jump back to the live source
 * of truth later via `WorkCanvasPanel.tsx`'s "Open" action. That is what
 * keeps a saved canvas "inspectable as project context" per the acceptance
 * criterion — it stays a map over real app state, not a stale snapshot of it.
 *
 * MVP scope note: node types are deliberately narrowed to the four the
 * ROADMAP implementation guidance calls out explicitly (chat session
 * reference, file reference, task/run reference, sticky note). Screenshots,
 * diagrams-as-first-class-nodes, model references, and connector references
 * are follow-ups — a sticky note already covers "jot down a diagram/screenshot
 * caption or a model/connector name" for this MVP.
 */

export type WorkCanvasNodeType = "chat" | "file" | "run" | "note";

interface WorkCanvasNodeBase {
  id: string;
  x: number;
  y: number;
  width: number;
  height: number;
  createdAt: number;
  updatedAt: number;
}

/** References a chat session by id — "Open" jumps back to that transcript. */
export interface ChatCanvasNode extends WorkCanvasNodeBase {
  type: "chat";
  sessionId: string;
  /** Session title frozen at the moment the node was added, so the card
   * still reads sensibly even if the session was later renamed or deleted —
   * "Open" always re-resolves the live session by id rather than trusting
   * this label. */
  title: string;
}

/** References a workspace-relative file path — "Open" re-reads the file
 * fresh via the same `tool_read_file` path `FileTree.tsx` already uses. */
export interface FileCanvasNode extends WorkCanvasNodeBase {
  type: "file";
  path: string;
}

/** References a durable run/task by id (see `runStore.ts`/`runProtocol.ts`)
 * — "Open" jumps to Run Center. */
export interface RunCanvasNode extends WorkCanvasNodeBase {
  type: "run";
  runId: string;
  /** Run title (`RunSpecWire.task`) frozen at add time, same rationale as
   * `ChatCanvasNode.title`. */
  title: string;
}

/** A freeform sticky note — no external reference, just inline text edited
 * directly on the card. */
export interface NoteCanvasNode extends WorkCanvasNodeBase {
  type: "note";
  text: string;
}

export type WorkCanvasNode = ChatCanvasNode | FileCanvasNode | RunCanvasNode | NoteCanvasNode;

/** An undirected connection between two nodes — a plan/board/map "arrow"
 * with no semantics of its own beyond "these two are related", matching the
 * acceptance criterion's "connect nodes into plans, research boards,
 * architecture maps, and task flows" (none of which need a directed or
 * typed edge to be useful at MVP scope). */
export interface WorkCanvasEdge {
  id: string;
  fromNodeId: string;
  toNodeId: string;
  createdAt: number;
}

export interface WorkCanvasViewport {
  x: number;
  y: number;
  zoom: number;
}

export interface WorkCanvasBoard {
  id: string;
  name: string;
  createdAt: number;
  updatedAt: number;
  nodes: WorkCanvasNode[];
  edges: WorkCanvasEdge[];
  viewport: WorkCanvasViewport;
}

export const DEFAULT_NODE_WIDTH = 220;
export const DEFAULT_NODE_HEIGHT = 112;
export const DEFAULT_NOTE_HEIGHT = 148;
export const MIN_ZOOM = 0.25;
export const MAX_ZOOM = 2.5;
export const DEFAULT_VIEWPORT: WorkCanvasViewport = { x: 0, y: 0, zoom: 1 };

function newId(): string {
  return crypto.randomUUID();
}

export function newBoard(name: string, now: number = Date.now()): WorkCanvasBoard {
  return {
    id: newId(),
    name: name.trim() || "Untitled board",
    createdAt: now,
    updatedAt: now,
    nodes: [],
    edges: [],
    viewport: { ...DEFAULT_VIEWPORT },
  };
}

export type NewNodeInput =
  | { type: "chat"; sessionId: string; title: string; x: number; y: number }
  | { type: "file"; path: string; x: number; y: number }
  | { type: "run"; runId: string; title: string; x: number; y: number }
  | { type: "note"; text: string; x: number; y: number };

export function createNode(input: NewNodeInput, now: number = Date.now()): WorkCanvasNode {
  const base = {
    id: newId(),
    x: input.x,
    y: input.y,
    width: DEFAULT_NODE_WIDTH,
    height: input.type === "note" ? DEFAULT_NOTE_HEIGHT : DEFAULT_NODE_HEIGHT,
    createdAt: now,
    updatedAt: now,
  };
  switch (input.type) {
    case "chat":
      return { ...base, type: "chat", sessionId: input.sessionId, title: input.title };
    case "file":
      return { ...base, type: "file", path: input.path };
    case "run":
      return { ...base, type: "run", runId: input.runId, title: input.title };
    case "note":
      return { ...base, type: "note", text: input.text };
  }
}

export function addNode(board: WorkCanvasBoard, node: WorkCanvasNode, now: number = Date.now()): WorkCanvasBoard {
  return { ...board, nodes: [...board.nodes, node], updatedAt: now };
}

export function removeNode(board: WorkCanvasBoard, nodeId: string, now: number = Date.now()): WorkCanvasBoard {
  return {
    ...board,
    nodes: board.nodes.filter((node) => node.id !== nodeId),
    edges: board.edges.filter((edge) => edge.fromNodeId !== nodeId && edge.toNodeId !== nodeId),
    updatedAt: now,
  };
}

export function moveNode(board: WorkCanvasBoard, nodeId: string, x: number, y: number, now: number = Date.now()): WorkCanvasBoard {
  return {
    ...board,
    nodes: board.nodes.map((node) => (node.id === nodeId ? { ...node, x, y, updatedAt: now } : node)),
    updatedAt: now,
  };
}

export function updateNoteText(board: WorkCanvasBoard, nodeId: string, text: string, now: number = Date.now()): WorkCanvasBoard {
  return {
    ...board,
    nodes: board.nodes.map((node) => (node.id === nodeId && node.type === "note" ? { ...node, text, updatedAt: now } : node)),
    updatedAt: now,
  };
}

/** Adds an undirected edge between two distinct, existing nodes. No-op
 * (returns `board` unchanged) for a self-loop, a reference to a node that
 * isn't on this board, or a duplicate of an edge (in either direction) that
 * already exists — silently, since this is always driven by a drag gesture
 * the user just performed, not a form with a validation message to show. */
export function addEdge(board: WorkCanvasBoard, fromNodeId: string, toNodeId: string, now: number = Date.now()): WorkCanvasBoard {
  if (fromNodeId === toNodeId) return board;
  const hasFrom = board.nodes.some((node) => node.id === fromNodeId);
  const hasTo = board.nodes.some((node) => node.id === toNodeId);
  if (!hasFrom || !hasTo) return board;
  const alreadyLinked = board.edges.some(
    (edge) =>
      (edge.fromNodeId === fromNodeId && edge.toNodeId === toNodeId) ||
      (edge.fromNodeId === toNodeId && edge.toNodeId === fromNodeId),
  );
  if (alreadyLinked) return board;
  const edge: WorkCanvasEdge = { id: newId(), fromNodeId, toNodeId, createdAt: now };
  return { ...board, edges: [...board.edges, edge], updatedAt: now };
}

export function removeEdge(board: WorkCanvasBoard, edgeId: string, now: number = Date.now()): WorkCanvasBoard {
  return { ...board, edges: board.edges.filter((edge) => edge.id !== edgeId), updatedAt: now };
}

export function renameBoard(board: WorkCanvasBoard, name: string, now: number = Date.now()): WorkCanvasBoard {
  const trimmed = name.trim();
  if (!trimmed) return board;
  return { ...board, name: trimmed, updatedAt: now };
}

export function setViewport(board: WorkCanvasBoard, viewport: WorkCanvasViewport, now: number = Date.now()): WorkCanvasBoard {
  return { ...board, viewport, updatedAt: now };
}

export function clampZoom(zoom: number): number {
  return Math.min(MAX_ZOOM, Math.max(MIN_ZOOM, zoom));
}

/** What "spawn task" should seed a side task with, built purely from a
 * node's own referenced content — kept here (not in the component) so the
 * exact prompt/title text is independently testable. The caller
 * (`WorkCanvasPanel.tsx`) supplies this to `sideTaskStore.openComposer`,
 * which still requires the user to review and press start — this function
 * only ever produces a *seed*, never runs anything itself. */
export function describeNodeForSideTask(node: WorkCanvasNode): { title: string; prompt: string } {
  switch (node.type) {
    case "chat":
      return {
        title: `From canvas: ${node.title}`,
        prompt: `Continue the work referenced by the "${node.title}" chat session linked on this work canvas (session id ${node.sessionId}). Review that context and proceed.`,
      };
    case "file":
      return {
        title: `From canvas: ${node.path}`,
        prompt: `Review the file referenced by this work-canvas node and continue the work implied by it: ${node.path}`,
      };
    case "run":
      return {
        title: `From canvas: ${node.title}`,
        prompt: `Follow up on the task/run referenced by this work-canvas node: "${node.title}" (run id ${node.runId}).`,
      };
    case "note":
      return {
        title: "From canvas note",
        prompt: node.text.trim() || "Follow up on this work-canvas sticky note.",
      };
  }
}

// --- Persistence -------------------------------------------------------

const STORAGE_KEY = "little-monkey-work-canvas-v1";

interface PersistedShape {
  version: 1;
  boards: WorkCanvasBoard[];
  activeBoardId: string | null;
}

export interface WorkCanvasPersistedState {
  boards: WorkCanvasBoard[];
  activeBoardId: string | null;
}

function isFiniteNumber(value: unknown): value is number {
  return typeof value === "number" && Number.isFinite(value);
}

function isNode(value: unknown): value is WorkCanvasNode {
  const node = value as Partial<WorkCanvasNode> | null;
  if (!node || typeof node.id !== "string") return false;
  if (!isFiniteNumber(node.x) || !isFiniteNumber(node.y)) return false;
  if (!isFiniteNumber(node.width) || !isFiniteNumber(node.height)) return false;
  switch (node.type) {
    case "chat":
      return typeof (node as ChatCanvasNode).sessionId === "string" && typeof (node as ChatCanvasNode).title === "string";
    case "file":
      return typeof (node as FileCanvasNode).path === "string";
    case "run":
      return typeof (node as RunCanvasNode).runId === "string" && typeof (node as RunCanvasNode).title === "string";
    case "note":
      return typeof (node as NoteCanvasNode).text === "string";
    default:
      return false;
  }
}

function isEdge(value: unknown): value is WorkCanvasEdge {
  const edge = value as Partial<WorkCanvasEdge> | null;
  return Boolean(edge && typeof edge.id === "string" && typeof edge.fromNodeId === "string" && typeof edge.toNodeId === "string");
}

function isViewport(value: unknown): value is WorkCanvasViewport {
  const viewport = value as Partial<WorkCanvasViewport> | null;
  return Boolean(viewport && isFiniteNumber(viewport.x) && isFiniteNumber(viewport.y) && isFiniteNumber(viewport.zoom));
}

function isBoard(value: unknown): value is WorkCanvasBoard {
  const board = value as Partial<WorkCanvasBoard> | null;
  if (!board || typeof board.id !== "string" || typeof board.name !== "string") return false;
  if (!Array.isArray(board.nodes) || !board.nodes.every(isNode)) return false;
  if (!Array.isArray(board.edges) || !board.edges.every(isEdge)) return false;
  if (!isViewport(board.viewport)) return false;
  return true;
}

/** Reads every saved board back from localStorage — best-effort, same
 * "corrupt/foreign data quietly becomes empty state" stance as
 * `workflowDraftStore.ts`'s `hydrate()`. */
export function loadCanvasState(): WorkCanvasPersistedState {
  try {
    const raw = localStorage.getItem(STORAGE_KEY);
    if (!raw) return { boards: [], activeBoardId: null };
    const parsed = JSON.parse(raw) as Partial<PersistedShape> | null;
    if (!parsed || parsed.version !== 1 || !Array.isArray(parsed.boards)) return { boards: [], activeBoardId: null };
    const boards = parsed.boards.filter(isBoard);
    const activeBoardId =
      typeof parsed.activeBoardId === "string" && boards.some((board) => board.id === parsed.activeBoardId)
        ? parsed.activeBoardId
        : null;
    return { boards, activeBoardId };
  } catch {
    return { boards: [], activeBoardId: null };
  }
}

export function saveCanvasState(boards: WorkCanvasBoard[], activeBoardId: string | null): void {
  try {
    const payload: PersistedShape = { version: 1, boards, activeBoardId };
    localStorage.setItem(STORAGE_KEY, JSON.stringify(payload));
  } catch {
    // Best-effort cache only — a board stays live in memory for this
    // session even if localStorage is unavailable or full.
  }
}
