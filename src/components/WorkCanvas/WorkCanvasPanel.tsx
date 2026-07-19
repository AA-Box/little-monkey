import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import {
  FileText,
  Link2,
  ListTodo,
  MessageSquare,
  Plus,
  Rocket,
  StickyNote,
  Trash2,
  X,
  ZoomIn,
  ZoomOut,
} from "lucide-react";

import { useT } from "../../lib/i18n";
import {
  clampZoom,
  describeNodeForSideTask,
  DEFAULT_VIEWPORT,
  type WorkCanvasEdge,
  type WorkCanvasNode,
  type WorkCanvasViewport,
} from "../../lib/workCanvas";
import { selectActiveBoard, useWorkCanvasStore } from "../../store/workCanvasStore";
import { useSessionStore } from "../../store/sessionStore";
import { useRunStore } from "../../store/runStore";
import { useSideTaskStore } from "../../store/sideTaskStore";
import { IconButton, Button } from "../ui";

export interface WorkCanvasPanelProps {
  onClose: () => void;
  onOpenSession: (sessionId: string) => void;
  onOpenRun: (runId: string) => void;
  onOpenFile: (path: string) => void;
}

type AddMenuMode = "root" | "chat" | "file" | "run";

const ZOOM_STEP = 1.2;
/** Small deterministic jitter so several nodes added back-to-back (without
 * dragging in between) don't land in an identical spot. */
const PLACEMENT_JITTER_PX = 28;

function nodeIcon(type: WorkCanvasNode["type"]) {
  switch (type) {
    case "chat":
      return <MessageSquare size={14} className="shrink-0 text-accent" aria-hidden="true" />;
    case "file":
      return <FileText size={14} className="shrink-0 text-accent" aria-hidden="true" />;
    case "run":
      return <ListTodo size={14} className="shrink-0 text-accent" aria-hidden="true" />;
    case "note":
      return <StickyNote size={14} className="shrink-0 text-warning" aria-hidden="true" />;
  }
}

function nodeCardLabel(node: WorkCanvasNode, t: (key: string, vars?: Record<string, string | number>) => string): string {
  switch (node.type) {
    case "chat":
      return node.title || t("WorkCanvasPanel.node.chatLabel");
    case "file":
      return node.path;
    case "run":
      return node.title || t("WorkCanvasPanel.node.runLabel");
    case "note":
      return t("WorkCanvasPanel.node.noteLabel");
  }
}

interface DragState {
  pointerId: number;
  startClientX: number;
  startClientY: number;
  startX: number;
  startY: number;
}

