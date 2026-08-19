import { useEffect, useMemo, useState, type ReactNode } from "react";
import { invoke, isTauri } from "@tauri-apps/api/core";
import {
  AlertTriangle,
  Archive,
  ArchiveRestore,
  MessagesSquare,
  MoreVertical,
  Pin,
  Plus,
  SlidersHorizontal,
  Smartphone,
} from "lucide-react";

import { type ChatSession, useSessionStore } from "../../store/sessionStore";
import { Button } from "../ui";
import { useT } from "../../lib/i18n";
import { SessionMenu } from "./SessionMenu";
import { SessionListMenu, useEnvironmentLabel } from "./SessionListMenu";
import { usePermissionStore } from "../../store/permissionStore";
import { useSessionListViewStore } from "../../store/sessionListViewStore";
import { useExternalConversationStore } from "../../store/externalConversationStore";
import { useGitDeliveryStore } from "../../store/gitDeliveryStore";
import { primaryRoot, useWorkspaceStore } from "../../store/workspaceStore";
import { REMOTE_CONTROL_ENVIRONMENT } from "../../lib/conversationsClient";
import { detectShortcutPlatform } from "../../lib/shortcuts";
import { SessionGitBadge, type SessionGitContext } from "./SessionGitBadge";
import {
  buildSessionListView,
  environmentOptions,
  externalRow,
  localRow,
  type SessionRow,
} from "./sessionListView";
import { sessionsAwaitingPermission, sessionStatus, type SessionStatus } from "./sessionStatus";

interface GitStatusSnapshot {
  is_repo: boolean;
  branch: string | null;
  changed_files: number;
  is_worktree: boolean;
  worktree_name: string | null;
}

const EMPTY_GIT_STATUS: GitStatusSnapshot = {
  is_repo: false,
  branch: null,
  changed_files: 0,
  is_worktree: false,
  worktree_name: null,
};

function normalizedPath(path: string): string {
  const normalized = path.replace(/[\\/]+$/, "");
  return normalized || path;
}

function pathsEqual(left: string | null, right: string | null): boolean {
  return Boolean(left && right && normalizedPath(left) === normalizedPath(right));
}

/**
 * Claude-Desktop-style session list for the left sidebar: a "New session"
 * action, then Pinned / per-section / Recents sections, plus a collapsed
 * "Archived" footer section. Click a row to switch, hover for the kebab menu
 * (rename/pin/fork/group/archive/delete/"open in" — see `SessionMenu`).
 *
 * The list is not only this machine's: a paired phone's chat and a messaging
 * conversation the agent is answering appear as rows too, tagged with the
 * environment they live in and read-only (see `ExternalConversationView`).
 * How the whole list is filtered, grouped and ordered lives in
 * `sessionListView.ts`, behind the header's view menu.
 */
/** The leading status marker on a session row. Working is the only animated
 * state; finished is a solid dot, read/idle is hollow, and failures use an
 * icon so the state is not conveyed by color or motion alone. */
function StatusMarker({ status, label }: { status: SessionStatus | null; label: string }) {
  return (
    <span
      role="img"
      aria-label={label}
      title={label}
      className="mr-1.5 inline-flex shrink-0 items-center align-middle"
    >
      {status === "error" ? (
        <AlertTriangle size={12} className="text-danger" aria-hidden />
      ) : status === "attention" ? (
        <AlertTriangle size={12} className="text-warning" aria-hidden />
      ) : status === null ? (
        <span className="h-1.5 w-1.5 rounded-full border border-faint" />
      ) : (
        <span
          className={`h-1.5 w-1.5 rounded-full ${status === "working" ? "bg-accent animate-pulse motion-reduce:animate-none" : "bg-accent"}`}
        />
      )}
    </span>
  );
}

/** The marker an outside conversation carries instead of a status dot: which
 * environment it is in, since that is the thing about it a local row can't
 * also be. */
function EnvironmentMarker({ environment, label }: { environment: string; label: string }) {
  const Icon = environment === REMOTE_CONTROL_ENVIRONMENT ? Smartphone : MessagesSquare;
  return (
    <span role="img" aria-label={label} title={label} className="mr-1.5 inline-flex shrink-0 items-center align-middle">
      <Icon size={12} className="text-faint" aria-hidden />
    </span>
  );
}

