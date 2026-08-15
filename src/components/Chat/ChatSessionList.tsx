import { useEffect, useState } from "react";
import { AlertTriangle, MoreVertical, Plus } from "lucide-react";

import { sessionDisplayTitle, type ChatSession, useSessionStore } from "../../store/sessionStore";
import { Button } from "../ui";
import { useT } from "../../lib/i18n";
import { SessionMenu } from "./SessionMenu";
import { usePermissionStore } from "../../store/permissionStore";
import { sessionsAwaitingPermission, sessionStatus, type SessionStatus } from "./sessionStatus";

/**
 * Claude-Desktop-style session list for the left sidebar: a "New session"
 * action, then Pinned / per-group / Recents sections (each sorted
 * most-recently-active first), plus a collapsed "Archived" footer section.
 * Click a row to switch, hover for the kebab menu (rename/pin/fork/group/
 * archive/delete/"open in" — see `SessionMenu`).
 */
/** Tailwind classes for each status' dot. `working` animates — a row that
 * is doing something should be the one thing moving in the sidebar. */
const STATUS_DOT: Record<Exclude<SessionStatus, "attention">, string> = {
  working: "bg-accent animate-pulse",
  finished: "bg-accent",
  error: "bg-danger",
};

/** The leading status marker on a session row. Never color alone: the
 * hover/screen-reader label names the state, and the two states worth
 * interrupting for carry a shape of their own (a pulse, a triangle). */
function StatusMarker({ status, label }: { status: SessionStatus; label: string }) {
  return (
    <span
      role="img"
      aria-label={label}
      title={label}
      className="mr-1.5 inline-flex shrink-0 items-center align-middle"
    >
      {status === "attention" ? (
        <AlertTriangle size={12} className="text-warning" aria-hidden />
      ) : (
        <span className={`h-1.5 w-1.5 rounded-full ${STATUS_DOT[status]}`} />
      )}
    </span>
  );
}

