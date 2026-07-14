import { useEffect, useMemo, useRef, useState } from "react";
import { createPortal } from "react-dom";
import { invoke } from "@tauri-apps/api/core";
import { save } from "@tauri-apps/plugin-dialog";
import {
  Archive,
  ArchiveRestore,
  ChevronRight,
  Code2,
  AppWindow,
  Columns2,
  FolderInput,
  FolderOpen,
  FileDown,
  GitFork,
  Mail,
  MailOpen,
  Languages,
  LoaderCircle,
  Pencil,
  Pin,
  PinOff,
  Trash2,
} from "lucide-react";

import { type ChatSession, useSessionStore } from "../../store/sessionStore";
import { useShortcutStore } from "../../store/shortcutStore";
import { primaryRoot, useWorkspaceStore } from "../../store/workspaceStore";
import { useT } from "../../lib/i18n";
import {
  cancelTranslation,
  defaultTranslationLocale,
  threadTranslationKey,
  TRANSLATION_LOCALES,
  translateThread,
} from "../../lib/translation";
import { exportPortableSession } from "../../lib/portability";
import {
  detectShortcutPlatform,
  shortcutDisplayLabel,
  shortcutIdForEvent,
  type ShortcutId,
  type ShortcutIdForScope,
} from "../../lib/shortcuts";

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
 * Most actions are global shortcuts (see App.tsx) that act on the active
 * session, so their `kbd` hint only renders on the active session's own
 * menu — showing it on another row's menu would imply the chord affects
 * that row, when it actually always targets whatever session is active.
 * "Open side task", "Delete", and "Close menu" have no active-session
 * equivalent and keep their menu-only mnemonic regardless.
 */