function NodeCard({
  node,
  viewport,
  isSelected,
  isConnectSource,
  onSelect,
  onCommitMove,
  onRemove,
  onOpen,
  onSpawnTask,
  onNoteChange,
  onStartConnect,
  t,
}: {
  node: WorkCanvasNode;
  viewport: WorkCanvasViewport;
  isSelected: boolean;
  isConnectSource: boolean;
  onSelect: (id: string) => void;
  onCommitMove: (id: string, x: number, y: number) => void;
  onRemove: (id: string) => void;
  onOpen: (node: WorkCanvasNode) => void;
  onSpawnTask: (node: WorkCanvasNode) => void;
  onNoteChange: (id: string, text: string) => void;
  onStartConnect: (id: string) => void;
  t: (key: string, vars?: Record<string, string | number>) => string;
}) {
  const dragRef = useRef<DragState | null>(null);
  const [livePos, setLivePos] = useState<{ x: number; y: number } | null>(null);
  const pos = livePos ?? { x: node.x, y: node.y };

  const handlePointerDown = useCallback(
    (event: React.PointerEvent<HTMLDivElement>) => {
      if ((event.target as HTMLElement).closest("button, textarea, input, a")) return;
      event.stopPropagation();
      onSelect(node.id);
      dragRef.current = {
        pointerId: event.pointerId,
        startClientX: event.clientX,
        startClientY: event.clientY,
        startX: node.x,
        startY: node.y,
      };
      event.currentTarget.setPointerCapture(event.pointerId);
    },
    [node.id, node.x, node.y, onSelect],
  );

  const handlePointerMove = useCallback(
    (event: React.PointerEvent<HTMLDivElement>) => {
      const drag = dragRef.current;
      if (!drag || drag.pointerId !== event.pointerId) return;
      const dx = (event.clientX - drag.startClientX) / viewport.zoom;
      const dy = (event.clientY - drag.startClientY) / viewport.zoom;
      setLivePos({ x: drag.startX + dx, y: drag.startY + dy });
    },
    [viewport.zoom],
  );

  const endDrag = useCallback(
    (event: React.PointerEvent<HTMLDivElement>) => {
      const drag = dragRef.current;
      if (!drag || drag.pointerId !== event.pointerId) return;
      dragRef.current = null;
      try {
        event.currentTarget.releasePointerCapture(event.pointerId);
      } catch {
        // Already released — nothing to clean up.
      }
      if (livePos) onCommitMove(node.id, livePos.x, livePos.y);
      setLivePos(null);
    },
    [livePos, node.id, onCommitMove],
  );

  return (
    <div
      data-canvas-node-id={node.id}
      onPointerDown={handlePointerDown}
      onPointerMove={handlePointerMove}
      onPointerUp={endDrag}
      onPointerCancel={endDrag}
      style={{ left: pos.x, top: pos.y, width: node.width, minHeight: node.height, position: "absolute" }}
      className={`flex cursor-grab flex-col gap-1.5 rounded-lg border bg-background p-2.5 shadow-sm select-none active:cursor-grabbing ${
        isSelected ? "border-accent ring-2 ring-accent/40" : isConnectSource ? "border-accent" : "border-border"
      }`}
    >
      <div className="flex items-center gap-1.5">
        {nodeIcon(node.type)}
        <span className="min-w-0 flex-1 truncate text-xs font-medium text-foreground" title={nodeCardLabel(node, t)}>
          {nodeCardLabel(node, t)}
        </span>
        <IconButton
          size="sm"
          className="h-6 w-6 shrink-0"
          aria-label={t("WorkCanvasPanel.node.remove")}
          onClick={() => onRemove(node.id)}
        >
          <Trash2 size={12} />
        </IconButton>
      </div>

      {node.type === "note" ? (
        <textarea
          value={node.text}
          onChange={(event) => onNoteChange(node.id, event.target.value)}
          placeholder={t("WorkCanvasPanel.node.notePlaceholder")}
          className="min-h-0 flex-1 resize-none rounded border border-border bg-surface p-1.5 text-xs text-foreground outline-none focus:border-accent"
        />
      ) : (
        <div className="flex-1" />
      )}

      <div className="flex items-center justify-between gap-1">
        <div className="flex items-center gap-1">
          {node.type !== "note" && (
            <button
              type="button"
              onClick={() => onOpen(node)}
              className="cursor-pointer rounded px-1.5 py-0.5 text-[11px] font-medium text-accent hover:bg-surface-2"
            >
              {t("WorkCanvasPanel.node.open")}
            </button>
          )}
          <button
            type="button"
            onClick={() => onSpawnTask(node)}
            className="flex cursor-pointer items-center gap-1 rounded px-1.5 py-0.5 text-[11px] font-medium text-muted hover:bg-surface-2 hover:text-foreground"
          >
            <Rocket size={11} />
            {t("WorkCanvasPanel.node.spawnTask")}
          </button>
        </div>
        <button
          type="button"
          title={t("WorkCanvasPanel.node.connect")}
          aria-label={t("WorkCanvasPanel.node.connect")}
          onPointerDown={(event) => {
            event.stopPropagation();
            onStartConnect(node.id);
          }}
          className="flex h-5 w-5 shrink-0 cursor-crosshair items-center justify-center rounded-full border border-border bg-surface-2 text-faint hover:border-accent hover:text-accent"
        >
          <Link2 size={11} />
        </button>
      </div>
    </div>
  );
}

/**
 * Infinite Work Canvas (ROADMAP.md Phase 7): a hand-rolled pannable/zoomable
 * spatial board — plain React + CSS transforms + pointer events, no
 * diagramming library. Nodes reference chat sessions, files, runs/tasks, or
 * hold a freeform sticky note; edges are drawn by dragging from a node's
 * connect handle onto another node. "Open" jumps back to the referenced live
 * app state (chat/file/run); "Spawn task" seeds a new Side Task from the
 * node's context via `sideTaskStore.openComposer` (the user still reviews
 * and presses start there — this never launches a run silently).
 */
