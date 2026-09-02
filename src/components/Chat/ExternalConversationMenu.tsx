import { useEffect, useRef, useState } from "react";
import { createPortal } from "react-dom";
import { save } from "@tauri-apps/plugin-dialog";
import {
  Archive,
  ArchiveRestore,
  ChevronRight,
  FileDown,
  FolderInput,
  GitFork,
  Mail,
  MailOpen,
  Pencil,
  Pin,
  PinOff,
  Trash2,
} from "lucide-react";

import { type ChatSession, useSessionStore } from "../../store/sessionStore";
import {
  conversationKey,
  useExternalConversationStore,
  type ExternalSelection,
} from "../../store/externalConversationStore";
import {
  useExternalConversationMetaStore,
  type ExternalConversationMeta,
} from "../../store/externalConversationMetaStore";
import { useT } from "../../lib/i18n";
import { exportPortableSession } from "../../lib/portability";
import { errorMessage } from "../../lib/errors";
import type { SessionRow } from "./sessionListView";

interface ExternalConversationMenuProps {
  row: Extract<SessionRow, { kind: "external" }>;
  meta: ExternalConversationMeta;
  anchorRect: DOMRect;
  onClose: () => void;
  onRename: () => void;
}

/** Matches `w-56`, like `SessionMenu`. */
const MENU_WIDTH = 224;
const VIEWPORT_MARGIN = 8;

const itemClass =
  "flex w-full cursor-pointer items-center justify-between gap-3 px-3 py-1.5 text-left text-sm text-foreground hover:bg-surface-2";
const submenuClass =
  "invisible absolute left-full top-0 z-30 w-48 rounded-lg border border-border bg-background py-1 opacity-0 shadow-lg transition-opacity";

/**
 * The kebab menu of an outside conversation — a paired phone's chat, a
 * messaging thread the agent answers — with the same actions a local row has,
 * wherever they mean something here.
 *
 * Pin, unread, rename, group and archive are this desktop's notes about the
 * conversation (`externalConversationMetaStore`): the daemon owns the
 * conversation and knows nothing of how this sidebar files it. Fork copies the
 * transcript into a local session — the way to keep talking about it with the
 * agent here, and the way to reach everything a local session can do
 * (translate, open in an editor). Export writes the transcript out. Delete
 * erases the conversation from this machine.
 *
 * Deliberately not here: "Open in" (there is no workspace to open; a forked
 * copy has one) and "Translate thread" (translation is kept per local session;
 * again, fork first). Both are one fork away rather than half-working here.
 */