export function SessionMenu({ session, anchorRect, onClose, onRename }: SessionMenuProps) {
  const { t } = useT();
  // Comparison groups are execution/result containers, not folders a user
  // can file unrelated sessions into.
  const allGroups = useSessionStore((s) => s.groups);
  const groups = useMemo(
    () => allGroups.filter((group) => group.kind === "folder"),
    [allGroups],
  );
  const activeSessionId = useSessionStore((s) => s.activeSessionId);
  const isActiveSession = session.id === activeSessionId;
  const togglePin = useSessionStore((s) => s.togglePin);
  const toggleUnread = useSessionStore((s) => s.toggleUnread);
  const forkSession = useSessionStore((s) => s.forkSession);
  const moveToGroup = useSessionStore((s) => s.moveToGroup);
  const createGroup = useSessionStore((s) => s.createGroup);
  const archiveSession = useSessionStore((s) => s.archiveSession);
  const unarchiveSession = useSessionStore((s) => s.unarchiveSession);
  const deleteSession = useSessionStore((s) => s.deleteSession);
  const setDisplayTranslationLocale = useSessionStore((s) => s.setDisplayTranslationLocale);
  const shortcutOverrides = useShortcutStore((s) => s.overrides);
  const platform = detectShortcutPlatform();
  const shortcutLabel = (id: ShortcutId) => shortcutDisplayLabel(id, platform, shortcutOverrides);

  const [newGroupOpen, setNewGroupOpen] = useState(false);
  const [newGroupName, setNewGroupName] = useState("");
  const [translationTarget, setTranslationTarget] = useState(
    session.displayTranslationLocale ?? defaultTranslationLocale(),
  );
  const [translating, setTranslating] = useState(false);
  const [translationError, setTranslationError] = useState<string | null>(null);
  const [exportError, setExportError] = useState<string | null>(null);
  const [exporting, setExporting] = useState(false);
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

  const handleTranslateThread = async () => {
    setTranslating(true);
    setTranslationError(null);
    try {
      await translateThread(session.id, translationTarget);
    } catch (error) {
      if (!(error instanceof DOMException && error.name === "AbortError")) {
        setTranslationError(error instanceof Error ? error.message : String(error));
      }
    } finally {
      setTranslating(false);
    }
  };

  const handleSessionExport = async (format: "markdown" | "json" | "docx") => {
    setExporting(true);
    setExportError(null);
    try {
      const extension = format === "markdown" ? "md" : format;
      const safeTitle = session.title.replace(/[^A-Za-z0-9._-]+/g, "-").replace(/^-+|-+$/g, "") || "conversation";
      const path = await save({
        defaultPath: `${safeTitle}.${extension}`,
        filters: [{
          name: format === "docx" ? "Word document" : format === "json" ? "JSON" : "Markdown",
          extensions: [extension],
        }],
      });
      if (path) await exportPortableSession(path, session, format);
    } catch (error) {
      setExportError(error instanceof Error ? error.message : String(error));
    } finally {
      setExporting(false);
    }
  };

  // Real keyboard shortcuts while the menu is open, matching the mnemonics
  // shown on each row. Suspended while the "new group" name field has focus
  // so typing a name doesn't also trigger actions.
  useEffect(() => {
    function handleKeyDown(event: KeyboardEvent) {
      if (event.repeat || event.isComposing) return;
      const { overrides, recordingId } = useShortcutStore.getState();
      // This document-capture listener also runs before the recorder target.
      // Do not let an open contextual menu consume the chord being recorded.
      if (recordingId !== null) return;
      // App-wide commands are handled at window-capture level. Close this
      // contextual menu as they pass through so it cannot remain active and
      // steal the next Escape behind a newly opened Settings modal.
      const eventPlatform = detectShortcutPlatform();
      if (shortcutIdForEvent(event, "global", eventPlatform, overrides)) {
        onClose();
        return;
      }
      if (newGroupOpen || event.defaultPrevented) return;
      const shortcut = shortcutIdForEvent(event, "sessionMenu", eventPlatform, overrides);
      if (!shortcut) return;

      const runAndClose = (action: () => void) => () => {
        action();
        onClose();
      };
      // Pin/unread/rename/fork/archive/open-in-X are global shortcuts now
      // (see App.tsx) — the block above already closes this menu for them.
      // Only the actions with no active-session equivalent stay here.
      const actions: Record<ShortcutIdForScope<"sessionMenu">, () => void> = {
        sessionCloseMenu: onClose,
        sessionOpenSplit: runAndClose(openSplit),
        sessionDelete: runAndClose(() => deleteSession(session.id)),
      };

      event.preventDefault();
      event.stopPropagation();
      actions[shortcut]();
    }
    document.addEventListener("keydown", handleKeyDown, true);
    return () => document.removeEventListener("keydown", handleKeyDown, true);
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
            <kbd className="text-xs text-faint">{shortcutLabel("sessionOpenSplit")}</kbd>
          </button>
          <button type="button" onClick={() => { openWindow(); onClose(); }} className={itemClass}>
            <span className="flex items-center gap-2">
              <AppWindow size={14} className="text-faint" />
              {t("SessionMenu.newWindow")}
            </span>
            {isActiveSession && <kbd className="text-xs text-faint">{shortcutLabel("sessionOpenWindow")}</kbd>}
          </button>
          <button type="button" onClick={() => { openEditor("cursor"); onClose(); }} className={itemClass}>
            <span className="flex items-center gap-2">
              <Code2 size={14} className="text-faint" />
              {t("SessionMenu.cursor")}
            </span>
            {isActiveSession && <kbd className="text-xs text-faint">{shortcutLabel("sessionOpenCursor")}</kbd>}
          </button>
          <button type="button" onClick={() => { openEditor("vscode"); onClose(); }} className={itemClass}>
            <span className="flex items-center gap-2">
              <Code2 size={14} className="text-faint" />
              {t("SessionMenu.vscode")}
            </span>
            {isActiveSession && <kbd className="text-xs text-faint">{shortcutLabel("sessionOpenVsCode")}</kbd>}
          </button>
          <button type="button" onClick={() => { openFinder(); onClose(); }} className={itemClass}>
            <span className="flex items-center gap-2">
              <FolderOpen size={14} className="text-faint" />
              {t("SessionMenu.finder")}
            </span>
            {isActiveSession && <kbd className="text-xs text-faint">{shortcutLabel("sessionRevealFinder")}</kbd>}
          </button>
        </div>
      </div>

      <div className="my-1 border-t border-border" />

      <button type="button" onClick={() => { togglePin(session.id); onClose(); }} className={itemClass}>
        <span className="flex items-center gap-2">
          {session.pinned ? <PinOff size={14} className="text-faint" /> : <Pin size={14} className="text-faint" />}
          {session.pinned ? t("SessionMenu.unpin") : t("SessionMenu.pin")}
        </span>
        {isActiveSession && <kbd className="text-xs text-faint">{shortcutLabel("sessionTogglePin")}</kbd>}
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
        {isActiveSession && <kbd className="text-xs text-faint">{shortcutLabel("sessionToggleUnread")}</kbd>}
      </button>
      <button type="button" onClick={() => { onRename(); onClose(); }} className={itemClass}>
        <span className="flex items-center gap-2">
          <Pencil size={14} className="text-faint" />
          {t("SessionMenu.rename")}
        </span>
        {isActiveSession && <kbd className="text-xs text-faint">{shortcutLabel("sessionRename")}</kbd>}
      </button>
      <button type="button" onClick={() => { forkSession(session.id); onClose(); }} className={itemClass}>
        <span className="flex items-center gap-2">
          <GitFork size={14} className="text-faint" />
          {t("SessionMenu.fork")}
        </span>
        {isActiveSession && <kbd className="text-xs text-faint">{shortcutLabel("sessionFork")}</kbd>}
      </button>

      <div className="group/translate relative">
        <button type="button" className={itemClass}>
          <span className="flex items-center gap-2">
            <Languages size={14} className="text-faint" />
            {t("Translation.translateThread")}
          </span>
          <ChevronRight size={14} className="text-faint" />
        </button>
        <div className={`${submenuClass} group-hover/translate:visible group-hover/translate:opacity-100 focus-within:visible focus-within:opacity-100`}>
          <div className="px-3 py-2">
            <label className="mb-1 block text-[11px] font-medium text-faint" htmlFor={`translation-${session.id}`}>
              {t("Translation.languageLabel")}
            </label>
            <select
              id={`translation-${session.id}`}
              value={translationTarget}
              disabled={translating}
              onChange={(event) => setTranslationTarget(event.target.value)}
              className="w-full cursor-pointer rounded-md border border-border bg-surface-2 px-2 py-1 text-xs text-foreground outline-none focus-visible:border-accent"
            >
              {TRANSLATION_LOCALES.map(({ code, label }) => <option key={code} value={code}>{label}</option>)}
            </select>
          </div>
          {translating ? (
            <button
              type="button"
              onClick={() => cancelTranslation(threadTranslationKey(session.id))}
              className={itemClass}
            >
              <span className="flex items-center gap-2">
                <LoaderCircle size={14} className="animate-spin text-faint" />
                {t("Translation.cancel")}
              </span>
            </button>
          ) : (
            <button type="button" onClick={() => void handleTranslateThread()} className={itemClass}>
              <span className="flex items-center gap-2">
                <Languages size={14} className="text-faint" />
                {t("Translation.translateThread")}
              </span>
            </button>
          )}
          {session.displayTranslationLocale && (
            <button
              type="button"
              onClick={() => setDisplayTranslationLocale(session.id, null)}
              className={itemClass}
            >
              <span>{t("Translation.showOriginalThread")}</span>
            </button>
          )}
          {translationError && <p className="px-3 py-1.5 text-xs text-danger" role="alert">{translationError}</p>}
        </div>
      </div>

      <div className="group/export relative">
        <button type="button" className={itemClass} disabled={exporting}>
          <span className="flex items-center gap-2">
            <FileDown size={14} className="text-faint" />
            {exporting ? t("Portability.busy") : t("Portability.exportSession")}
          </span>
          <ChevronRight size={14} className="text-faint" />
        </button>
        <div className={`${submenuClass} group-hover/export:visible group-hover/export:opacity-100 focus-within:visible focus-within:opacity-100`}>
          <button type="button" className={itemClass} onClick={() => void handleSessionExport("markdown")}>
            <span>{t("Portability.sessionMarkdown")}</span>
          </button>
          <button type="button" className={itemClass} onClick={() => void handleSessionExport("json")}>
            <span>{t("Portability.sessionJson")}</span>
          </button>
          <button type="button" className={itemClass} onClick={() => void handleSessionExport("docx")}>
            <span>{t("Portability.sessionWord")}</span>
          </button>
          {exportError && <p className="px-3 py-1.5 text-xs text-danger" role="alert">{exportError}</p>}
        </div>
      </div>

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
        {isActiveSession && <kbd className="text-xs text-faint">{shortcutLabel("sessionArchive")}</kbd>}
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
        <kbd className="text-xs text-danger/70">{shortcutLabel("sessionDelete")}</kbd>
      </button>
    </div>,
    document.body,
  );
}

export default SessionMenu;
