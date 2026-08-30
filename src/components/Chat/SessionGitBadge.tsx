import { useEffect, useRef, useState } from "react";
import { createPortal } from "react-dom";
import { invoke, isTauri } from "@tauri-apps/api/core";
import {
  CheckCircle2,
  CircleDot,
  Folder,
  GitBranch,
  GitPullRequest,
  LoaderCircle,
} from "lucide-react";

import { useT } from "../../lib/i18n";
import type { ChatSession } from "../../store/sessionStore";

export interface SessionGitContext {
  workspacePath: string;
  branch: string;
  worktreeName: string | null;
  repositorySlug: string | null;
  changedFiles: number | null;
  pullRequest: Record<string, unknown> | null;
  checks: Record<string, unknown> | null;
  /** Review reads are only valid for the workspace currently open in the app. */
  canReview: boolean;
}

interface GitReviewSnapshot {
  branch: string | null;
  target: string | null;
  totalAdded: number;
  totalDeleted: number;
  files: unknown[];
  prUrl: string | null;
}

type CheckState = "success" | "pending" | "failure";

const POPOVER_WIDTH = 320;
const POPOVER_HEIGHT = 160;

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null;
}

function stringValue(value: unknown, ...keys: string[]): string | null {
  if (!isRecord(value)) return null;
  for (const key of keys) {
    const candidate = value[key];
    if (typeof candidate === "string" && candidate.trim()) return candidate.trim();
  }
  return null;
}

function numberValue(value: unknown, ...keys: string[]): number | null {
  if (!isRecord(value)) return null;
  for (const key of keys) {
    const candidate = value[key];
    if (typeof candidate === "number" && Number.isFinite(candidate)) return candidate;
  }
  return null;
}

function workspaceLabel(path: string): string {
  return path.split(/[\\/]/).filter(Boolean).pop() ?? path;
}

function relativeTimeLabel(timestamp: number): string {
  const elapsed = Math.max(0, Date.now() - timestamp);
  const minutes = Math.floor(elapsed / 60_000);
  if (minutes < 1) return "now";
  if (minutes < 60) return `${minutes}m`;
  const hours = Math.floor(minutes / 60);
  if (hours < 24) return `${hours}h`;
  const days = Math.floor(hours / 24);
  if (days < 7) return `${days}d`;
  return `${Math.floor(days / 7)}w`;
}

export function SessionPreviewCard({
  session,
  workspacePath,
  anchorRect,
  onPointerEnter,
  onPointerLeave,
}: {
  session: ChatSession;
  workspacePath: string;
  anchorRect: DOMRect;
  onPointerEnter: () => void;
  onPointerLeave: () => void;
}) {
  const top = Math.min(Math.max(anchorRect.top, 8), Math.max(8, window.innerHeight - 72));
  const left = Math.min(
    anchorRect.right + 8,
    Math.max(8, window.innerWidth - 320 - 8),
  );

  return createPortal(
    <div
      role="tooltip"
      style={{ position: "fixed", top, left, width: 320 }}
      className="z-40 rounded-xl border border-border bg-background px-2.5 py-2 shadow-xl"
      onPointerEnter={onPointerEnter}
      onPointerLeave={onPointerLeave}
    >
      <div className="flex min-w-0 items-center justify-between gap-2">
        <p className="min-w-0 truncate text-xs font-medium text-foreground" title={session.title}>
          {session.title}
        </p>
        <time
          className="shrink-0 text-[10px] text-faint"
          dateTime={new Date(session.updatedAt).toISOString()}
          title={new Date(session.updatedAt).toLocaleString()}
        >
          {relativeTimeLabel(session.updatedAt)}
        </time>
      </div>
      <div className="mt-1.5 flex min-w-0 items-center gap-2 text-xs text-muted">
        <Folder size={14} className="shrink-0 text-faint" aria-hidden />
        <span className="truncate" title={workspacePath}>{workspaceLabel(workspacePath)}</span>
      </div>
    </div>,
    document.body,
  );
}

function summarizePullRequest(value: Record<string, unknown> | null) {
  if (!value) return null;
  return {
    number: numberValue(value, "number"),
    title: stringValue(value, "title", "name"),
    url: stringValue(value, "html_url", "url", "web_url"),
    state: stringValue(value, "state"),
    head: isRecord(value.head) ? stringValue(value.head, "ref") : null,
    base: isRecord(value.base) ? stringValue(value.base, "ref") : null,
    draft: value.draft === true || value.isDraft === true,
  };
}