export function ExternalConversationMenu({
  row,
  meta,
  anchorRect,
  onClose,
  onRename,
}: ExternalConversationMenuProps) {
  const { t } = useT();
  const key = row.id;
  const selection: ExternalSelection = {
    environment: row.conversation.environment,
    id: row.conversation.id,
  };
  const groups = useSessionStore((s) => s.groups).filter((group) => group.kind === "folder");
  const createGroup = useSessionStore((s) => s.createGroup);
  const importPortableSessions = useSessionStore((s) => s.importPortableSessions);
  const switchSession = useSessionStore((s) => s.switchSession);
  const update = useExternalConversationMetaStore((s) => s.update);
  const forgetMeta = useExternalConversationMetaStore((s) => s.forget);
  const remove = useExternalConversationStore((s) => s.remove);
  const selectExternal = useExternalConversationStore((s) => s.select);

  const [newGroupOpen, setNewGroupOpen] = useState(false);
  const [newGroupName, setNewGroupName] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const menuRef = useRef<HTMLDivElement>(null);

  useEffect(() => {
    function handlePointerDown(event: PointerEvent) {
      const target = event.target as Element | null;
      if (
        menuRef.current &&
        !menuRef.current.contains(event.target as Node) &&
        !target?.closest("[data-session-menu-trigger]")
      ) {
        onClose();
      }
    }
    function handleKeyDown(event: KeyboardEvent) {
      if (event.key === "Escape") onClose();
    }
    document.addEventListener("pointerdown", handlePointerDown);
    document.addEventListener("keydown", handleKeyDown);
    return () => {
      document.removeEventListener("pointerdown", handlePointerDown);
      document.removeEventListener("keydown", handleKeyDown);
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  /** The conversation as a local session would hold it: what this machine
   * recorded, loaded first if the pane has not shown it yet. */
  const transcriptSession = async (): Promise<ChatSession> => {
    const store = useExternalConversationStore.getState();
    if (!store.messages[conversationKey(selection)]) await store.loadMessages(selection);
    const messages = useExternalConversationStore.getState().messages[conversationKey(selection)] ?? [];
    return {
      id: crypto.randomUUID(),
      title: row.title,
      messages: messages.map((message) => ({
        role: message.role === "assistant" ? "assistant" : "user",
        content: message.text,
        at: message.at_ms,
      })),
      createdAt: row.createdAt,
      updatedAt: row.updatedAt,
      pinned: false,
      unread: false,
      archived: false,
      groupId: meta.groupId,
      modelTarget: null,
      comparisonBranch: null,
      workspacePath: null,
      personaId: null,
      attachedStackIds: [],
      docChatMode: false,
      subagentRuns: {},
    };
  };

  const handleFork = async () => {
    setBusy(true);
    setError(null);
    try {
      const session = await transcriptSession();
      // Merge keeps the fresh id it was given; the copy is a session of its
      // own from here on, and opening it takes the main pane back.
      importPortableSessions([session], "merge");
      selectExternal(null);
      switchSession(session.id);
      onClose();
    } catch (reason) {
      setError(errorMessage(reason));
    } finally {
      setBusy(false);
    }
  };

  const handleExport = async (format: "markdown" | "json" | "docx") => {
    setBusy(true);
    setError(null);
    try {
      const extension = format === "markdown" ? "md" : format;
      const safeTitle =
        row.title.replace(/[^A-Za-z0-9._-]+/g, "-").replace(/^-+|-+$/g, "") || "conversation";
      const path = await save({
        defaultPath: `${safeTitle}.${extension}`,
        filters: [
          {
            name: format === "docx" ? "Word document" : format === "json" ? "JSON" : "Markdown",
            extensions: [extension],
          },
        ],
      });
      if (path) await exportPortableSession(path, await transcriptSession(), format);
      onClose();
    } catch (reason) {
      setError(errorMessage(reason));
    } finally {
      setBusy(false);
    }
  };

  const handleCreateGroup = () => {
    const id = createGroup(newGroupName);
    if (id) update(key, { groupId: id });
    setNewGroupName("");
    setNewGroupOpen(false);
    onClose();
  };

  const handleDelete = () => {
    if (!window.confirm(t("ExternalConversation.deleteConfirm", { title: row.title }))) return;
    void remove(selection).then(() => forgetMeta(key));
    onClose();
  };

  const left = Math.min(
    Math.max(anchorRect.right - MENU_WIDTH, VIEWPORT_MARGIN),
    window.innerWidth - MENU_WIDTH - VIEWPORT_MARGIN,
  );
  const top = anchorRect.bottom + 4;

  return createPortal(
    <div
      ref={menuRef}
      role="menu"
      style={{ position: "fixed", top, left, width: MENU_WIDTH }}
      className="z-50 rounded-lg border border-border bg-background py-1 shadow-lg"
      onClick={(event) => event.stopPropagation()}
      onPointerDown={(event) => event.stopPropagation()}
    >
      <button
        type="button"
        onClick={() => {
          update(key, { pinned: !meta.pinned });
          onClose();
        }}
        className={itemClass}
      >
        <span className="flex items-center gap-2">
          {meta.pinned ? <PinOff size={14} className="text-faint" /> : <Pin size={14} className="text-faint" />}
          {meta.pinned ? t("SessionMenu.unpin") : t("SessionMenu.pin")}
        </span>
      </button>
      <button
        type="button"
        onClick={() => {
          update(key, { unread: !meta.unread });
          onClose();
        }}
        className={itemClass}
      >
        <span className="flex items-center gap-2">
          {meta.unread ? <MailOpen size={14} className="text-faint" /> : <Mail size={14} className="text-faint" />}
          {meta.unread ? t("SessionMenu.markAsRead") : t("SessionMenu.markAsUnread")}
        </span>
      </button>
      <button
        type="button"
        onClick={() => {
          onRename();
          onClose();
        }}
        className={itemClass}
      >
        <span className="flex items-center gap-2">
          <Pencil size={14} className="text-faint" />
          {t("SessionMenu.rename")}
        </span>
      </button>
      <button type="button" onClick={() => void handleFork()} disabled={busy} className={itemClass}>
        <span className="flex items-center gap-2">
          <GitFork size={14} className="text-faint" />
          {t("SessionMenu.fork")}
        </span>
      </button>

      <div className="group/export relative">
        <button type="button" className={itemClass} disabled={busy}>
          <span className="flex items-center gap-2">
            <FileDown size={14} className="text-faint" />
            {busy ? t("Portability.busy") : t("Portability.exportSession")}
          </span>
          <ChevronRight size={14} className="text-faint" />
        </button>
        <div
          className={`${submenuClass} group-hover/export:visible group-hover/export:opacity-100 focus-within:visible focus-within:opacity-100`}
        >
          <button type="button" className={itemClass} onClick={() => void handleExport("markdown")}>
            <span>{t("Portability.sessionMarkdown")}</span>
          </button>
          <button type="button" className={itemClass} onClick={() => void handleExport("json")}>
            <span>{t("Portability.sessionJson")}</span>
          </button>
          <button type="button" className={itemClass} onClick={() => void handleExport("docx")}>
            <span>{t("Portability.sessionWord")}</span>
          </button>
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
        <div
          className={`${submenuClass} group-hover/movegroup:visible group-hover/movegroup:opacity-100 focus-within:visible focus-within:opacity-100`}
        >
          {meta.groupId && (
            <button
              type="button"
              onClick={() => {
                update(key, { groupId: null });
                onClose();
              }}
              className={itemClass}
            >
              <span>{t("SessionMenu.noGroup")}</span>
            </button>
          )}
          {groups.map((group) => (
            <button
              key={group.id}
              type="button"
              onClick={() => {
                update(key, { groupId: group.id });
                onClose();
              }}
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
        onClick={() => {
          // Archiving un-pins, as it does for a local session: a pinned row
          // is one to keep in sight, an archived one is not.
          update(key, meta.archived ? { archived: false } : { archived: true, pinned: false });
          onClose();
        }}
        className={itemClass}
      >
        <span className="flex items-center gap-2">
          {meta.archived ? (
            <ArchiveRestore size={14} className="text-faint" />
          ) : (
            <Archive size={14} className="text-faint" />
          )}
          {meta.archived ? t("SessionMenu.unarchive") : t("SessionMenu.archive")}
        </span>
      </button>
      <button type="button" onClick={handleDelete} className={`${itemClass} text-danger hover:bg-danger-soft`}>
        <span className="flex items-center gap-2">
          <Trash2 size={14} />
          {t("SessionMenu.delete")}
        </span>
      </button>
      {error && (
        <p className="px-3 py-1.5 text-xs text-danger" role="alert">
          {error}
        </p>
      )}
    </div>,
    document.body,
  );
}

export default ExternalConversationMenu;