export default function ChatSessionList() {
  const sessions = useSessionStore((state) => state.sessions);
  const groups = useSessionStore((state) => state.groups);
  const activeSessionId = useSessionStore((state) => state.activeSessionId);
  const runningTurns = useSessionStore((state) => state.runningTurns);
  const turnOutcomes = useSessionStore((state) => state.turnOutcomes);
  const turnSessions = useSessionStore((state) => state.turnSessions);
  const permissionQueue = usePermissionStore((state) => state.queue);
  const awaitingPermission = sessionsAwaitingPermission(permissionQueue, turnSessions);
  const newSession = useSessionStore((state) => state.newSession);
  const switchSession = useSessionStore((state) => state.switchSession);
  const renameSession = useSessionStore((state) => state.renameSession);
  const renameRequestId = useSessionStore((state) => state.renameRequestId);
  const clearRenameRequest = useSessionStore((state) => state.clearRenameRequest);
  const { t } = useT();

  const [menuOpenId, setMenuOpenId] = useState<string | null>(null);
  const [menuAnchor, setMenuAnchor] = useState<DOMRect | null>(null);
  const [renamingId, setRenamingId] = useState<string | null>(null);
  const [renameValue, setRenameValue] = useState("");
  const [archivedOpen, setArchivedOpen] = useState(false);

  const byUpdatedDesc = (a: ChatSession, b: ChatSession) => b.updatedAt - a.updatedAt;

  // Unstarted sessions (no messages yet — see `newSession`'s reset-in-place
  // logic) stay out of the sidebar entirely, same as Claude Desktop: a "New
  // session" only earns a row once the user actually sends something.
  const started = sessions.filter((s) => s.messages.length > 0);
  const active = started.filter((s) => !s.archived);
  const archivedSessions = started.filter((s) => s.archived).sort(byUpdatedDesc);
  const pinned = active.filter((s) => s.pinned).sort(byUpdatedDesc);
  const groupedSections = groups
    .map((group) => ({
      group,
      items: active.filter((s) => !s.pinned && s.groupId === group.id).sort(byUpdatedDesc),
    }))
    .filter((section) => section.items.length > 0);
  const ungrouped = active.filter((s) => !s.pinned && !s.groupId).sort(byUpdatedDesc);

  const startRename = (session: ChatSession) => {
    setRenameValue(session.title);
    setRenamingId(session.id);
  };

  const commitRename = () => {
    if (renamingId) renameSession(renamingId, renameValue);
    setRenamingId(null);
  };

  // The global "Rename" shortcut (App.tsx) sets `renameRequestId` on the
  // store rather than reaching into this component's local rename state
  // directly — pick it up here and hand off to the same inline input the
  // kebab menu's "Rename" uses.
  useEffect(() => {
    if (!renameRequestId) return;
    const target = sessions.find((s) => s.id === renameRequestId);
    if (target) {
      startRename(target);
      // The row only exists in the DOM once its section is open — an
      // archived active session would otherwise start renaming invisibly.
      if (target.archived) setArchivedOpen(true);
    }
    clearRenameRequest();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [renameRequestId]);

  const renderRow = (session: ChatSession) => {
    const isActive = session.id === activeSessionId;
    const isRenaming = renamingId === session.id;
    const isMenuOpen = menuOpenId === session.id;
    const status = sessionStatus(
      session,
      runningTurns[session.id] === true,
      turnOutcomes[session.id],
      awaitingPermission.has(session.id),
    );

    return (
      <div
        key={session.id}
        role="button"
        tabIndex={0}
        onClick={() => !isRenaming && switchSession(session.id)}
        onKeyDown={(event) => {
          if (isRenaming) return;
          if (event.key === "Enter" || event.key === " ") {
            event.preventDefault();
            switchSession(session.id);
          }
        }}
        className={`group relative flex cursor-pointer items-center justify-between gap-2 rounded-md px-2.5 py-1 text-sm ${
          isActive || isMenuOpen ? "bg-surface-2 text-foreground" : "hover:bg-surface-2"
        }`}
      >
        {isRenaming ? (
          <input
            autoFocus
            value={renameValue}
            onChange={(event) => setRenameValue(event.target.value)}
            onClick={(event) => event.stopPropagation()}
            onBlur={commitRename}
            onKeyDown={(event) => {
              event.stopPropagation();
              if (event.key === "Enter") commitRename();
              if (event.key === "Escape") setRenamingId(null);
            }}
            className="w-full rounded-md border border-border bg-surface px-1.5 py-0.5 text-sm text-foreground outline-none focus-visible:border-accent"
          />
        ) : (
          <span className={`truncate ${session.unread ? "font-semibold" : ""}`}>
            {status && <StatusMarker status={status} label={t(`ChatSessionList.status.${status}`)} />}
            {sessionDisplayTitle(session)}
          </span>
        )}

        {!isRenaming && (
          <button
            type="button"
            onClick={(event) => {
              event.stopPropagation();
              if (isMenuOpen) {
                setMenuOpenId(null);
              } else {
                setMenuAnchor(event.currentTarget.getBoundingClientRect());
                setMenuOpenId(session.id);
              }
            }}
            aria-label={t("ChatSessionList.sessionMenuAriaLabel")}
            className={`shrink-0 rounded p-0.5 text-faint hover:bg-surface hover:text-foreground ${
              isMenuOpen ? "opacity-100" : "opacity-0 group-hover:opacity-100"
            }`}
          >
            <MoreVertical size={14} />
          </button>
        )}

        {isMenuOpen && menuAnchor && (
          <SessionMenu
            session={session}
            anchorRect={menuAnchor}
            onClose={() => setMenuOpenId(null)}
            onRename={() => startRename(session)}
          />
        )}
      </div>
    );
  };

  return (
    <div>
      <div className="p-2">
        <Button variant="secondary" size="md" onClick={() => newSession()} className="w-full justify-start">
          <Plus size={16} className="shrink-0" />
          <span>{t("ChatSessionList.newSession")}</span>
        </Button>
      </div>

      {pinned.length > 0 && (
        <>
          <h2 className="px-3 pb-1 pt-3 text-[11px] font-semibold uppercase tracking-wider text-faint">
            {t("ChatSessionList.pinnedHeading")}
          </h2>
          <div className="flex flex-col px-2 pb-2">{pinned.map(renderRow)}</div>
        </>
      )}

      {groupedSections.map(({ group, items }) => (
        <div key={group.id}>
          <h2 className="px-3 pb-1 pt-3 text-[11px] font-semibold uppercase tracking-wider text-faint">
            {group.name}
          </h2>
          <div className="flex flex-col px-2 pb-2">{items.map(renderRow)}</div>
        </div>
      ))}

      <h2 className="px-3 pb-1 pt-3 text-[11px] font-semibold uppercase tracking-wider text-faint">
        {t("ChatSessionList.recentsHeading")}
      </h2>
      {ungrouped.length > 0 && <div className="flex flex-col px-2 pb-2">{ungrouped.map(renderRow)}</div>}

      {archivedSessions.length > 0 && (
        <div className="px-2 pb-2">
          <button
            type="button"
            onClick={() => setArchivedOpen((prev) => !prev)}
            className="w-full px-1 pb-1 pt-2 text-left text-[11px] font-semibold uppercase tracking-wider text-faint hover:text-muted"
          >
            {t("ChatSessionList.archivedHeading", { count: archivedSessions.length })}
          </button>
          {archivedOpen && <div className="flex flex-col">{archivedSessions.map(renderRow)}</div>}
        </div>
      )}
    </div>
  );
}
