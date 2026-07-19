/**
 * Infinite Work Canvas (ROADMAP.md Phase 7) — English source of truth for
 * the `WorkCanvasPanel.*` / `AppMenu.workCanvas` key namespace. Copied and
 * spread into every other locale file, then every key below is overridden
 * there with a real translation (see de.ts/fr.ts/etc.) — mirrors the
 * structure every other feature locale slice in this directory uses.
 */
export const workCanvasLocale: Record<string, string> = {
  "AppMenu.workCanvas": "Work Canvas",
  "WorkCanvasPanel.title": "Work Canvas",
  "WorkCanvasPanel.subtitle": "A spatial board for chats, files, tasks, and notes — connect them into plans, research boards, and architecture maps.",
  "WorkCanvasPanel.close": "Close Work Canvas",
  "WorkCanvasPanel.emptyTitle": "No boards yet",
  "WorkCanvasPanel.emptyDescription": "Create a board to start mapping chats, files, tasks, and notes in one place.",
  "WorkCanvasPanel.newBoardPlaceholder": "Board name",
  "WorkCanvasPanel.createBoard": "Create board",
  "WorkCanvasPanel.boardSwitcher.newBoard": "New board",
  "WorkCanvasPanel.boardSwitcher.delete": "Delete board",
  "WorkCanvasPanel.boardSwitcher.deleteConfirmYes": "Delete",
  "WorkCanvasPanel.boardSwitcher.deleteConfirmCancel": "Cancel",
  "WorkCanvasPanel.zoomIn": "Zoom in",
  "WorkCanvasPanel.zoomOut": "Zoom out",
  "WorkCanvasPanel.addNode": "Add to canvas",
  "WorkCanvasPanel.addNode.chat": "Chat session",
  "WorkCanvasPanel.addNode.file": "File",
  "WorkCanvasPanel.addNode.run": "Task / run",
  "WorkCanvasPanel.addNode.note": "Sticky note",
  "WorkCanvasPanel.addNode.back": "Back",
  "WorkCanvasPanel.addNode.noSessions": "No chat sessions yet.",
  "WorkCanvasPanel.addNode.noRuns": "No tasks/runs yet.",
  "WorkCanvasPanel.addNode.filePathLabel": "Workspace-relative path",
  "WorkCanvasPanel.addNode.filePathPlaceholder": "src/App.tsx",
  "WorkCanvasPanel.addNode.filePathAdd": "Add file",
  "WorkCanvasPanel.canvasEmptyTitle": "This board is empty",
  "WorkCanvasPanel.canvasEmptyDescription": "Use \"Add to canvas\" to place your first chat, file, task, or note.",
  "WorkCanvasPanel.node.open": "Open",
  "WorkCanvasPanel.node.spawnTask": "Spawn task",
  "WorkCanvasPanel.node.spawnTaskSourceLabel": "Work Canvas node",
  "WorkCanvasPanel.node.remove": "Remove from canvas",
  "WorkCanvasPanel.node.connect": "Drag to connect to another node",
  "WorkCanvasPanel.node.notePlaceholder": "Write a note…",
  "WorkCanvasPanel.node.chatLabel": "Chat",
  "WorkCanvasPanel.node.runLabel": "Task / run",
  "WorkCanvasPanel.node.noteLabel": "Note",
  "WorkCanvasPanel.edge.remove": "Click to remove this connection",
  "WorkCanvasPanel.helpHint": "Drag the background to pan, scroll to zoom, drag a card to move it, drag from a card's dot onto another to connect them.",
};