/** A list section's heading row, optionally carrying the view menu's trigger. */
function SectionHeading({ title, action }: { title: string; action?: ReactNode }) {
  return (
    <div className="flex items-center justify-between gap-2 px-3 pb-1 pt-3">
      <h2 className="truncate text-[11px] font-semibold uppercase tracking-wider text-faint">{title}</h2>
      {action}
    </div>
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
  const conversations = useExternalConversationStore((state) => state.conversations);
  const selectedExternal = useExternalConversationStore((state) => state.selected);
  const selectExternal = useExternalConversationStore((state) => state.select);
  const refreshExternal = useExternalConversationStore((state) => state.refresh);
  const primaryWorkspacePath = useWorkspaceStore((state) => primaryRoot(state.roots)?.path ?? null);
  const deliveryWorktrees = useGitDeliveryStore((state) => state.worktrees);
  const deliverySelectedWorktreeId = useGitDeliveryStore((state) => state.selectedWorktreeId);
  const deliveryPullRequest = useGitDeliveryStore((state) => state.pullRequest);
  const deliveryChecks = useGitDeliveryStore((state) => state.checks);
  const refreshDelivery = useGitDeliveryStore((state) => state.refresh);
  const togglePin = useSessionStore((state) => state.togglePin);
  const archiveSession = useSessionStore((state) => state.archiveSession);
  const unarchiveSession = useSessionStore((state) => state.unarchiveSession);
  const { t } = useT();
  const environmentLabel = useEnvironmentLabel();

  const [menuOpenId, setMenuOpenId] = useState<string | null>(null);
  const [menuAnchor, setMenuAnchor] = useState<DOMRect | null>(null);
  const [viewMenuAnchor, setViewMenuAnchor] = useState<DOMRect | null>(null);
  const [renamingId, setRenamingId] = useState<string | null>(null);
  const [renameValue, setRenameValue] = useState("");
  const [archivedOpen, setArchivedOpen] = useState(false);
  const [gitStatus, setGitStatus] = useState<GitStatusSnapshot | null>(null);
  const prefs = useSessionListViewStore((state) => state.prefs);

  // The delivery store owns the durable list of application-created
  // worktrees. Loading it here lets the sidebar identify a chat that is
  // attached to one of those branches without making the list depend on the
  // Git Delivery settings panel being open first.
  useEffect(() => {
    if (!isTauri()) return;
    void refreshDelivery().catch(() => undefined);
  }, [refreshDelivery]);

  // The lightweight status command is scoped to the current primary
  // workspace. It is enough for ordinary branch badges; the heavier review
  // snapshot is deferred until a user hovers or focuses the badge.
  useEffect(() => {
    let disposed = false;
    const load = async () => {
      if (!isTauri() || !primaryWorkspacePath) {
        setGitStatus(null);
        return;
      }
      try {
        const status = await invoke<GitStatusSnapshot>("git_status");
        if (!disposed) setGitStatus(status);
      } catch {
        if (!disposed) setGitStatus(EMPTY_GIT_STATUS);
      }
    };
    void load();
    window.addEventListener("focus", load);
    return () => {
      disposed = true;
      window.removeEventListener("focus", load);
    };
  }, [primaryWorkspacePath]);

  // The daemon owns the outside conversations, so this list is fetched rather
  // than subscribed to. Refreshed on mount and whenever the window comes back
  // to the front — the moment a user is most likely to be looking for what
  // arrived while they were elsewhere — instead of on a timer that would
  // spawn a CLI process every few seconds forever.
  useEffect(() => {
    void refreshExternal();
    const onFocus = () => void refreshExternal();
    window.addEventListener("focus", onFocus);
    return () => window.removeEventListener("focus", onFocus);
  }, [refreshExternal]);

  // Unstarted sessions (no messages yet — see `newSession`'s reset-in-place
  // logic) stay out of the sidebar entirely, same as Claude Desktop: a "New
  // session" only earns a row once the user actually sends something.
  const rows = useMemo(
    () => [
      ...sessions
        .filter((session) => session.messages.length > 0)
        .map((session) =>
          localRow(
            session,
            sessionStatus(
              session,
              runningTurns[session.id] === true,
              turnOutcomes[session.id],
              awaitingPermission.has(session.id),
            ),
          ),
        ),
      ...conversations.map(externalRow),
    ],
    // `awaitingPermission` is a fresh Set every render; the queue it derives
    // from is the real input.
    // eslint-disable-next-line react-hooks/exhaustive-deps
    [sessions, conversations, runningTurns, turnOutcomes, permissionQueue],
  );

  const environments = useMemo(() => environmentOptions(rows), [rows]);
  const { pinned, sections, archived: archivedRows, filtered } = buildSessionListView({
    rows,
    groups,
    prefs,
    // Only read when grouping by date, and re-read on every render the store
    // triggers — a session that ages past midnight moves buckets the next
    // time the list re-renders, which any session activity forces.
    now: Date.now(),
    labels: {
      recents: t("ChatSessionList.recentsHeading"),
      today: t("ChatSessionList.view.today"),
      yesterday: t("ChatSessionList.view.yesterday"),
      lastWeek: t("ChatSessionList.view.lastWeek"),
      older: t("ChatSessionList.view.older"),
      noFolder: t("ChatSessionList.view.noFolder"),
      idle: t("ChatSessionList.view.stateIdle"),
      state: {
        working: t("ChatSessionList.status.working"),
        attention: t("ChatSessionList.status.attention"),
        error: t("ChatSessionList.status.error"),
        finished: t("ChatSessionList.status.finished"),
      },
    },
  });

  const sessionGitContexts = useMemo(() => {
    const contexts = new Map<string, SessionGitContext>();
    for (const session of sessions) {
      if (!session.workspacePath) continue;
      const deliveryWorktree = deliveryWorktrees.find((item) =>
        pathsEqual(session.workspacePath, item.marker.canonicalPath),
      );
      const isCurrentWorkspace = pathsEqual(session.workspacePath, primaryWorkspacePath);
      const branch = deliveryWorktree?.marker.branch ?? (isCurrentWorkspace ? gitStatus?.branch : null);
      if (!branch) continue;

      const selectedDeliveryWorktree = deliveryWorktree &&
        deliveryWorktree.marker.worktreeId === deliverySelectedWorktreeId;
      contexts.set(session.id, {
        workspacePath: session.workspacePath,
        branch,
        worktreeName: deliveryWorktree?.marker.branch ?? (isCurrentWorkspace ? gitStatus?.worktree_name ?? null : null),
        repositorySlug: deliveryWorktree?.marker.repositorySlug ?? null,
        changedFiles: isCurrentWorkspace ? gitStatus?.changed_files ?? null : null,
        pullRequest: selectedDeliveryWorktree ? deliveryPullRequest : null,
        checks: selectedDeliveryWorktree ? deliveryChecks : null,
        canReview: isCurrentWorkspace,
      });
    }
    return contexts;
  }, [deliveryChecks, deliveryPullRequest, deliverySelectedWorktreeId, deliveryWorktrees, gitStatus, primaryWorkspacePath, sessions]);

  // Recent chats get the compact primary-modifier hints shown in the
  // reference UI. Pinned chats stay in their own section and do not consume
  // the recent-chat number range, so Cmd/Ctrl+2…9 remains stable as the list
  // is reordered by pinning.
  const shortcutTargets = useMemo(() => {
    const targets = new Map<number, string>();
    let key = 2;
    for (const row of [...sections.flatMap((section) => section.items), ...pinned]) {
      if (key > 9 || row.kind !== "local" || row.session.pinned) continue;
      targets.set(key, row.session.id);
      key += 1;
    }
    return targets;
  }, [pinned, sections]);

  const shortcutBySessionId = useMemo(() => {
    const labels = new Map<string, string>();
    for (const [key, sessionId] of shortcutTargets) {
      labels.set(sessionId, `${detectShortcutPlatform() === "macos" ? "⌘" : "Ctrl+"}${key}`);
    }
    return labels;
  }, [shortcutTargets]);

  useEffect(() => {
    const platform = detectShortcutPlatform();
    const onKeyDown = (event: KeyboardEvent) => {
      if (event.repeat || event.isComposing || event.defaultPrevented) return;
      const target = event.target as HTMLElement | null;
      if (target?.closest("input, textarea, select, [contenteditable=\"true\"]")) return;
      const primaryPressed = platform === "macos" ? event.metaKey : event.ctrlKey;
      if (!primaryPressed || event.altKey || event.shiftKey) return;
      const key = Number(event.key);
      if (!Number.isInteger(key) || !shortcutTargets.has(key)) return;
      const sessionId = shortcutTargets.get(key);
      if (!sessionId) return;
      event.preventDefault();
      selectExternal(null);
      switchSession(sessionId);
    };
    window.addEventListener("keydown", onKeyDown);
    return () => window.removeEventListener("keydown", onKeyDown);
  }, [selectExternal, shortcutTargets, switchSession]);

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

  const rowClass = (highlighted: boolean) =>
    `group relative flex min-w-0 cursor-pointer items-center gap-2 rounded-md px-2.5 py-1 text-sm focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-accent ${
      highlighted ? "bg-surface-2 text-foreground" : "hover:bg-surface-2"
    }`;

  const renderRow = (row: SessionRow) => {
    if (row.kind === "external") {
      const label = environmentLabel(row.environment);
      const isSelected =
        selectedExternal?.environment === row.conversation.environment &&
        selectedExternal.id === row.conversation.id;
      return (
        <div
          key={row.id}
          role="button"
          tabIndex={0}
          onClick={() =>
            selectExternal({ environment: row.conversation.environment, id: row.conversation.id })
          }
          onKeyDown={(event) => {
            if (event.key === "Enter" || event.key === " ") {
              event.preventDefault();
              selectExternal({ environment: row.conversation.environment, id: row.conversation.id });
            }
          }}
          className={rowClass(isSelected)}
        >
          <span className="truncate">
            <EnvironmentMarker environment={row.environment} label={label} />
            {row.title}
          </span>
        </div>
      );
    }

    const session = row.session;
    const isActive = session.id === activeSessionId && selectedExternal === null;
    const isRenaming = renamingId === session.id;
    const isMenuOpen = menuOpenId === session.id;
    const gitContext = sessionGitContexts.get(session.id) ?? null;
    const shortcutLabel = shortcutBySessionId.get(session.id) ?? null;
    const closeMenu = () => {
      setMenuOpenId(null);
      setMenuAnchor(null);
    };
    const open = () => {
      // Opening a local session takes the main pane back from whatever
      // outside conversation was being read.
      selectExternal(null);
      switchSession(session.id);
    };

    return (
      <div
        key={row.id}
        role="button"
        tabIndex={0}
        onClick={() => !isRenaming && open()}
        onKeyDown={(event) => {
          if (isRenaming) return;
          if (event.key === "Enter" || event.key === " ") {
            event.preventDefault();
            open();
          }
        }}
        className={rowClass(isActive || isMenuOpen)}
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
          <span className={`min-w-0 flex-1 truncate ${gitContext ? "pr-28" : "pr-20"} ${session.unread ? "font-semibold" : ""}`}>
            <StatusMarker
              status={row.status}
              label={row.status ? t(`ChatSessionList.status.${row.status}`) : t("ChatSessionList.view.stateIdle")}
            />
            {row.title}
          </span>
        )}

        {!isRenaming && (
          <div className={`absolute right-1 flex items-center gap-0.5 rounded-md pl-0.5 ${
            gitContext || session.pinned || isMenuOpen ? "bg-surface-2/95" : "group-hover:bg-surface-2/95 group-focus-within:bg-surface-2/95"
          }`}>
            {shortcutLabel && !gitContext && (
              <kbd className="mr-0.5 shrink-0 px-1 font-mono text-[11px] text-faint group-hover:hidden group-focus-within:hidden">
                {shortcutLabel}
              </kbd>
            )}
            {gitContext && <SessionGitBadge session={session} context={gitContext} />}
            <button
              type="button"
              onClick={(event) => {
                event.stopPropagation();
                togglePin(session.id);
                closeMenu();
              }}
              aria-label={session.pinned ? t("SessionMenu.unpin") : t("SessionMenu.pin")}
              aria-pressed={session.pinned}
              title={session.pinned ? t("SessionMenu.unpin") : t("SessionMenu.pin")}
              className={`inline-flex h-6 w-6 shrink-0 cursor-pointer items-center justify-center rounded text-faint transition-colors duration-150 hover:bg-surface hover:text-foreground focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-accent ${
                session.pinned || isMenuOpen ? "opacity-100" : "opacity-0 group-hover:opacity-100 group-focus-within:opacity-100"
              }`}
            >
              <Pin size={14} aria-hidden />
            </button>
            <button
              type="button"
              onClick={(event) => {
                event.stopPropagation();
                (session.archived ? unarchiveSession : archiveSession)(session.id);
                closeMenu();
              }}
              aria-label={session.archived ? t("SessionMenu.unarchive") : t("SessionMenu.archive")}
              title={session.archived ? t("SessionMenu.unarchive") : t("SessionMenu.archive")}
              className={`inline-flex h-6 w-6 shrink-0 cursor-pointer items-center justify-center rounded text-faint transition-colors duration-150 hover:bg-surface hover:text-foreground focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-accent ${
                isMenuOpen ? "opacity-100" : "opacity-0 group-hover:opacity-100 group-focus-within:opacity-100"
              }`}
            >
              {session.archived ? <ArchiveRestore size={14} aria-hidden /> : <Archive size={14} aria-hidden />}
            </button>
            <button
              type="button"
              onClick={(event) => {
                event.stopPropagation();
                if (isMenuOpen) {
                  closeMenu();
                } else {
                  setMenuAnchor(event.currentTarget.getBoundingClientRect());
                  setMenuOpenId(session.id);
                }
              }}
              aria-label={t("ChatSessionList.sessionMenuAriaLabel")}
              title={t("ChatSessionList.sessionMenuAriaLabel")}
              className={`inline-flex h-6 w-6 shrink-0 cursor-pointer items-center justify-center rounded text-faint transition-colors duration-150 hover:bg-surface hover:text-foreground focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-accent ${
                isMenuOpen ? "opacity-100" : "opacity-0 group-hover:opacity-100 group-focus-within:opacity-100"
              }`}
            >
              <MoreVertical size={14} aria-hidden />
            </button>
          </div>
        )}

        {isMenuOpen && menuAnchor && (
          <SessionMenu
            session={session}
            anchorRect={menuAnchor}
            onClose={closeMenu}
            onRename={() => startRename(session)}
          />
        )}
      </div>
    );
  };

  const viewMenuButton = (
    <button
      type="button"
      onClick={(event) => setViewMenuAnchor(event.currentTarget.getBoundingClientRect())}
      aria-label={t("ChatSessionList.view.menuAriaLabel")}
      aria-expanded={viewMenuAnchor !== null}
      className={`shrink-0 rounded p-0.5 hover:bg-surface-2 hover:text-foreground ${
        viewMenuAnchor || filtered ? "text-foreground" : "text-faint"
      }`}
    >
      <SlidersHorizontal size={13} />
    </button>
  );

  return (
    <div>
      <div className="p-2">
        <Button
          variant="secondary"
          size="md"
          onClick={() => {
            selectExternal(null);
            newSession();
          }}
          className="w-full justify-start"
        >
          <Plus size={16} className="shrink-0" />
          <span>{t("ChatSessionList.newSession")}</span>
        </Button>
      </div>

      {viewMenuAnchor && (
        <SessionListMenu
          anchorRect={viewMenuAnchor}
          environments={environments}
          onClose={() => setViewMenuAnchor(null)}
        />
      )}

      {pinned.length > 0 && (
        <>
          <SectionHeading title={t("ChatSessionList.pinnedHeading")} />
          <div className="flex flex-col px-2 pb-2">{pinned.map(renderRow)}</div>
        </>
      )}

      {/* The view menu hangs off the first section's heading — the same row
          it sits on in the desktop sidebar. With every section filtered away
          there is no heading to hang it from, so an empty "Recents" one
          carries it rather than stranding the user in a filtered list with
          no way back. */}
      {sections.length === 0 ? (
        <>
          <SectionHeading title={t("ChatSessionList.recentsHeading")} action={viewMenuButton} />
          {filtered && (
            <p className="px-3 pb-2 text-xs text-faint">{t("ChatSessionList.view.noMatches")}</p>
          )}
        </>
      ) : (
        sections.map((section, index) => (
          <div key={section.id}>
            <SectionHeading title={section.title} action={index === 0 ? viewMenuButton : undefined} />
            {section.items.length > 0 ? (
              <div className="flex flex-col px-2 pb-2">{section.items.map(renderRow)}</div>
            ) : (
              filtered && <p className="px-3 pb-2 text-xs text-faint">{t("ChatSessionList.view.noMatches")}</p>
            )}
          </div>
        ))
      )}

      {archivedRows.length > 0 && (
        <div className="px-2 pb-2">
          <button
            type="button"
            onClick={() => setArchivedOpen((prev) => !prev)}
            className="w-full px-1 pb-1 pt-2 text-left text-[11px] font-semibold uppercase tracking-wider text-faint hover:text-muted"
          >
            {t("ChatSessionList.archivedHeading", { count: archivedRows.length })}
          </button>
          {archivedOpen && <div className="flex flex-col">{archivedRows.map(renderRow)}</div>}
        </div>
      )}
    </div>
  );
}
