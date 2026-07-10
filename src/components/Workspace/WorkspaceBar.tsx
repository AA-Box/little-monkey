import { useCallback, useEffect, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { open } from "@tauri-apps/plugin-dialog";
import { FolderOpen, FolderPlus, GitBranch, GitFork, RefreshCw, SquareTerminal } from "lucide-react";

import { Button, ContextMenu, IconButton } from "../ui";
import { AttachmentChip } from "../Chat";
import { selectSessionMessages, useSessionStore } from "../../store/sessionStore";
import { primaryRoot, useWorkspaceStore } from "../../store/workspaceStore";
import { useT } from "../../lib/i18n";

/** Mirrors `GitStatusPayload` returned by the Rust `git_status` command (see
 * src-tauri/src/git.rs). Field names are left as-is (snake_case) since the
 * Rust struct has no serde rename attribute. */
interface GitStatusPayload {
  is_repo: boolean;
  branch: string | null;
  added: number;
  deleted: number;
  changed_files: number;
  is_worktree: boolean;
  worktree_name: string | null;
}

const NOT_A_REPO: GitStatusPayload = {
  is_repo: false,
  branch: null,
  added: 0,
  deleted: 0,
  changed_files: 0,
  is_worktree: false,
  worktree_name: null,
};

function formatError(err: unknown): string {
  if (err instanceof Error) return err.message;
  if (typeof err === "string") return err;
  try {
    return JSON.stringify(err);
  } catch {
    return String(err);
  }
}

/** Floor on the spin animation so a manual click always reads as "doing
 * something" even when the underlying `git_status` call resolves near-
 * instantly (typical on a local repo). */
const MIN_SPIN_MS = 500;
/** How long the transient status message (refresh result, copy
 * confirmation, folder-action error) stays visible before fading back out. */
const ACTION_MESSAGE_MS = 2000;

/**
 * Bar rendered directly above the chat input: the attached workspace
 * folders (a primary one plus any number of secondary ones), the primary's
 * git branch/worktree status, and a "Commit" popover.
 *
 * The primary chip's dropdown is the entry point for opening a folder (via
 * "Recent" or "Open folder…") — replacing the old sidebar button — and the
 * trailing icon button attaches additional folders the agent can address by
 * prefixing tool paths with their label (see src-tauri/src/workspace.rs).
 *
 * Once the active chat session has messages, switching the primary folder
 * and attaching/removing secondary folders all lock; the read-only utility
 * actions (Show in Finder / Copy path / Open in terminal / copy branch or
 * worktree name) stay available since they don't mutate anything.
 */
export function WorkspaceBar({ sessionId }: { sessionId: string }) {
  const { t } = useT();
  const roots = useWorkspaceStore((s) => s.roots);
  const recent = useWorkspaceStore((s) => s.recent);
  const rootsVersion = useWorkspaceStore((s) => s.rootsVersion);
  const openPrimary = useWorkspaceStore((s) => s.openPrimary);
  const addSecondary = useWorkspaceStore((s) => s.addSecondary);
  const removeSecondary = useWorkspaceStore((s) => s.removeSecondary);
  const locked = useSessionStore((s) => selectSessionMessages(sessionId)(s).length > 0);

  const primary = primaryRoot(roots);
  const secondaries = roots.filter((r) => !r.is_primary);

  const [status, setStatus] = useState<GitStatusPayload | null>(null);
  const [refreshing, setRefreshing] = useState(false);
  const [actionMessage, setActionMessage] = useState<{ kind: "success" | "error"; text: string } | null>(
    null,
  );
  const actionMessageTimer = useRef<ReturnType<typeof setTimeout> | null>(null);

  const [primaryMenuOpen, setPrimaryMenuOpen] = useState(false);
  const [branchMenuOpen, setBranchMenuOpen] = useState(false);
  const [worktreeMenuOpen, setWorktreeMenuOpen] = useState(false);
  const primaryMenuRef = useRef<HTMLDivElement>(null);
  const branchMenuRef = useRef<HTMLDivElement>(null);
  const worktreeMenuRef = useRef<HTMLDivElement>(null);

  const [commitOpen, setCommitOpen] = useState(false);
  const [message, setMessage] = useState("");
  const [committing, setCommitting] = useState(false);
  const [commitError, setCommitError] = useState<string | null>(null);
  const popoverRef = useRef<HTMLDivElement>(null);

  const showActionMessage = useCallback((kind: "success" | "error", text: string) => {
    if (actionMessageTimer.current) clearTimeout(actionMessageTimer.current);
    setActionMessage({ kind, text });
    actionMessageTimer.current = setTimeout(() => setActionMessage(null), ACTION_MESSAGE_MS);
  }, []);

  const fetchStatus = useCallback(async () => {
    if (!primary) {
      setStatus(null);
      return { ok: true as const };
    }
    setRefreshing(true);
    const started = Date.now();
    try {
      const result = await invoke<GitStatusPayload>("git_status");
      setStatus(result);
      return { ok: true as const };
    } catch (err) {
      // No git binary, primary root vanished, etc. — treat as "not a repo"
      // rather than surfacing an error.
      setStatus(NOT_A_REPO);
      return { ok: false as const, error: formatError(err) };
    } finally {
      const elapsed = Date.now() - started;
      if (elapsed < MIN_SPIN_MS) {
        await new Promise((resolve) => setTimeout(resolve, MIN_SPIN_MS - elapsed));
      }
      setRefreshing(false);
    }
  }, [primary]);

  const handleManualRefresh = useCallback(async () => {
    const result = await fetchStatus();
    showActionMessage(result.ok ? "success" : "error", result.ok ? t("WorkspaceBar.refreshedMessage") : result.error);
  }, [fetchStatus, showActionMessage, t]);

  const handleShowInFinder = useCallback(async () => {
    setPrimaryMenuOpen(false);
    if (!primary) return;
    try {
      await invoke("reveal_in_finder", { path: primary.path });
    } catch (err) {
      showActionMessage("error", formatError(err));
    }
  }, [primary, showActionMessage]);

  const handleCopyPath = useCallback(async () => {
    setPrimaryMenuOpen(false);
    if (!primary) return;
    try {
      await navigator.clipboard.writeText(primary.path);
      showActionMessage("success", t("WorkspaceBar.pathCopied"));
    } catch {
      showActionMessage("error", t("WorkspaceBar.failedToCopyPath"));
    }
  }, [primary, showActionMessage, t]);

  const handleOpenInTerminal = useCallback(
    async (path: string) => {
      setPrimaryMenuOpen(false);
      setBranchMenuOpen(false);
      setWorktreeMenuOpen(false);
      try {
        await invoke("open_in_terminal", { path });
      } catch (err) {
        showActionMessage("error", formatError(err));
      }
    },
    [showActionMessage],
  );

  const handleCopyBranchName = useCallback(
    async (branch: string | null) => {
      setBranchMenuOpen(false);
      if (!branch) return;
      try {
        await navigator.clipboard.writeText(branch);
        showActionMessage("success", t("WorkspaceBar.branchNameCopied"));
      } catch {
        showActionMessage("error", t("WorkspaceBar.failedToCopyBranchName"));
      }
    },
    [showActionMessage, t],
  );

  const handleCopyWorktreeName = useCallback(
    async (name: string | null) => {
      setWorktreeMenuOpen(false);
      if (!name) return;
      try {
        await navigator.clipboard.writeText(name);
        showActionMessage("success", t("WorkspaceBar.worktreeNameCopied"));
      } catch {
        showActionMessage("error", t("WorkspaceBar.failedToCopyWorktreeName"));
      }
    },
    [showActionMessage, t],
  );

  const handleOpenFolderDialog = useCallback(async () => {
    setPrimaryMenuOpen(false);
    try {
      const selected = await open({ directory: true, multiple: false });
      if (!selected || Array.isArray(selected)) return;
      await openPrimary(selected);
    } catch (err) {
      showActionMessage("error", formatError(err));
    }
  }, [openPrimary, showActionMessage]);

  const handleRecentSelect = useCallback(
    async (path: string) => {
      setPrimaryMenuOpen(false);
      try {
        await openPrimary(path);
      } catch (err) {
        showActionMessage("error", formatError(err));
      }
    },
    [openPrimary, showActionMessage],
  );

  const handleAddFolderDialog = useCallback(async () => {
    try {
      const selected = await open({ directory: true, multiple: false });
      if (!selected || Array.isArray(selected)) return;
      await addSecondary(selected);
    } catch (err) {
      showActionMessage("error", formatError(err));
    }
  }, [addSecondary, showActionMessage]);

  useEffect(() => {
    void fetchStatus();
    // Re-fetch whenever the primary root changes, or on mount.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [rootsVersion, primary?.id]);

  useEffect(() => {
    return () => {
      if (actionMessageTimer.current) clearTimeout(actionMessageTimer.current);
    };
  }, []);

  // Close the commit popover and folder/branch/worktree menus on outside
  // clicks, so they behave like the other floating panels in the app.
  useEffect(() => {
    if (!commitOpen && !primaryMenuOpen && !branchMenuOpen && !worktreeMenuOpen) return;
    function handlePointerDown(event: PointerEvent) {
      const target = event.target as Node;
      if (commitOpen && popoverRef.current && !popoverRef.current.contains(target)) {
        setCommitOpen(false);
      }
      if (primaryMenuOpen && primaryMenuRef.current && !primaryMenuRef.current.contains(target)) {
        setPrimaryMenuOpen(false);
      }
      if (branchMenuOpen && branchMenuRef.current && !branchMenuRef.current.contains(target)) {
        setBranchMenuOpen(false);
      }
      if (worktreeMenuOpen && worktreeMenuRef.current && !worktreeMenuRef.current.contains(target)) {
        setWorktreeMenuOpen(false);
      }
    }
    document.addEventListener("pointerdown", handlePointerDown);
    return () => document.removeEventListener("pointerdown", handlePointerDown);
  }, [commitOpen, primaryMenuOpen, branchMenuOpen, worktreeMenuOpen]);

  const handleCommit = useCallback(async () => {
    if (!message.trim() || committing) return;
    setCommitting(true);
    setCommitError(null);
    try {
      await invoke<string>("git_commit", { message });
      setCommitOpen(false);
      setMessage("");
      await fetchStatus();
    } catch (err) {
      setCommitError(formatError(err));
    } finally {
      setCommitting(false);
    }
  }, [message, committing, fetchStatus]);

  const effectiveStatus = status ?? NOT_A_REPO;
  const hasChanges = effectiveStatus.added > 0 || effectiveStatus.deleted > 0;
  // Only genuinely dead once locked with nothing to fall back on — a locked
  // bar with a primary open still needs the trigger clickable so the
  // read-only utility actions (Finder/copy/terminal) stay reachable.
  const primaryMenuDisabled = locked && !primary;

  return (
    <div className="mx-auto mb-2 flex max-w-3xl flex-wrap items-center gap-1.5 px-1 text-xs">
      <div className="relative" ref={primaryMenuRef}>
        <button
          type="button"
          disabled={primaryMenuDisabled}
          onClick={() => {
            setBranchMenuOpen(false);
            setWorktreeMenuOpen(false);
            setPrimaryMenuOpen((prev) => !prev);
          }}
          title={locked ? t("WorkspaceBar.workspaceLockedTitle") : undefined}
          className={
            hasChanges
              ? "cursor-pointer truncate rounded-md px-1 py-0.5 font-mono font-semibold text-foreground hover:bg-surface-2 disabled:cursor-not-allowed disabled:opacity-50"
              : "inline-flex cursor-pointer items-center gap-1.5 truncate rounded-md border border-border bg-surface px-2 py-1 font-mono font-medium text-foreground hover:bg-surface-2 disabled:cursor-not-allowed disabled:opacity-50"
          }
        >
          {!hasChanges && <FolderOpen size={12} className="shrink-0 text-faint" />}
          {primary ? primary.label : t("WorkspaceBar.openLabel")}
        </button>
        {primaryMenuOpen && (
          <div className="absolute bottom-full left-0 z-30 mb-1 w-64 rounded-lg border border-border bg-background py-1 shadow-lg">
            {!locked && (
              <>
                <div className="px-3 pb-1 pt-2 text-[11px] font-semibold uppercase tracking-wider text-faint">
                  {t("WorkspaceBar.recentHeading")}
                </div>
                {recent.length === 0 ? (
                  <p className="px-3 py-1.5 text-xs text-faint">{t("WorkspaceBar.noRecentWorkspaces")}</p>
                ) : (
                  <div className="max-h-40 overflow-y-auto">
                    {recent.map((entry) => (
                      <button
                        key={entry.path}
                        type="button"
                        onClick={() => void handleRecentSelect(entry.path)}
                        className="flex w-full cursor-pointer flex-col items-start px-3 py-1.5 text-left hover:bg-surface-2"
                      >
                        <span className="w-full truncate text-sm text-foreground">{entry.label}</span>
                        <span className="w-full truncate font-mono text-[11px] text-faint">{entry.path}</span>
                      </button>
                    ))}
                  </div>
                )}
                <div className="my-1 border-t border-border" />
                <button
                  type="button"
                  onClick={() => void handleOpenFolderDialog()}
                  className="flex w-full cursor-pointer items-center gap-2 px-3 py-1.5 text-left text-sm text-foreground hover:bg-surface-2"
                >
                  <FolderOpen size={14} className="text-faint" />
                  {t("WorkspaceBar.openFolderEllipsis")}
                </button>
              </>
            )}
            {primary && (
              <>
                {!locked && <div className="my-1 border-t border-border" />}
                <button
                  type="button"
                  onClick={() => void handleShowInFinder()}
                  className="flex w-full cursor-pointer items-center gap-2 px-3 py-1.5 text-left text-sm text-foreground hover:bg-surface-2"
                >
                  {t("WorkspaceBar.showInFinder")}
                </button>
                <button
                  type="button"
                  onClick={() => void handleCopyPath()}
                  className="flex w-full cursor-pointer items-center gap-2 px-3 py-1.5 text-left text-sm text-foreground hover:bg-surface-2"
                >
                  {t("WorkspaceBar.copyPath")}
                </button>
                <button
                  type="button"
                  onClick={() => void handleOpenInTerminal(primary.path)}
                  className="flex w-full cursor-pointer items-center gap-2 px-3 py-1.5 text-left text-sm text-foreground hover:bg-surface-2"
                >
                  <SquareTerminal size={14} className="text-faint" />
                  {t("WorkspaceBar.openInTerminal")}
                </button>
              </>
            )}
          </div>
        )}
      </div>

      {primary && effectiveStatus.is_repo && (
        <>
          {hasChanges ? (
            <div className="relative" ref={branchMenuRef}>
              <button
                type="button"
                onClick={() => {
                  setPrimaryMenuOpen(false);
                  setWorktreeMenuOpen(false);
                  setBranchMenuOpen((prev) => !prev);
                }}
                className="cursor-pointer truncate rounded-md px-1 py-0.5 font-mono text-muted hover:bg-surface-2"
              >
                {effectiveStatus.branch}
              </button>
              {branchMenuOpen && (
                <ContextMenu
                  className="left-0 top-full mt-1"
                  entries={[
                    {
                      label: t("WorkspaceBar.copyBranchName"),
                      onClick: () => void handleCopyBranchName(effectiveStatus.branch),
                    },
                    { separator: true },
                    {
                      label: t("WorkspaceBar.openInTerminal"),
                      icon: <SquareTerminal size={14} className="text-faint" />,
                      onClick: () => void handleOpenInTerminal(primary.path),
                    },
                  ]}
                />
              )}
            </div>
          ) : (
            <div className="inline-flex shrink-0 items-center gap-1.5 rounded-md border border-border bg-surface px-2 py-1 font-mono text-muted">
              <GitBranch size={12} className="shrink-0 text-faint" />
              <div className="relative" ref={branchMenuRef}>
                <button
                  type="button"
                  onClick={() => {
                    setPrimaryMenuOpen(false);
                    setWorktreeMenuOpen(false);
                    setBranchMenuOpen((prev) => !prev);
                  }}
                  className="cursor-pointer truncate hover:text-foreground"
                >
                  {effectiveStatus.branch}
                </button>
                {branchMenuOpen && (
                  <ContextMenu
                    className="left-0 top-full mt-1"
                    entries={[
                      {
                        label: t("WorkspaceBar.copyBranchName"),
                        onClick: () => void handleCopyBranchName(effectiveStatus.branch),
                      },
                      { separator: true },
                      {
                        label: t("WorkspaceBar.openInTerminal"),
                        icon: <SquareTerminal size={14} className="text-faint" />,
                        onClick: () => void handleOpenInTerminal(primary.path),
                      },
                    ]}
                  />
                )}
              </div>

              <span className="text-border-strong">|</span>
              <GitFork size={12} className="shrink-0 text-faint" />
              {effectiveStatus.is_worktree ? (
                <div className="relative" ref={worktreeMenuRef}>
                  <button
                    type="button"
                    onClick={() => {
                      setPrimaryMenuOpen(false);
                      setBranchMenuOpen(false);
                      setWorktreeMenuOpen((prev) => !prev);
                    }}
                    className="cursor-pointer truncate hover:text-foreground"
                  >
                    {effectiveStatus.worktree_name}
                  </button>
                  {worktreeMenuOpen && (
                    <ContextMenu
                      className="left-0 top-full mt-1"
                      entries={[
                        {
                          label: t("WorkspaceBar.copyWorktreeName"),
                          onClick: () => void handleCopyWorktreeName(effectiveStatus.worktree_name),
                        },
                        { separator: true },
                        {
                          label: t("WorkspaceBar.openInTerminal"),
                          icon: <SquareTerminal size={14} className="text-faint" />,
                          onClick: () => void handleOpenInTerminal(primary.path),
                        },
                      ]}
                    />
                  )}
                </div>
              ) : (
                <span className="truncate text-faint">{t("WorkspaceBar.worktreeFallback")}</span>
              )}
            </div>
          )}

          <IconButton
            size="sm"
            variant="ghost"
            onClick={() => void handleManualRefresh()}
            disabled={refreshing}
            aria-label={t("WorkspaceBar.refreshGitStatusAriaLabel")}
          >
            <RefreshCw size={14} className={refreshing ? "animate-spin" : undefined} />
          </IconButton>
        </>
      )}

      {secondaries.map((root) => (
        <AttachmentChip
          key={root.id}
          name={root.label}
          isDir
          removable={!locked}
          onRemove={() => void removeSecondary(root.id)}
        />
      ))}

      {primary && (
        <IconButton
          size="sm"
          variant="ghost"
          onClick={() => void handleAddFolderDialog()}
          disabled={locked}
          title={t("WorkspaceBar.addAnotherFolder")}
          aria-label={t("WorkspaceBar.addAnotherFolder")}
        >
          <FolderPlus size={14} />
        </IconButton>
      )}

      {actionMessage && (
        <span
          className={`animate-fade-in truncate font-mono text-xs ${
            actionMessage.kind === "success" ? "text-success" : "text-danger"
          }`}
        >
          {actionMessage.text}
        </span>
      )}

      {primary && effectiveStatus.is_repo && hasChanges && (
        <div className="ml-auto flex items-center gap-2">
          <span className="inline-flex items-center gap-1.5 rounded-full bg-surface-2 px-2.5 py-1 font-mono text-xs">
            <span className="text-success">+{effectiveStatus.added}</span>
            <span className="text-danger">-{effectiveStatus.deleted}</span>
          </span>

          <div className="relative" ref={popoverRef}>
            <Button
              variant="secondary"
              size="sm"
              onClick={() => {
                setCommitError(null);
                setCommitOpen((prev) => !prev);
              }}
            >
              {t("WorkspaceBar.commitChanges")}
            </Button>

            {commitOpen && (
            // Same floating-panel idiom as MentionAutocomplete (absolute,
            // rounded-lg border border-border bg-background shadow-lg z-20),
            // anchored right/below since the Commit button sits at the
            // right edge of the bar.
            <div className="absolute right-0 top-full z-20 mt-1 w-64 rounded-lg border border-border bg-background p-2 shadow-lg">
              <textarea
                autoFocus
                value={message}
                onChange={(e) => setMessage(e.target.value)}
                placeholder={t("WorkspaceBar.commitMessagePlaceholder")}
                rows={3}
                className="w-full resize-none rounded-md border border-border bg-surface-2 p-1.5 font-mono text-xs text-foreground outline-none placeholder:text-faint focus-visible:border-accent"
              />
              {commitError && <p className="mt-1.5 text-xs text-danger">{commitError}</p>}
              <div className="mt-2 flex justify-end">
                <Button
                  variant="primary"
                  size="sm"
                  onClick={() => void handleCommit()}
                  disabled={!message.trim() || committing}
                >
                  {committing ? t("WorkspaceBar.committingEllipsis") : t("WorkspaceBar.commitButton")}
                </Button>
              </div>
            </div>
            )}
          </div>
        </div>
      )}
    </div>
  );
}

export default WorkspaceBar;