export function WorkCanvasPanel({ onClose, onOpenSession, onOpenRun, onOpenFile }: WorkCanvasPanelProps) {
  const { t } = useT();
  const boards = useWorkCanvasStore((state) => state.boards);
  const activeBoardId = useWorkCanvasStore((state) => state.activeBoardId);
  const board = useWorkCanvasStore(selectActiveBoard);
  const selectedNodeId = useWorkCanvasStore((state) => state.selectedNodeId);
  const connectingFromNodeId = useWorkCanvasStore((state) => state.connectingFromNodeId);
  const createBoard = useWorkCanvasStore((state) => state.createBoard);
  const deleteBoard = useWorkCanvasStore((state) => state.deleteBoard);
  const renameBoard = useWorkCanvasStore((state) => state.renameBoard);
  const selectBoard = useWorkCanvasStore((state) => state.selectBoard);
  const selectNode = useWorkCanvasStore((state) => state.selectNode);
  const addNode = useWorkCanvasStore((state) => state.addNode);
  const moveNode = useWorkCanvasStore((state) => state.moveNode);
  const removeNode = useWorkCanvasStore((state) => state.removeNode);
  const updateNoteText = useWorkCanvasStore((state) => state.updateNoteText);
  const setViewport = useWorkCanvasStore((state) => state.setViewport);
  const startConnecting = useWorkCanvasStore((state) => state.startConnecting);
  const cancelConnecting = useWorkCanvasStore((state) => state.cancelConnecting);
  const completeConnecting = useWorkCanvasStore((state) => state.completeConnecting);
  const removeEdge = useWorkCanvasStore((state) => state.removeEdge);

  const sessions = useSessionStore((state) => state.sessions);
  const activeSessionId = useSessionStore((state) => state.activeSessionId);
  const runs = useRunStore((state) => state.runs);
  const refreshRuns = useRunStore((state) => state.refresh);
  const openSideTaskComposer = useSideTaskStore((state) => state.openComposer);

  const containerRef = useRef<HTMLDivElement>(null);
  const [newBoardName, setNewBoardName] = useState("");
  const [boardSwitcherOpen, setBoardSwitcherOpen] = useState(false);
  const [pendingDeleteBoardId, setPendingDeleteBoardId] = useState<string | null>(null);
  const [renamingBoardId, setRenamingBoardId] = useState<string | null>(null);
  const [renameDraft, setRenameDraft] = useState("");
  const [addMenuOpen, setAddMenuOpen] = useState(false);
  const [addMenuMode, setAddMenuMode] = useState<AddMenuMode>("root");
  const [filePathDraft, setFilePathDraft] = useState("");
  const [connectPoint, setConnectPoint] = useState<{ x: number; y: number } | null>(null);
  const placementCounter = useRef(0);

  useEffect(() => {
    void refreshRuns();
  }, [refreshRuns]);

  const viewport = board?.viewport ?? DEFAULT_VIEWPORT;
  const panRef = useRef<{ pointerId: number; startClientX: number; startClientY: number; startX: number; startY: number } | null>(null);
  const [livePan, setLivePan] = useState<{ x: number; y: number } | null>(null);
  const effectiveViewport: WorkCanvasViewport = livePan ? { ...viewport, x: livePan.x, y: livePan.y } : viewport;

  const applyZoom = useCallback(
    (factor: number, anchorClientX?: number, anchorClientY?: number) => {
      if (!board) return;
      const rect = containerRef.current?.getBoundingClientRect();
      const anchorX = (anchorClientX ?? (rect ? rect.left + rect.width / 2 : 0)) - (rect?.left ?? 0);
      const anchorY = (anchorClientY ?? (rect ? rect.top + rect.height / 2 : 0)) - (rect?.top ?? 0);
      const nextZoom = clampZoom(viewport.zoom * factor);
      const worldX = (anchorX - viewport.x) / viewport.zoom;
      const worldY = (anchorY - viewport.y) / viewport.zoom;
      setViewport({ x: anchorX - worldX * nextZoom, y: anchorY - worldY * nextZoom, zoom: nextZoom });
    },
    [board, setViewport, viewport],
  );

  const handleWheel = useCallback(
    (event: React.WheelEvent<HTMLDivElement>) => {
      if (!board) return;
      event.preventDefault();
      const rect = containerRef.current?.getBoundingClientRect();
      const factor = Math.exp(-event.deltaY * 0.0015);
      const anchorX = event.clientX - (rect?.left ?? 0);
      const anchorY = event.clientY - (rect?.top ?? 0);
      const nextZoom = clampZoom(viewport.zoom * factor);
      const worldX = (anchorX - viewport.x) / viewport.zoom;
      const worldY = (anchorY - viewport.y) / viewport.zoom;
      setViewport({ x: anchorX - worldX * nextZoom, y: anchorY - worldY * nextZoom, zoom: nextZoom });
    },
    [board, setViewport, viewport],
  );

  const handleBackgroundPointerDown = useCallback(
    (event: React.PointerEvent<HTMLDivElement>) => {
      if (event.target !== event.currentTarget || !board) return;
      selectNode(null);
      panRef.current = {
        pointerId: event.pointerId,
        startClientX: event.clientX,
        startClientY: event.clientY,
        startX: viewport.x,
        startY: viewport.y,
      };
      event.currentTarget.setPointerCapture(event.pointerId);
    },
    [board, selectNode, viewport.x, viewport.y],
  );

  const handleBackgroundPointerMove = useCallback(
    (event: React.PointerEvent<HTMLDivElement>) => {
      const pan = panRef.current;
      if (pan && pan.pointerId === event.pointerId) {
        setLivePan({ x: pan.startX + (event.clientX - pan.startClientX), y: pan.startY + (event.clientY - pan.startClientY) });
      }
      if (connectingFromNodeId) {
        const rect = containerRef.current?.getBoundingClientRect();
        setConnectPoint({ x: event.clientX - (rect?.left ?? 0), y: event.clientY - (rect?.top ?? 0) });
      }
    },
    [connectingFromNodeId],
  );

  const endBackgroundGesture = useCallback(
    (event: React.PointerEvent<HTMLDivElement>) => {
      const pan = panRef.current;
      if (pan && pan.pointerId === event.pointerId) {
        panRef.current = null;
        try {
          event.currentTarget.releasePointerCapture(event.pointerId);
        } catch {
          // Already released.
        }
        if (livePan) setViewport({ ...viewport, x: livePan.x, y: livePan.y });
        setLivePan(null);
      }
      if (connectingFromNodeId) {
        const target = document.elementFromPoint(event.clientX, event.clientY)?.closest("[data-canvas-node-id]") as HTMLElement | null;
        const targetId = target?.dataset.canvasNodeId;
        if (targetId && targetId !== connectingFromNodeId) completeConnecting(targetId);
        else cancelConnecting();
        setConnectPoint(null);
      }
    },
    [cancelConnecting, completeConnecting, connectingFromNodeId, livePan, setViewport, viewport],
  );

  function computeDropPosition(): { x: number; y: number } {
    const rect = containerRef.current?.getBoundingClientRect();
    const centerScreenX = rect ? rect.width / 2 : 0;
    const centerScreenY = rect ? rect.height / 2 : 0;
    placementCounter.current += 1;
    const jitter = (placementCounter.current % 6) * PLACEMENT_JITTER_PX;
    return {
      x: (centerScreenX - viewport.x) / viewport.zoom + jitter,
      y: (centerScreenY - viewport.y) / viewport.zoom + jitter,
    };
  }

  function closeAddMenu() {
    setAddMenuOpen(false);
    setAddMenuMode("root");
    setFilePathDraft("");
  }

  function handleAddNote() {
    const pos = computeDropPosition();
    addNode({ type: "note", text: "", x: pos.x, y: pos.y });
    closeAddMenu();
  }

  function handleAddChat(sessionId: string, title: string) {
    const pos = computeDropPosition();
    addNode({ type: "chat", sessionId, title, x: pos.x, y: pos.y });
    closeAddMenu();
  }

  function handleAddRun(runId: string, title: string) {
    const pos = computeDropPosition();
    addNode({ type: "run", runId, title, x: pos.x, y: pos.y });
    closeAddMenu();
  }

  function handleAddFile() {
    const path = filePathDraft.trim();
    if (!path) return;
    const pos = computeDropPosition();
    addNode({ type: "file", path, x: pos.x, y: pos.y });
    closeAddMenu();
  }

  function handleOpenNode(node: WorkCanvasNode) {
    if (node.type === "chat") onOpenSession(node.sessionId);
    else if (node.type === "file") onOpenFile(node.path);
    else if (node.type === "run") onOpenRun(node.runId);
  }

  function handleSpawnTask(node: WorkCanvasNode) {
    const { title, prompt } = describeNodeForSideTask(node);
    openSideTaskComposer({
      title,
      prompt,
      profile: "explore",
      source: { kind: "manual", label: t("WorkCanvasPanel.node.spawnTaskSourceLabel"), excerpt: prompt.slice(0, 240) },
      sessionId: activeSessionId,
    });
  }

  const eligibleSessions = useMemo(
    () => sessions.filter((session) => session.messages.length > 0 && !session.archived).sort((a, b) => b.updatedAt - a.updatedAt),
    [sessions],
  );
  const eligibleRuns = useMemo(() => runs.filter((run) => run.archivedAtMs === null).sort((a, b) => b.updatedAtMs - a.updatedAtMs), [runs]);

  function nodeCenter(node: WorkCanvasNode) {
    return { x: node.x + node.width / 2, y: node.y + node.height / 2 };
  }

  function edgeLine(edge: WorkCanvasEdge, nodes: WorkCanvasNode[]) {
    const from = nodes.find((n) => n.id === edge.fromNodeId);
    const to = nodes.find((n) => n.id === edge.toNodeId);
    if (!from || !to) return null;
    const a = nodeCenter(from);
    const b = nodeCenter(to);
    return { a, b };
  }

  return (
    <section className="flex min-h-0 flex-1 flex-col" aria-labelledby="work-canvas-title">
      <header className="flex shrink-0 items-center justify-between gap-3 border-b border-border px-4 py-3">
        <div className="min-w-0">
          <h1 id="work-canvas-title" className="text-base font-semibold text-foreground">
            {t("WorkCanvasPanel.title")}
          </h1>
          <p className="truncate text-xs text-muted">{t("WorkCanvasPanel.subtitle")}</p>
        </div>
        <div className="flex shrink-0 items-center gap-1.5">
          {board && (
            <div className="relative">
              <Button size="sm" variant="secondary" onClick={() => setBoardSwitcherOpen((open) => !open)}>
                {board.name}
              </Button>
              {boardSwitcherOpen && (
                <div className="absolute right-0 top-full z-30 mt-1 w-64 rounded-lg border border-border bg-background py-1 shadow-lg">
                  <div className="max-h-56 overflow-y-auto">
                    {boards.map((entry) => (
                      <div key={entry.id} className="flex items-center gap-1 px-2 py-1 hover:bg-surface-2">
                        {renamingBoardId === entry.id ? (
                          <input
                            autoFocus
                            value={renameDraft}
                            onChange={(event) => setRenameDraft(event.target.value)}
                            onKeyDown={(event) => {
                              if (event.key === "Enter") {
                                renameBoard(entry.id, renameDraft);
                                setRenamingBoardId(null);
                              } else if (event.key === "Escape") {
                                setRenamingBoardId(null);
                              }
                            }}
                            onBlur={() => {
                              renameBoard(entry.id, renameDraft);
                              setRenamingBoardId(null);
                            }}
                            className="min-w-0 flex-1 rounded border border-border bg-surface px-1.5 py-0.5 text-xs text-foreground outline-none focus:border-accent"
                          />
                        ) : (
                          <button
                            type="button"
                            onClick={() => {
                              selectBoard(entry.id);
                              setBoardSwitcherOpen(false);
                            }}
                            onDoubleClick={() => {
                              setRenamingBoardId(entry.id);
                              setRenameDraft(entry.name);
                            }}
                            className={`min-w-0 flex-1 truncate rounded px-1 py-0.5 text-left text-xs ${
                              entry.id === activeBoardId ? "font-semibold text-accent" : "text-foreground"
                            }`}
                          >
                            {entry.name}
                          </button>
                        )}
                        {pendingDeleteBoardId === entry.id ? (
                          <div className="flex shrink-0 items-center gap-1">
                            <button
                              type="button"
                              onClick={() => {
                                deleteBoard(entry.id);
                                setPendingDeleteBoardId(null);
                              }}
                              className="cursor-pointer rounded px-1.5 py-0.5 text-[11px] font-medium text-danger hover:bg-danger-soft"
                            >
                              {t("WorkCanvasPanel.boardSwitcher.deleteConfirmYes")}
                            </button>
                            <button
                              type="button"
                              onClick={() => setPendingDeleteBoardId(null)}
                              className="cursor-pointer rounded px-1.5 py-0.5 text-[11px] text-muted hover:bg-surface"
                            >
                              {t("WorkCanvasPanel.boardSwitcher.deleteConfirmCancel")}
                            </button>
                          </div>
                        ) : (
                          <IconButton
                            size="sm"
                            className="h-6 w-6 shrink-0"
                            aria-label={t("WorkCanvasPanel.boardSwitcher.delete")}
                            onClick={() => setPendingDeleteBoardId(entry.id)}
                          >
                            <Trash2 size={12} />
                          </IconButton>
                        )}
                      </div>
                    ))}
                  </div>
                  <div className="mt-1 flex items-center gap-1 border-t border-border px-2 pt-1.5">
                    <input
                      value={newBoardName}
                      onChange={(event) => setNewBoardName(event.target.value)}
                      onKeyDown={(event) => {
                        if (event.key === "Enter" && newBoardName.trim()) {
                          createBoard(newBoardName);
                          setNewBoardName("");
                        }
                      }}
                      placeholder={t("WorkCanvasPanel.newBoardPlaceholder")}
                      className="min-w-0 flex-1 rounded border border-border bg-surface px-1.5 py-0.5 text-xs text-foreground outline-none focus:border-accent"
                    />
                    <IconButton
                      size="sm"
                      className="h-6 w-6 shrink-0"
                      aria-label={t("WorkCanvasPanel.boardSwitcher.newBoard")}
                      disabled={!newBoardName.trim()}
                      onClick={() => {
                        createBoard(newBoardName);
                        setNewBoardName("");
                      }}
                    >
                      <Plus size={12} />
                    </IconButton>
                  </div>
                </div>
              )}
            </div>
          )}

          {board && (
            <>
              <IconButton size="sm" aria-label={t("WorkCanvasPanel.zoomOut")} onClick={() => applyZoom(1 / ZOOM_STEP)}>
                <ZoomOut size={15} />
              </IconButton>
              <IconButton size="sm" aria-label={t("WorkCanvasPanel.zoomIn")} onClick={() => applyZoom(ZOOM_STEP)}>
                <ZoomIn size={15} />
              </IconButton>

              <div className="relative">
                <Button size="sm" variant="primary" onClick={() => setAddMenuOpen((open) => !open)}>
                  <Plus size={14} />
                  {t("WorkCanvasPanel.addNode")}
                </Button>
                {addMenuOpen && (
                  <div className="absolute right-0 top-full z-30 mt-1 w-64 rounded-lg border border-border bg-background py-1 shadow-lg">
                    {addMenuMode === "root" && (
                      <>
                        <button
                          type="button"
                          onClick={() => setAddMenuMode("chat")}
                          className="flex w-full cursor-pointer items-center gap-2 px-3 py-1.5 text-left text-sm text-foreground hover:bg-surface-2"
                        >
                          <MessageSquare size={14} className="text-faint" />
                          {t("WorkCanvasPanel.addNode.chat")}
                        </button>
                        <button
                          type="button"
                          onClick={() => setAddMenuMode("file")}
                          className="flex w-full cursor-pointer items-center gap-2 px-3 py-1.5 text-left text-sm text-foreground hover:bg-surface-2"
                        >
                          <FileText size={14} className="text-faint" />
                          {t("WorkCanvasPanel.addNode.file")}
                        </button>
                        <button
                          type="button"
                          onClick={() => setAddMenuMode("run")}
                          className="flex w-full cursor-pointer items-center gap-2 px-3 py-1.5 text-left text-sm text-foreground hover:bg-surface-2"
                        >
                          <ListTodo size={14} className="text-faint" />
                          {t("WorkCanvasPanel.addNode.run")}
                        </button>
                        <button
                          type="button"
                          onClick={handleAddNote}
                          className="flex w-full cursor-pointer items-center gap-2 px-3 py-1.5 text-left text-sm text-foreground hover:bg-surface-2"
                        >
                          <StickyNote size={14} className="text-faint" />
                          {t("WorkCanvasPanel.addNode.note")}
                        </button>
                      </>
                    )}

                    {addMenuMode === "chat" && (
                      <div className="max-h-64 overflow-y-auto">
                        {eligibleSessions.length === 0 ? (
                          <p className="px-3 py-2 text-xs text-faint">{t("WorkCanvasPanel.addNode.noSessions")}</p>
                        ) : (
                          eligibleSessions.map((session) => (
                            <button
                              key={session.id}
                              type="button"
                              onClick={() => handleAddChat(session.id, session.title)}
                              className="flex w-full cursor-pointer items-center px-3 py-1.5 text-left text-sm text-foreground hover:bg-surface-2"
                            >
                              <span className="truncate">{session.title}</span>
                            </button>
                          ))
                        )}
                      </div>
                    )}

                    {addMenuMode === "run" && (
                      <div className="max-h-64 overflow-y-auto">
                        {eligibleRuns.length === 0 ? (
                          <p className="px-3 py-2 text-xs text-faint">{t("WorkCanvasPanel.addNode.noRuns")}</p>
                        ) : (
                          eligibleRuns.map((run) => (
                            <button
                              key={run.spec.run_id}
                              type="button"
                              onClick={() => handleAddRun(run.spec.run_id, run.spec.task)}
                              className="flex w-full cursor-pointer flex-col items-start px-3 py-1.5 text-left text-sm text-foreground hover:bg-surface-2"
                            >
                              <span className="w-full truncate">{run.spec.task}</span>
                              <span className="text-[11px] text-faint">{t(`DailyBriefPanel.runStatus.${run.status}`)}</span>
                            </button>
                          ))
                        )}
                      </div>
                    )}

                    {addMenuMode === "file" && (
                      <div className="flex flex-col gap-1.5 px-3 py-2">
                        <label className="text-xs font-medium text-muted" htmlFor="work-canvas-file-path">
                          {t("WorkCanvasPanel.addNode.filePathLabel")}
                        </label>
                        <input
                          id="work-canvas-file-path"
                          autoFocus
                          value={filePathDraft}
                          onChange={(event) => setFilePathDraft(event.target.value)}
                          onKeyDown={(event) => {
                            if (event.key === "Enter") handleAddFile();
                          }}
                          placeholder={t("WorkCanvasPanel.addNode.filePathPlaceholder")}
                          className="rounded border border-border bg-surface px-2 py-1 text-xs text-foreground outline-none focus:border-accent"
                        />
                        <Button size="sm" variant="primary" disabled={!filePathDraft.trim()} onClick={handleAddFile}>
                          {t("WorkCanvasPanel.addNode.filePathAdd")}
                        </Button>
                      </div>
                    )}

                    {addMenuMode !== "root" && (
                      <button
                        type="button"
                        onClick={() => setAddMenuMode("root")}
                        className="mt-1 w-full cursor-pointer border-t border-border px-3 py-1.5 text-left text-xs text-muted hover:bg-surface-2"
                      >
                        {t("WorkCanvasPanel.addNode.back")}
                      </button>
                    )}
                  </div>
                )}
              </div>
            </>
          )}

          <IconButton size="sm" onClick={onClose} aria-label={t("WorkCanvasPanel.close")}>
            <X size={16} />
          </IconButton>
        </div>
      </header>

      {!board ? (
        <div className="flex min-h-0 flex-1 flex-col items-center justify-center gap-3 px-6 text-center">
          <StickyNote size={28} className="text-faint" aria-hidden="true" />
          <h2 className="text-sm font-semibold text-foreground">{t("WorkCanvasPanel.emptyTitle")}</h2>
          <p className="max-w-sm text-xs text-muted">{t("WorkCanvasPanel.emptyDescription")}</p>
          <div className="flex items-center gap-1.5">
            <input
              value={newBoardName}
              onChange={(event) => setNewBoardName(event.target.value)}
              onKeyDown={(event) => {
                if (event.key === "Enter" && newBoardName.trim()) {
                  createBoard(newBoardName);
                  setNewBoardName("");
                }
              }}
              placeholder={t("WorkCanvasPanel.newBoardPlaceholder")}
              className="rounded-md border border-border bg-surface px-2.5 py-1.5 text-sm text-foreground outline-none focus:border-accent"
            />
            <Button
              variant="primary"
              disabled={!newBoardName.trim()}
              onClick={() => {
                createBoard(newBoardName);
                setNewBoardName("");
              }}
            >
              {t("WorkCanvasPanel.createBoard")}
            </Button>
          </div>
        </div>
      ) : (
        <div
          ref={containerRef}
          onWheel={handleWheel}
          className="relative min-h-0 flex-1 overflow-hidden bg-surface"
          style={{
            backgroundImage: "radial-gradient(var(--color-border) 1px, transparent 1px)",
            backgroundSize: `${24 * effectiveViewport.zoom}px ${24 * effectiveViewport.zoom}px`,
            backgroundPosition: `${effectiveViewport.x}px ${effectiveViewport.y}px`,
          }}
        >
          <div
            onPointerDown={handleBackgroundPointerDown}
            onPointerMove={handleBackgroundPointerMove}
            onPointerUp={endBackgroundGesture}
            onPointerCancel={endBackgroundGesture}
            className="absolute inset-0 cursor-grab active:cursor-grabbing"
            style={{
              transform: `translate(${effectiveViewport.x}px, ${effectiveViewport.y}px) scale(${effectiveViewport.zoom})`,
              transformOrigin: "0 0",
            }}
          >
            <svg className="pointer-events-none absolute left-0 top-0 overflow-visible" width={1} height={1}>
              {board.edges.map((edge) => {
                const line = edgeLine(edge, board.nodes);
                if (!line) return null;
                return (
                  <line
                    key={edge.id}
                    x1={line.a.x}
                    y1={line.a.y}
                    x2={line.b.x}
                    y2={line.b.y}
                    stroke="var(--color-border-strong, var(--color-border))"
                    strokeWidth={2}
                    className="pointer-events-auto cursor-pointer"
                    onClick={() => removeEdge(edge.id)}
                  >
                    <title>{t("WorkCanvasPanel.edge.remove")}</title>
                  </line>
                );
              })}
            </svg>

            {board.nodes.map((node) => (
              <NodeCard
                key={node.id}
                node={node}
                viewport={effectiveViewport}
                isSelected={selectedNodeId === node.id}
                isConnectSource={connectingFromNodeId === node.id}
                onSelect={selectNode}
                onCommitMove={moveNode}
                onRemove={removeNode}
                onOpen={handleOpenNode}
                onSpawnTask={handleSpawnTask}
                onNoteChange={updateNoteText}
                onStartConnect={startConnecting}
                t={t}
              />
            ))}
          </div>

          {connectingFromNodeId && connectPoint && (
            <svg className="pointer-events-none absolute inset-0 h-full w-full overflow-visible">
              {(() => {
                const source = board.nodes.find((n) => n.id === connectingFromNodeId);
                if (!source) return null;
                const center = nodeCenter(source);
                const screenX = center.x * effectiveViewport.zoom + effectiveViewport.x;
                const screenY = center.y * effectiveViewport.zoom + effectiveViewport.y;
                return (
                  <line
                    x1={screenX}
                    y1={screenY}
                    x2={connectPoint.x}
                    y2={connectPoint.y}
                    stroke="var(--color-accent)"
                    strokeWidth={2}
                    strokeDasharray="4 4"
                  />
                );
              })()}
            </svg>
          )}

          {board.nodes.length === 0 && (
            <div className="pointer-events-none absolute inset-0 flex flex-col items-center justify-center gap-1 text-center">
              <p className="text-sm font-medium text-muted">{t("WorkCanvasPanel.canvasEmptyTitle")}</p>
              <p className="max-w-xs text-xs text-faint">{t("WorkCanvasPanel.canvasEmptyDescription")}</p>
            </div>
          )}

          <p className="pointer-events-none absolute bottom-2 left-2 max-w-md rounded bg-background/80 px-2 py-1 text-[11px] text-faint">
            {t("WorkCanvasPanel.helpHint")}
          </p>
        </div>
      )}
    </section>
  );
}

export default WorkCanvasPanel;