function summarizeChecks(value: Record<string, unknown> | null): CheckState | null {
  if (!value) return null;
  const runs = Array.isArray(value.check_runs)
    ? value.check_runs
    : Array.isArray(value.checks)
      ? value.checks
      : [];
  const conclusions = runs
    .filter(isRecord)
    .map((run) => stringValue(run, "conclusion", "status")?.toLowerCase())
    .filter((state): state is string => Boolean(state));
  if (conclusions.length === 0) {
    const state = stringValue(value, "state", "status")?.toLowerCase();
    if (!state) return null;
    if (["success", "successful", "passed", "pass"].includes(state)) return "success";
    if (["failure", "failed", "error", "cancelled"].includes(state)) return "failure";
    return "pending";
  }
  if (conclusions.every((state) => ["success", "successful", "passed", "pass"].includes(state))) {
    return "success";
  }
  if (conclusions.some((state) => ["failure", "failed", "error", "cancelled"].includes(state))) {
    return "failure";
  }
  return "pending";
}

function GitContextPopover({
  session,
  context,
  anchorRect,
  review,
  loading,
  onPointerEnter,
  onPointerLeave,
}: {
  session: ChatSession;
  context: SessionGitContext;
  anchorRect: DOMRect;
  review: GitReviewSnapshot | null;
  loading: boolean;
  onPointerEnter: () => void;
  onPointerLeave: () => void;
}) {
  const { t } = useT();
  const pullRequest = summarizePullRequest(context.pullRequest);
  const checkState = summarizeChecks(context.checks);
  const reviewChangedFiles = review?.files.length ?? null;
  const showChecks = checkState !== null || reviewChangedFiles !== null || context.changedFiles !== null;
  const hasChanges = reviewChangedFiles !== null
    ? reviewChangedFiles > 0
    : context.changedFiles !== null && context.changedFiles > 0;
  const statusState = checkState ?? (hasChanges ? "pending" : "success");
  const statusLabel = checkState === "success"
    ? t("ChatSessionList.gitChecksSuccessful")
    : checkState === "failure"
      ? t("ChatSessionList.gitChecksFailed")
      : reviewChangedFiles !== null || context.changedFiles !== null
        ? hasChanges
          ? t("ChatSessionList.gitFilesChanged", { count: reviewChangedFiles ?? context.changedFiles ?? 0 })
          : t("ChatSessionList.gitWorkingTreeClean")
        : t("ChatSessionList.gitChecksNotLoaded");
  const prUrl = pullRequest?.url ?? review?.prUrl;
  const prLabel = pullRequest
    ? `${pullRequest.number ? `#${pullRequest.number} ` : ""}${pullRequest.title ?? t("ChatSessionList.gitPullRequest")}`
    : review?.prUrl
      ? t("ChatSessionList.gitCompareBranch")
      : null;
  const top = Math.min(
    Math.max(anchorRect.top - 4, 8),
    Math.max(8, window.innerHeight - POPOVER_HEIGHT - 8),
  );
  const left = Math.min(
    anchorRect.right + 8,
    Math.max(8, window.innerWidth - POPOVER_WIDTH - 8),
  );

  return createPortal(
    <div
      role="dialog"
      aria-label={t("ChatSessionList.gitContextTitle")}
      style={{ position: "fixed", top, left, width: POPOVER_WIDTH }}
      className="z-40 rounded-xl border border-border bg-background p-2.5 shadow-xl"
      onPointerEnter={onPointerEnter}
      onPointerLeave={onPointerLeave}
    >
      <div className="flex items-center justify-between gap-2 border-b border-border px-0.5 pb-2">
        <p className="min-w-0 truncate text-xs font-medium text-foreground" title={session.title}>
          {session.title}
        </p>
        <time
          className="shrink-0 text-[10px] text-faint"
          dateTime={new Date(session.updatedAt).toISOString()}
          title={new Date(session.updatedAt).toLocaleString()}
        >
          {relativeTimeLabel(session.updatedAt)}
        </time>
      </div>

      <div className="mt-2 space-y-2 text-xs">
        <div className="flex min-w-0 items-center gap-2 text-muted">
          <Folder size={14} className="shrink-0 text-faint" aria-hidden />
          <span className="truncate" title={context.workspacePath}>{workspaceLabel(context.workspacePath)}</span>
        </div>
        <div className="flex min-w-0 items-center gap-2 text-foreground">
          <GitBranch size={14} className="shrink-0 text-faint" aria-hidden />
          <span className="truncate font-mono text-[11px]" title={context.branch}>{context.branch}</span>
        </div>
        {prLabel && (prUrl ? (
          <a
            href={prUrl}
            target="_blank"
            rel="noreferrer"
            className="flex min-w-0 items-center gap-2 text-foreground hover:text-accent"
            onClick={(event) => event.stopPropagation()}
          >
            <GitPullRequest size={14} className="shrink-0 text-faint" aria-hidden />
            <span className="truncate" title={prLabel}>{prLabel}</span>
          </a>
        ) : (
          <div className="flex min-w-0 items-center gap-2 text-foreground">
            <GitPullRequest size={14} className="shrink-0 text-faint" aria-hidden />
            <span className="truncate" title={prLabel}>{prLabel}</span>
          </div>
        ))}
        {showChecks && <div className="flex items-center gap-2 border-t border-border pt-2">
          {loading ? (
            <LoaderCircle size={14} className="shrink-0 animate-spin text-faint motion-reduce:animate-none" aria-hidden />
          ) : statusState === "success" ? (
            <CheckCircle2 size={14} className="shrink-0 text-success" aria-hidden />
          ) : (
            <CircleDot size={14} className={`shrink-0 ${statusState === "failure" ? "text-danger" : "text-warning"}`} aria-hidden />
          )}
          <span className="text-muted">{loading ? t("ChatSessionList.gitLoading") : statusLabel}</span>
        </div>}
      </div>
    </div>,
    document.body,
  );
}

