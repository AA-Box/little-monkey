import { useEffect, useRef, useState } from "react";
import { createPortal } from "react-dom";
import { invoke } from "@tauri-apps/api/core";
import {
  Archive,
  ArchiveRestore,
  ChevronRight,
  Code2,
  AppWindow,
  Columns2,
  FolderInput,
  FolderOpen,
  GitFork,
  Mail,
  MailOpen,
  Pencil,
  Pin,
  PinOff,
  Trash2,
} from "lucide-react";

import { type ChatSession, useSessionStore } from "../../store/sessionStore";
import { primaryRoot, useWorkspaceStore } from "../../store/workspaceStore";
import { useT } from "../../lib/i18n";

interface SessionMenuProps {
  session: ChatSession;
  /** Trigger button's `getBoundingClientRect()`, captured at open time —
   * the menu is portaled to `document.body` (see below) and positions
   * itself against this instead of relying on CSS `absolute`, since the
   * sidebar's scroll container clips anything that overflows its bounds. */
  anchorRect: DOMRect;
  onClose: () => void;
  onRename: () => void;
}

/** Matches `w-56` — used to compute the portaled menu's fixed position. */
const MENU_WIDTH = 224;
const VIEWPORT_MARGIN = 8;

const itemClass =
  "flex w-full cursor-pointer items-center justify-between gap-3 px-3 py-1.5 text-left text-sm text-foreground hover:bg-surface-2";

// Flush against the parent menu's right edge (no margin) — a gap would be a
// hover dead zone: crossing it drops :hover on the group, hiding the
// submenu before the pointer can reach it.
const submenuClass =
  "invisible absolute left-full top-0 z-30 w-48 rounded-lg border border-border bg-background py-1 opacity-0 shadow-lg transition-opacity";

/**
 * Claude-Desktop-style kebab dropdown for a `ChatSessionList` row. "Open in"
 * and "Move to group" are nested panels opened on hover via Tailwind named
 * groups (`group/openin`, `group/movegroup`) — no JS hover state needed.
 * Every top-level action also has a real single-letter/digit shortcut that
 * fires while the menu is open (see the keydown listener below), matching
 * the mnemonics shown on the right of each row.
 */
