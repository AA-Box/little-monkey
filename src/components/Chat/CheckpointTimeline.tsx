import { useEffect, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { Ban, FilePenLine, History, MessageSquareX, RefreshCw, TerminalSquare, Undo2 } from "lucide-react";

import { useT } from "../../lib/i18n";
import {
  checkpointAnchorValid,
  checkpointChainBlockReason,
  formatCheckpointNotice,
  isCheckpointNotice,
  parseCheckpointNotice,
} from "../../lib/agentLoop";
import { selectTurnRunning, sessionMessages, useSessionStore } from "../../store/sessionStore";
import { selectSessionCheckpoints, useCheckpointStore, type CheckpointInfo } from "../../store/checkpointStore";

/** The three restore scopes offered per row — same semantics as
 * `MessageList.tsx`'s `CheckpointRow` (Claude Code /rewind: code only /
 * conversation only / both). */
type RestoreScope = "files" | "conversation" | "both";

type Translate = ReturnType<typeof useT>["t"];

/** Coarse "N minute(s)/hour(s)/day(s) ago" label — matches this app's
 * existing i18n idiom of a single `{{count}}` interpolation with an
 * "(s)" suffix baked into the translated string (see
 * `MessageList.checkpointFilesChanged`) rather than full ICU plural rules. */
function relativeLabel(ms: number, t: Translate): string {
  const diffMs = Math.max(0, Date.now() - ms);
  const minute = 60_000;
  const hour = 60 * minute;
  const day = 24 * hour;
  if (diffMs < minute) return t("CheckpointTimeline.justNow");
  if (diffMs < hour) return t("CheckpointTimeline.minutesAgo", { count: Math.max(1, Math.floor(diffMs / minute)) });
  if (diffMs < day) return t("CheckpointTimeline.hoursAgo", { count: Math.max(1, Math.floor(diffMs / hour)) });
  return t("CheckpointTimeline.daysAgo", { count: Math.max(1, Math.floor(diffMs / day)) });
}

/**
 * Finds `id`'s own checkpoint notice message in `sessionId`'s live transcript
 * (if it's still there — compaction or a rewind may have dropped it) and
 * rewrites it in place with `reverted`. Keeps `MessageList.tsx`'s
 * `CheckpointRow` rendering of that same checkpoint in sync with actions
 * taken from the timeline instead of only mutating disk state the chat
 * bubble has no way to know about.
 */
function syncNoticeReverted(sessionId: string, id: string, reverted: boolean): void {
  const messages = sessionMessages(sessionId);
  for (let i = 0; i < messages.length; i++) {
    const msg = messages[i];
    if (!isCheckpointNotice(msg)) continue;
    const notice = parseCheckpointNotice(msg);
    if (notice && notice.id === id) {
      useSessionStore.getState().updateMessageAt(sessionId, i, {
        content: formatCheckpointNotice({ ...notice, reverted }),
      });
      return;
    }
  }
}

function TimelineRow({
  sessionId,
  info,
  chainBlockedReason,
  onChanged,
}: {
  sessionId: string;
  info: CheckpointInfo;
  /** Non-null (a translated warning) when "Restore to here" should be
   * disabled for this row — a shell command ran during this checkpoint's
   * turn or an earlier one in the newest→here chain, or an intermediate
   * checkpoint in that chain was pruned off disk, so file restore alone
   * can't guarantee full coverage across that span. */
  chainBlockedReason: string | null;
  onChanged: () => void;
}) {
  const { t } = useT();
  const turnRunning = useSessionStore(selectTurnRunning(sessionId));
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [menuOpen, setMenuOpen] = useState(false);
  const menuRef = useRef<HTMLDivElement>(null);

  useEffect(() => {
    if (!menuOpen) return;
    function handlePointerDown(event: PointerEvent) {
      if (menuRef.current && !menuRef.current.contains(event.target as Node)) setMenuOpen(false);
    }
    window.addEventListener("pointerdown", handlePointerDown);
    return () => window.removeEventListener("pointerdown", handlePointerDown);
  }, [menuOpen]);

  const anchorValid = checkpointAnchorValid(sessionMessages(sessionId), {
    id: info.id,
    files: [],
    anchorIndex: info.anchorIndex,
    label: info.label,
  });
  const canRewind = anchorValid && !turnRunning;
  const rewindBlockedReason = turnRunning
    ? t("MessageList.checkpointRewindBlockedTurnRunning")
    : !anchorValid
      ? t("MessageList.checkpointRewindUnavailable")
      : undefined;

  const restoreFiles = async (): Promise<boolean> => {
    try {
      await invoke("checkpoint_revert", { id: info.id });
      syncNoticeReverted(sessionId, info.id, true);
      return true;
    } catch (err) {
      setError(t("MessageList.checkpointRevertFailed", { error: err instanceof Error ? err.message : String(err) }));
      return false;
    }
  };

  const handleRestore = async (scope: RestoreScope) => {
    setMenuOpen(false);
    setBusy(true);
    setError(null);
    try {
      if (scope === "files") {
        await restoreFiles();
      } else if (scope === "conversation") {
        if (canRewind) useSessionStore.getState().truncateFromIndex(sessionId, info.anchorIndex);
      } else if (await restoreFiles()) {
        if (canRewind) useSessionStore.getState().truncateFromIndex(sessionId, info.anchorIndex);
      }
      onChanged();
    } finally {
      setBusy(false);
    }
  };

  const reapply = async () => {
    setBusy(true);
    setError(null);
    try {
      await invoke("checkpoint_reapply", { id: info.id });
      syncNoticeReverted(sessionId, info.id, false);
      onChanged();
    } catch (err) {
      setError(t("MessageList.checkpointReapplyFailed", { error: err instanceof Error ? err.message : String(err) }));
    } finally {
      setBusy(false);
    }
  };

  return (
    <div className="flex flex-col gap-1 border-b border-border px-3 py-2 last:border-b-0">
      <div className="flex items-center justify-between gap-2">
        <span className="min-w-0 truncate text-sm text-foreground" title={info.label || undefined}>
          {info.label || t("CheckpointTimeline.untitledLabel")}
        </span>
        <span className="shrink-0 text-xs text-faint">{relativeLabel(info.createdAtMs, t)}</span>
      </div>
      <div className="flex flex-wrap items-center gap-1.5 text-xs text-muted">
        <span>{t("CheckpointTimeline.filesCount", { count: info.files })}</span>
        {info.shellRan && (
          <span className="flex items-center gap-1 rounded-full bg-warning-soft px-1.5 py-0.5 text-warning">
            <TerminalSquare size={10} />
            {t("CheckpointTimeline.shellRanBadge")}
          </span>
        )}
        {info.reverted && (
          <span className="rounded-full bg-surface-2 px-1.5 py-0.5 text-muted">{t("MessageList.checkpointRevertedLabel")}</span>
        )}
      </div>
      {error && <div className="text-xs text-danger">{error}</div>}
      <div className="mt-1 flex flex-wrap items-center gap-2">
        {info.reverted ? (
          info.reapplyable && (
            <button
              type="button"
              onClick={() => void reapply()}
              disabled={busy}
              className="flex cursor-pointer items-center gap-1 rounded-md border border-border px-2 py-1 text-xs text-muted transition-colors hover:text-foreground disabled:cursor-not-allowed disabled:opacity-50"
            >
              <RefreshCw size={11} />
              {busy ? t("MessageList.checkpointReapplying") : t("MessageList.checkpointReapplyButton")}
            </button>
          )
        ) : (
          <div ref={menuRef} className="relative inline-block">
            <button
              type="button"
              onClick={() => setMenuOpen((prev) => !prev)}
              disabled={busy}
              aria-haspopup="true"
              aria-expanded={menuOpen}
              className="flex cursor-pointer items-center gap-1 rounded-md border border-border px-2 py-1 text-xs text-muted transition-colors hover:text-foreground disabled:cursor-not-allowed disabled:opacity-50"
            >
              <Undo2 size={11} />
              {busy ? t("MessageList.checkpointReverting") : t("MessageList.checkpointRestoreButton")}
            </button>
            {menuOpen && (
              <div className="absolute left-0 top-full z-30 mt-1 w-52 rounded-lg border border-border bg-background py-1 shadow-lg">
                <button
                  type="button"
                  onClick={() => void handleRestore("files")}
                  className="flex w-full cursor-pointer items-center gap-2 px-3 py-2 text-left text-sm hover:bg-surface-2"
                >
                  <FilePenLine size={14} className="shrink-0 text-faint" />
                  {t("MessageList.checkpointRestoreFiles")}
                </button>
                <button
                  type="button"
                  onClick={() => void handleRestore("conversation")}
                  disabled={!canRewind}
                  title={rewindBlockedReason}
                  className="flex w-full cursor-pointer items-center gap-2 px-3 py-2 text-left text-sm hover:bg-surface-2 disabled:cursor-not-allowed disabled:opacity-50 disabled:hover:bg-transparent"
                >
                  <MessageSquareX size={14} className="shrink-0 text-faint" />
                  {t("MessageList.checkpointRewindConversation")}
                </button>
                <button
                  type="button"
                  onClick={() => void handleRestore("both")}
                  disabled={!canRewind}
                  title={rewindBlockedReason}
                  className="flex w-full cursor-pointer items-center gap-2 px-3 py-2 text-left text-sm hover:bg-surface-2 disabled:cursor-not-allowed disabled:opacity-50 disabled:hover:bg-transparent"
                >
                  <Undo2 size={14} className="shrink-0 text-faint" />
                  {t("MessageList.checkpointRestoreBoth")}
                </button>
              </div>
            )}
          </div>
        )}
        <RestoreToHereButton
          sessionId={sessionId}
          info={info}
          disabledReason={chainBlockedReason}
          onDone={onChanged}
        />
      </div>
    </div>
  );
}

/** "Restore to here" — reverts every checkpoint from the newest in this
 * session down through (and including) `info`, in that order. Whole-file
 * originals make each individual revert exact, and reverting newest-first is
 * order-correct even when two turns touched overlapping files (see the
 * design note in checkpoints.rs). Disabled (with a tooltip) when the chain
 * crosses a checkpoint whose turn ran a shell command, or an intermediate
 * checkpoint was pruned off disk — file restore alone can't promise full
 * coverage there. */
function RestoreToHereButton({
  sessionId,
  info,
  disabledReason,
  onDone,
}: {
  sessionId: string;
  info: CheckpointInfo;
  disabledReason: string | null;
  onDone: () => void;
}) {
  const { t } = useT();
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const restoreToHere = async () => {
    setBusy(true);
    setError(null);
    try {
      const chain = useCheckpointStore.getState().bySession[sessionId] ?? [];
      const targetIndex = chain.findIndex((c) => c.id === info.id);
      if (targetIndex === -1) return;
      for (const step of chain.slice(0, targetIndex + 1)) {
        await invoke("checkpoint_revert", { id: step.id });
        syncNoticeReverted(sessionId, step.id, true);
      }
      onDone();
    } catch (err) {
      setError(t("CheckpointTimeline.restoreToHereFailed", { error: err instanceof Error ? err.message : String(err) }));
    } finally {
      setBusy(false);
    }
  };

  return (
    <span className="inline-flex flex-col">
      <button
        type="button"
        onClick={() => void restoreToHere()}
        disabled={busy || Boolean(disabledReason)}
        title={disabledReason ?? undefined}
        className="flex cursor-pointer items-center gap-1 rounded-md border border-border px-2 py-1 text-xs text-muted transition-colors hover:text-foreground disabled:cursor-not-allowed disabled:opacity-50"
      >
        {disabledReason && <Ban size={11} />}
        {busy ? t("CheckpointTimeline.restoringToHere") : t("CheckpointTimeline.restoreToHereButton")}
      </button>
      {error && <span className="mt-0.5 text-xs text-danger">{error}</span>}
    </span>
  );
}

/**
 * Floating timeline panel (matches `ContextUsageIndicator`'s idiom: a small
 * trigger + absolute-positioned popover, closing on outside pointerdown)
 * listing `sessionId`'s checkpoints newest-first via `checkpointStore`.
 * Opened from a history-icon pill placed next to `ContextUsageIndicator` in
 * `ChatWindow`'s toolbar.
 */
export function CheckpointTimeline({ sessionId }: { sessionId: string }) {
  const { t } = useT();
  const [open, setOpen] = useState(false);
  const containerRef = useRef<HTMLDivElement>(null);
  // Not `s.bySession[sessionId] ?? []` inline: a fresh `[]` from getSnapshot
  // on every call makes useSyncExternalStore re-render forever and crash the
  // pane with "Maximum update depth exceeded".
  const checkpoints = useCheckpointStore(selectSessionCheckpoints(sessionId));
  const loading = useCheckpointStore((s) => Boolean(s.loadingSessions[sessionId]));
  const listError = useCheckpointStore((s) => s.errorsBySession[sessionId] ?? null);
  const refresh = useCheckpointStore((s) => s.refresh);

  useEffect(() => {
    if (!open) return;
    function handlePointerDown(event: PointerEvent) {
      if (containerRef.current && !containerRef.current.contains(event.target as Node)) setOpen(false);
    }
    window.addEventListener("pointerdown", handlePointerDown);
    return () => window.removeEventListener("pointerdown", handlePointerDown);
  }, [open]);

  useEffect(() => {
    if (open) void refresh(sessionId);
  }, [open, sessionId, refresh]);

  return (
    <div ref={containerRef} className="relative inline-block">
      <button
        type="button"
        onClick={() => setOpen((prev) => !prev)}
        aria-haspopup="true"
        aria-expanded={open}
        aria-label={t("CheckpointTimeline.openAriaLabel")}
        title={t("CheckpointTimeline.openAriaLabel")}
        className="flex cursor-pointer items-center text-muted transition-colors duration-150 hover:text-foreground"
      >
        <History size={15} />
      </button>

      {open && (
        <div className="absolute bottom-full right-0 z-20 mb-1 max-h-96 w-80 overflow-y-auto rounded-lg border border-border bg-background shadow-lg">
          <div className="sticky top-0 border-b border-border bg-background px-3 py-2">
            <p className="text-sm font-semibold text-foreground">{t("CheckpointTimeline.heading")}</p>
          </div>
          {loading ? (
            <p className="px-3 py-4 text-center text-xs text-faint">{t("CheckpointTimeline.loading")}</p>
          ) : listError ? (
            <p className="px-3 py-4 text-center text-xs text-danger">{listError}</p>
          ) : checkpoints.length === 0 ? (
            <p className="px-3 py-4 text-center text-xs text-faint">{t("CheckpointTimeline.emptyState")}</p>
          ) : (
            checkpoints.map((info, index) => {
              const blockReason = checkpointChainBlockReason(checkpoints, index);
              const chainBlockedReason =
                blockReason === "prunedGap"
                  ? t("CheckpointTimeline.restoreToHereBlockedPruned")
                  : blockReason === "shellRan"
                    ? t("CheckpointTimeline.restoreToHereBlockedShell")
                    : null;
              return (
                <TimelineRow
                  key={info.id}
                  sessionId={sessionId}
                  info={info}
                  chainBlockedReason={chainBlockedReason}
                  onChanged={() => void refresh(sessionId)}
                />
              );
            })
          )}
        </div>
      )}
    </div>
  );
}

export default CheckpointTimeline;