export function SessionGitBadge({
  session,
  context,
  rowHovered = false,
  rowHoverReady = false,
  closeImmediately = false,
  rowAnchorRect = null,
}: {
  session: ChatSession;
  context: SessionGitContext | null;
  rowHovered?: boolean;
  rowHoverReady?: boolean;
  closeImmediately?: boolean;
  rowAnchorRect?: DOMRect | null;
}) {
  const { t } = useT();
  const [open, setOpen] = useState(false);
  const [anchorRect, setAnchorRect] = useState<DOMRect | null>(null);
  const [review, setReview] = useState<GitReviewSnapshot | null>(null);
  const [loading, setLoading] = useState(false);
  const closeTimer = useRef<ReturnType<typeof setTimeout> | null>(null);

  const clearCloseTimer = () => {
    if (closeTimer.current) clearTimeout(closeTimer.current);
    closeTimer.current = null;
  };

  const scheduleClose = () => {
    clearCloseTimer();
    closeTimer.current = setTimeout(() => setOpen(false), 120);
  };

  const loadReview = async () => {
    if (!context || !context.canReview || !isTauri() || review || loading) return;
    setLoading(true);
    try {
      const payload = await invoke<{
        branch: string | null;
        target: string | null;
        total_added: number;
        total_deleted: number;
        files: unknown[];
        pr_url: string | null;
      }>("git_review", { mode: "branch" });
      setReview({
        branch: payload.branch,
        target: payload.target,
        totalAdded: payload.total_added,
        totalDeleted: payload.total_deleted,
        files: payload.files,
        prUrl: payload.pr_url,
      });
    } catch {
      // The branch chip is still useful when the optional review read is
      // unavailable (for example while the workspace is being closed).
    } finally {
      setLoading(false);
    }
  };

  useEffect(() => {
    if (!rowHovered) {
      if (closeImmediately) {
        clearCloseTimer();
        setOpen(false);
      } else {
        scheduleClose();
      }
      return;
    }
    if (!rowHoverReady || !rowAnchorRect) {
      clearCloseTimer();
      setOpen(false);
      return;
    }
    clearCloseTimer();
    setAnchorRect(rowAnchorRect);
    setOpen(true);
    void loadReview();
  }, [closeImmediately, rowAnchorRect, rowHoverReady, rowHovered]);

  if (!context) return null;

  const showPopover = (target: HTMLElement) => {
    clearCloseTimer();
    setAnchorRect(target.getBoundingClientRect());
    setOpen(true);
    void loadReview();
  };

  return (
    <>
      <button
        type="button"
        aria-label={t("ChatSessionList.gitBranchAriaLabel", { branch: context.branch })}
        aria-expanded={open}
        title={context.branch}
        className="relative inline-flex h-6 w-6 shrink-0 cursor-pointer items-center justify-center rounded text-faint transition-colors duration-150 hover:bg-surface hover:text-foreground focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-accent"
        onPointerEnter={(event) => showPopover(event.currentTarget)}
        onFocus={(event) => showPopover(event.currentTarget)}
        onPointerLeave={() => {
          if (!rowHovered) scheduleClose();
        }}
        onClick={(event) => {
          event.stopPropagation();
          setOpen((visible) => !visible);
        }}
        onKeyDown={(event) => event.stopPropagation()}
      >
        {context.pullRequest ? <GitPullRequest size={14} aria-hidden /> : <GitBranch size={14} aria-hidden />}
        {summarizeChecks(context.checks) === "success" && (
          <span className="absolute bottom-0.5 right-0.5 h-1.5 w-1.5 rounded-full bg-success" aria-hidden />
        )}
      </button>
      {open && anchorRect && (
        <GitContextPopover
          session={session}
          context={context}
          anchorRect={anchorRect}
          review={review}
          loading={loading}
          onPointerEnter={clearCloseTimer}
          onPointerLeave={scheduleClose}
        />
      )}
    </>
  );
}

export default SessionGitBadge;