export function SessionMenu({ session, anchorRect, onClose, onRename }: SessionMenuProps) {
  const { t } = useT();
  const groups = useSessionStore((s) => s.groups);
  const togglePin = useSessionStore((s) => s.togglePin);
  const toggleUnread = useSessionStore((s) => s.toggleUnread);
  const forkSession = useSessionStore((s) => s.forkSession);
  const moveToGroup = useSessionStore((s) => s.moveToGroup);
  const createGroup = useSessionStore((s) => s.createGroup);
  const archiveSession = useSessionStore((s) => s.archiveSession);
  const unarchiveSession = useSessionStore((s) => s.unarchiveSession);
  const deleteSession = useSessionStore((s) => s.deleteSession);

  const [newGroupOpen, setNewGroupOpen] = useState(false);
  const [newGroupName, setNewGroupName] = useState("");
  const menuRef = useRef<HTMLDivElement>(null);

  // The trigger button lives in the sidebar; this menu is portaled to
  // `document.body`, so the pointerdown-outside-closes pattern used
  // elsewhere (see WorkspaceBar) needs its own ref here instead of relying
  // on a shared ancestor with the row.
  useEffect(() => {
    function handlePointerDown(event: PointerEvent) {
      if (menuRef.current && !menuRef.current.contains(event.target as Node)) {
        onClose();
      }
    }
    document.addEventListener("pointerdown", handlePointerDown);
    return () => document.removeEventListener("pointerdown", handlePointerDown);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  // Sessions aren't otherwise tied to a workspace (it's app-global and can
  // change after a session is created) — fall back to whatever's currently
  // open if this session predates `workspacePath` or never had one.
  const resolveWorkspacePath = () =>
    session.workspacePath ?? primaryRoot(useWorkspaceStore.getState().roots)?.path ?? null;

  // In-window split pane (Claude-Desktop-style) — see App.tsx.
  const openSplit = () => {
    useSessionStore.getState().openSplit(session.id);
  };

  const openWindow = () => {
    void invoke("open_session_window", { sessionId: session.id }).catch((err) => console.error(err));
  };

  const openEditor = (editor: "cursor" | "vscode") => {
    const path = resolveWorkspacePath();
    if (!path) return;
    void invoke("open_in_editor", { path, editor }).catch((err) => console.error(err));
  };

  const openFinder = () => {
    const path = resolveWorkspacePath();
    if (!path) return;
    void invoke("reveal_in_finder", { path }).catch((err) => console.error(err));
  };

  const handleCreateGroup = () => {
    const id = createGroup(newGroupName);
    if (id) moveToGroup(session.id, id);
    setNewGroupName("");
    setNewGroupOpen(false);
    onClose();
  };

  // Real keyboard shortcuts while the menu is open, matching the mnemonics
  // shown on each row. Suspended while the "new group" name field has focus
  // so typing a name doesn't also trigger actions.
  useEffect(() => {
    function handleKeyDown(event: KeyboardEvent) {
      if (newGroupOpen) return;
      switch (event.key.toLowerCase()) {
        case "escape":
          onClose();
          break;
        case "1":
          openSplit();
          onClose();
          break;
        case "2":
          openWindow();
          onClose();
          break;
        case "3":
          openEditor("cursor");
          onClose();
          break;
        case "4":
          openEditor("vscode");
          onClose();
          break;
        case "5":
          openFinder();
          onClose();
          break;
        case "p":
          togglePin(session.id);
          onClose();
          break;
        case "u":
          toggleUnread(session.id);
          onClose();
          break;
        case "r":
          onRename();
          onClose();
          break;
        case "f":
          forkSession(session.id);
          onClose();
          break;
        case "a":
          (session.archived ? unarchiveSession : archiveSession)(session.id);
          onClose();
          break;
        case "d":
          deleteSession(session.id);
          onClose();
          break;
        default:
          break;
      }
    }
    document.addEventListener("keydown", handleKeyDown);
    return () => document.removeEventListener("keydown", handleKeyDown);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [newGroupOpen, session.id, session.archived]);

  // Fixed (viewport) coordinates, computed from the trigger button's rect —
  // NOT CSS `absolute`, since that would anchor to the sidebar's
  // `overflow-y-auto` container and get clipped at its edge the moment the
  // menu (or its "Open in"/"Move to group" submenus) needs to extend past
  // the sidebar's own width.
  const left = Math.min(
    Math.max(anchorRect.right - MENU_WIDTH, VIEWPORT_MARGIN),
    window.innerWidth - MENU_WIDTH - VIEWPORT_MARGIN,
  );
  const top = anchorRect.bottom + 4;

  return createPortal(
    // stopPropagation on click/pointerdown: the portal is declared inside
    // the session row's JSX, so React synthetic events would otherwise
    // bubble through the React tree to the row's onClick and fire
    // switchSession after every menu action — which, among other things,
    // immediately clears the unread flag "Mark as unread" just set.
    <div
      ref={menuRef}
      style={{ position: "fixed", top, left, width: MENU_WIDTH }}
      className="z-30 rounded-lg border border-border bg-background py-1 shadow-lg"
      onClick={(event) => event.stopPropagation()}
      onPointerDown={(event) => event.stopPropagation()}
    >
      <div className="group/openin relative">
        <button type="button" className={itemClass}>
          <span>{t("SessionMenu.openIn")}</span>
          <ChevronRight size={14} className="text-faint" />
        </button>
        <div className={`${submenuClass} group-hover/openin:visible group-hover/openin:opacity-100`}>
          <button type="button" onClick={() => { openSplit(); onClose(); }} className={itemClass}>
            <span className="flex items-center gap-2">
              <Columns2 size={14} className="text-faint" />
              {t("SessionMenu.splitView")}
            </span>
            <kbd className="text-xs text-faint">1</kbd>
          </button>
          <button type="button" onClick={() => { openWindow(); onClose(); }} className={itemClass}>
            <span className="flex items-center gap-2">
              <AppWindow size={14} className="text-faint" />
              {t("SessionMenu.newWindow")}
            </span>
            <kbd className="text-xs text-faint">2</kbd>
          </button>
          <button type="button" onClick={() => { openEditor("cursor"); onClose(); }} className={itemClass}>
            <span className="flex items-center gap-2">
              <Code2 size={14} className="text-faint" />
              {t("SessionMenu.cursor")}
            </span>
            <kbd className="text-xs text-faint">3</kbd>
          </button>
          <button type="button" onClick={() => { openEditor("vscode"); onClose(); }} className={itemClass}>
            <span className="flex items-center gap-2">
              <Code2 size={14} className="text-faint" />
              {t("SessionMenu.vscode")}
            </span>
            <kbd className="text-xs text-faint">4</kbd>
          </button>
          <button type="button" onClick={() => { openFinder(); onClose(); }} className={itemClass}>
            <span className="flex items-center gap-2">
              <FolderOpen size={14} className="text-faint" />
              {t("SessionMenu.finder")}
            </span>
            <kbd className="text-xs text-faint">5</kbd>
          </button>
        </div>
      </div>

      <div className="my-1 border-t border-border" />

      <button type="button" onClick={() => { togglePin(session.id); onClose(); }} className={itemClass}>
        <span className="flex items-center gap-2">
          {session.pinned ? <PinOff size={14} className="text-faint" /> : <Pin size={14} className="text-faint" />}
          {session.pinned ? t("SessionMenu.unpin") : t("SessionMenu.pin")}
        </span>
        <kbd className="text-xs text-faint">P</kbd>
      </button>
      <button type="button" onClick={() => { toggleUnread(session.id); onClose(); }} className={itemClass}>
        <span className="flex items-center gap-2">
          {session.unread ? (
            <MailOpen size={14} className="text-faint" />
          ) : (
            <Mail size={14} className="text-faint" />
          )}
          {session.unread ? t("SessionMenu.markAsRead") : t("SessionMenu.markAsUnread")}
        </span>
        <kbd className="text-xs text-faint">U</kbd>
      </button>
      <button type="button" onClick={() => { onRename(); onClose(); }} className={itemClass}>
        <span className="flex items-center gap-2">
          <Pencil size={14} className="text-faint" />
          {t("SessionMenu.rename")}
        </span>
        <kbd className="text-xs text-faint">R</kbd>
      </button>
      <button type="button" onClick={() => { forkSession(session.id); onClose(); }} className={itemClass}>
        <span className="flex items-center gap-2">
          <GitFork size={14} className="text-faint" />
          {t("SessionMenu.fork")}
        </span>
        <kbd className="text-xs text-faint">F</kbd>
      </button>

      <div className="my-1 border-t border-border" />

      <div className="group/movegroup relative">
        <button type="button" className={itemClass}>
          <span className="flex items-center gap-2">
            <FolderInput size={14} className="text-faint" />
            {t("SessionMenu.moveToGroup")}
          </span>
          <ChevronRight size={14} className="text-faint" />
        </button>
        <div className={`${submenuClass} group-hover/movegroup:visible group-hover/movegroup:opacity-100`}>
          {session.groupId && (
            <button
              type="button"
              onClick={() => { moveToGroup(session.id, null); onClose(); }}
              className={itemClass}
            >
              <span>{t("SessionMenu.noGroup")}</span>
            </button>
          )}
          {groups.map((group) => (
            <button
              key={group.id}
              type="button"
              onClick={() => { moveToGroup(session.id, group.id); onClose(); }}
              className={itemClass}
            >
              <span className="truncate">{group.name}</span>
            </button>
          ))}
          {groups.length > 0 && <div className="my-1 border-t border-border" />}
          {newGroupOpen ? (
            <form
              className="px-3 py-1.5"
              onSubmit={(event) => {
                event.preventDefault();
                handleCreateGroup();
              }}
            >
              <input
                autoFocus
                value={newGroupName}
                onChange={(event) => setNewGroupName(event.target.value)}
                onKeyDown={(event) => event.stopPropagation()}
                placeholder={t("SessionMenu.newGroupPlaceholder")}
                className="w-full rounded-md border border-border bg-surface-2 px-2 py-1 text-xs text-foreground outline-none focus-visible:border-accent"
              />
            </form>
          ) : (
            <button type="button" onClick={() => setNewGroupOpen(true)} className={itemClass}>
              <span>{t("SessionMenu.newGroup")}</span>
            </button>
          )}
        </div>
      </div>

      <div className="my-1 border-t border-border" />

      <button
        type="button"
        onClick={() => { (session.archived ? unarchiveSession : archiveSession)(session.id); onClose(); }}
        className={itemClass}
      >
        <span className="flex items-center gap-2">
          {session.archived ? (
            <ArchiveRestore size={14} className="text-faint" />
          ) : (
            <Archive size={14} className="text-faint" />
          )}
          {session.archived ? t("SessionMenu.unarchive") : t("SessionMenu.archive")}
        </span>
        <kbd className="text-xs text-faint">A</kbd>
      </button>
      <button
        type="button"
        onClick={() => { deleteSession(session.id); onClose(); }}
        className={`${itemClass} text-danger hover:bg-danger-soft`}
      >
        <span className="flex items-center gap-2">
          <Trash2 size={14} />
          {t("SessionMenu.delete")}
        </span>
        <kbd className="text-xs text-danger/70">D</kbd>
      </button>
    </div>,
    document.body,
  );
}

export default SessionMenu;
