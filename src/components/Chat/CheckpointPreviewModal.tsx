import { useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import {
  AlertTriangle,
  CheckCircle2,
  ChevronDown,
  ChevronRight,
  FileText,
  Image as ImageIcon,
  MessageSquareX,
  ShieldAlert,
  Undo2,
  X,
  XCircle,
} from "lucide-react";

import { useT } from "../../lib/i18n";
import { Button, StatusPill, type PillTone } from "../ui";
import { formatCheckpointNotice, isCheckpointNotice, parseCheckpointNotice } from "../../lib/agentLoop";
import { sessionMessages, useSessionStore } from "../../store/sessionStore";
import { useCheckpointStore } from "../../store/checkpointStore";
import {
  fetchCheckpointSimulateRestore,
  loadCheckpointFullPreview,
  type CheckpointFullPreview,
  type DiffResult,
  type FileChangeStatus,
  type FilePreviewEntry,
  type RestoreSimulation,
} from "../../lib/checkpointPreview";
import { errorMessage } from "../../lib/errors";

const STATUS_TONE: Record<FileChangeStatus, PillTone> = {
  added: "success",
  modified: "neutral",
  deleted: "danger",
  unchanged: "neutral",
  unknown: "warning",
};

/** Caps how many diff lines are actually rendered — `diff_lines` on the Rust
 * side already refuses to compute a diff at all past a much larger cell
 * count (see `MAX_DIFF_CELLS`), but a merely-large (not pathological) file
 * can still produce thousands of lines that would make the DOM sluggish for
 * no benefit — nobody reads a 3000-line diff in a preview popover. */
const MAX_RENDERED_DIFF_LINES = 600;

function basename(path: string): string {
  return path.split(/[\\/]/).filter(Boolean).pop() ?? path;
}

function DiffView({ diff, binary, t }: { diff: DiffResult | null; binary: boolean; t: ReturnType<typeof useT>["t"] }) {
  if (binary) {
    return <p className="px-2 py-1.5 text-xs text-faint">{t("CheckpointPreview.binaryFile")}</p>;
  }
  if (!diff) {
    return <p className="px-2 py-1.5 text-xs text-faint">{t("CheckpointPreview.diffUnavailable")}</p>;
  }
  if (diff.truncated) {
    return <p className="px-2 py-1.5 text-xs text-faint">{t("CheckpointPreview.diffTooLarge")}</p>;
  }
  if (diff.lines.length === 0) {
    return <p className="px-2 py-1.5 text-xs text-faint">{t("CheckpointPreview.noTextChange")}</p>;
  }
  const shown = diff.lines.slice(0, MAX_RENDERED_DIFF_LINES);
  const hidden = diff.lines.length - shown.length;
  return (
    <pre className="max-h-64 overflow-auto rounded-md border border-border bg-surface-2 p-2 font-mono text-[11px] leading-relaxed">
      {shown.map((line, i) => (
        <div
          key={i}
          className={
            line.kind === "added"
              ? "bg-success-soft text-success"
              : line.kind === "removed"
                ? "bg-danger-soft text-danger"
                : "text-muted"
          }
        >
          {line.kind === "added" ? "+ " : line.kind === "removed" ? "- " : "  "}
          {line.text}
        </div>
      ))}
      {hidden > 0 && <div className="text-faint">{t("CheckpointPreview.diffMoreLines", { count: hidden })}</div>}
    </pre>
  );
}

function FileEntryRow({
  entry,
  expanded,
  onToggle,
  t,
}: {
  entry: FilePreviewEntry;
  expanded: boolean;
  onToggle: () => void;
  t: ReturnType<typeof useT>["t"];
}) {
  const statusLabel = t(`CheckpointPreview.status.${entry.status}`);
  return (
    <div className="border-b border-border last:border-b-0">
      <button
        type="button"
        onClick={onToggle}
        className="flex w-full cursor-pointer items-center gap-2 px-2 py-1.5 text-left text-xs hover:bg-surface-2"
      >
        {expanded ? <ChevronDown size={12} className="shrink-0 text-faint" /> : <ChevronRight size={12} className="shrink-0 text-faint" />}
        <FileText size={12} className="shrink-0 text-faint" />
        <span className="min-w-0 flex-1 truncate font-mono" title={entry.path}>
          {basename(entry.path)}
        </span>
        <StatusPill tone={STATUS_TONE[entry.status]}>{statusLabel}</StatusPill>
        {entry.afterSource === "live" && (
          <span title={t("CheckpointPreview.afterSourceLiveTooltip")} className="text-faint">
            <AlertTriangle size={11} />
          </span>
        )}
      </button>
      {expanded && (
        <div className="px-2 pb-2">
          <DiffView diff={entry.diff} binary={entry.binary} t={t} />
        </div>
      )}
    </div>
  );
}

type RestoreScope = "files" | "conversation" | "both";

/** The minimal shape this modal actually needs — satisfied structurally by
 * both `checkpointStore.ts`'s full `CheckpointInfo` (the timeline's rows)
 * and `agentLoop.ts`'s `CheckpointNotice` (the inline transcript notice
 * `MessageList.tsx`'s `CheckpointRow` renders), so either call site can open
 * this modal without an adapter object. */
export interface CheckpointPreviewSubject {
  id: string;
  anchorIndex: number;
  label: string;
  shellRan: boolean;
  reverted: boolean;
}

export interface CheckpointPreviewModalProps {
  sessionId: string;
  checkpoint: CheckpointPreviewSubject;
  onClose: () => void;
  /** Called after a successful action (restore/reapply) so the caller can
   * refresh whatever list is showing this checkpoint. */
  onChanged: () => void;
}

/**
 * Modal preview for one checkpoint: its per-file diff (via
 * `checkpoint_preview`), any artifacts/screenshots/verification results
 * produced during its turn, and a rollback simulation (via
 * `checkpoint_simulate_restore`) showing exactly what restoring it would do
 * — all before anything is actually restored. See
 * `src/lib/checkpointPreview.ts` for the data-gathering layer this renders.
 */
export function CheckpointPreviewModal({ sessionId, checkpoint, onClose, onChanged }: CheckpointPreviewModalProps) {
  const { t } = useT();
  const [preview, setPreview] = useState<CheckpointFullPreview | null>(null);
  const [simulation, setSimulation] = useState<RestoreSimulation | null>(null);
  const [loading, setLoading] = useState(true);
  const [loadError, setLoadError] = useState<string | null>(null);
  const [expanded, setExpanded] = useState<Set<string>>(new Set());
  const [busy, setBusy] = useState(false);
  const [actionError, setActionError] = useState<string | null>(null);

  useEffect(() => {
    let cancelled = false;
    setLoading(true);
    setLoadError(null);
    Promise.all([
      loadCheckpointFullPreview(sessionMessages(sessionId), {
        id: checkpoint.id,
        anchorIndex: checkpoint.anchorIndex,
        label: checkpoint.label,
        shellRan: checkpoint.shellRan,
      }),
      fetchCheckpointSimulateRestore(checkpoint.id),
    ])
      .then(([p, s]) => {
        if (cancelled) return;
        setPreview(p);
        setSimulation(s);
      })
      .catch((err) => {
        if (cancelled) return;
        setLoadError(errorMessage(err));
      })
      .finally(() => {
        if (!cancelled) setLoading(false);
      });
    return () => {
      cancelled = true;
    };
  }, [sessionId, checkpoint.id, checkpoint.anchorIndex, checkpoint.label, checkpoint.shellRan]);

  useEffect(() => {
    function handleKeyDown(e: KeyboardEvent) {
      if (e.key === "Escape") onClose();
    }
    window.addEventListener("keydown", handleKeyDown);
    return () => window.removeEventListener("keydown", handleKeyDown);
  }, [onClose]);

  const toggle = (path: string) => {
    setExpanded((prev) => {
      const next = new Set(prev);
      if (next.has(path)) next.delete(path);
      else next.add(path);
      return next;
    });
  };

  const runRestore = async (scope: RestoreScope) => {
    if (!preview) return;
    setBusy(true);
    setActionError(null);
    try {
      let filesOk = true;
      if (scope !== "conversation") {
        try {
          await invoke("checkpoint_revert", { id: checkpoint.id });
        } catch (err) {
          filesOk = false;
          setActionError(t("CheckpointPreview.restoreFailed", { error: errorMessage(err) }));
        }
      }
      if (filesOk && scope !== "files" && preview.conversationRewindAvailable) {
        useSessionStore.getState().truncateFromIndex(sessionId, checkpoint.anchorIndex);
      }
      if (filesOk && scope !== "conversation") {
        const messages = sessionMessages(sessionId);
        for (let i = 0; i < messages.length; i++) {
          const msg = messages[i];
          if (!isCheckpointNotice(msg)) continue;
          const notice = parseCheckpointNotice(msg);
          if (notice?.id === checkpoint.id) {
            useSessionStore.getState().updateMessageAt(sessionId, i, {
              content: formatCheckpointNotice({ ...notice, reverted: true }),
            });
            break;
          }
        }
      }
      void useCheckpointStore.getState().refresh(sessionId);
      onChanged();
      if (filesOk) onClose();
    } finally {
      setBusy(false);
    }
  };

  const context = preview;
  const externalCount = context?.external.length ?? 0;

  return (
    <div
      className="fixed inset-0 z-50 flex items-center justify-center bg-black/40 p-4 backdrop-blur-[2px]"
      role="dialog"
      aria-modal="true"
      aria-labelledby="checkpoint-preview-title"
      onClick={(e) => {
        if (e.target === e.currentTarget) onClose();
      }}
    >
      <div className="flex max-h-[85vh] w-full max-w-2xl flex-col rounded-xl border border-border bg-background shadow-xl">
        <div className="flex items-center justify-between gap-2 border-b border-border px-4 py-3">
          <div className="min-w-0">
            <h2 id="checkpoint-preview-title" className="truncate text-sm font-semibold text-foreground">
              {checkpoint.label || t("CheckpointTimeline.untitledLabel")}
            </h2>
            <p className="text-xs text-faint">{t("CheckpointPreview.subtitle")}</p>
          </div>
          <button
            type="button"
            onClick={onClose}
            aria-label={t("CheckpointPreview.closeAriaLabel")}
            className="flex h-7 w-7 shrink-0 cursor-pointer items-center justify-center rounded-md text-muted hover:bg-surface-2 hover:text-foreground"
          >
            <X size={16} />
          </button>
        </div>

        <div className="min-h-0 flex-1 overflow-y-auto px-4 py-3">
          {loading && <p className="py-6 text-center text-xs text-faint">{t("CheckpointPreview.loading")}</p>}
          {loadError && <p className="py-6 text-center text-xs text-danger">{loadError}</p>}

          {!loading && !loadError && context && (
            <div className="flex flex-col gap-4">
              {/* External / needs-reconciliation banner — must be the first
                  thing shown, never buried, per the acceptance criterion
                  that unsupported external effects are surfaced, not hidden. */}
              {context.needsReconciliation && (
                <div className="flex flex-col gap-1.5 rounded-lg border border-warning/40 bg-warning-soft p-3">
                  <div className="flex items-center gap-2 text-sm font-medium text-warning">
                    <ShieldAlert size={15} className="shrink-0" />
                    {t("CheckpointPreview.needsReconciliationTitle")}
                  </div>
                  <p className="text-xs text-warning">{t("CheckpointPreview.needsReconciliationBody")}</p>
                  {externalCount > 0 && (
                    <ul className="flex flex-wrap gap-1.5 pt-1">
                      {context.external.map((effect) => (
                        <li key={effect.tool} className="rounded-full bg-background px-2 py-0.5 text-[11px] font-mono text-warning">
                          {effect.tool}
                        </li>
                      ))}
                    </ul>
                  )}
                  {context.external.length === 0 && (
                    <p className="text-[11px] text-warning/80">{t("CheckpointPreview.shellOnlyCaveat")}</p>
                  )}
                </div>
              )}

              {/* File state */}
              <section>
                <h3 className="mb-1.5 text-xs font-semibold uppercase tracking-wide text-faint">
                  {t("CheckpointPreview.filesHeading", { count: preview?.filePreview.files.length ?? 0 })}
                </h3>
                <div className="overflow-hidden rounded-lg border border-border">
                  {preview?.filePreview.files.length ? (
                    preview.filePreview.files.map((entry) => (
                      <FileEntryRow key={entry.path} entry={entry} expanded={expanded.has(entry.path)} onToggle={() => toggle(entry.path)} t={t} />
                    ))
                  ) : (
                    <p className="px-2 py-2 text-xs text-faint">{t("CheckpointTimeline.emptyState")}</p>
                  )}
                </div>
              </section>

              {/* Artifact state */}
              {context.artifacts.length > 0 && (
                <section>
                  <h3 className="mb-1.5 text-xs font-semibold uppercase tracking-wide text-faint">
                    {t("CheckpointPreview.artifactsHeading", { count: context.artifacts.length })}
                  </h3>
                  <ul className="flex flex-col gap-1">
                    {context.artifacts.map((block) => (
                      <li
                        key={`${block.ref.messageIndex}-${block.ref.blockIndex}`}
                        className="flex items-center gap-2 rounded-md border border-border px-2 py-1.5 text-xs text-muted"
                      >
                        <span className="rounded bg-surface-2 px-1.5 py-0.5 font-mono text-[10px] uppercase">{block.kind}</span>
                        <span className="truncate">{block.title}</span>
                      </li>
                    ))}
                  </ul>
                </section>
              )}

              {/* Screenshot / image evidence */}
              {context.images.length > 0 && (
                <section>
                  <h3 className="mb-1.5 text-xs font-semibold uppercase tracking-wide text-faint">
                    {t("CheckpointPreview.imagesHeading", { count: context.images.length })}
                  </h3>
                  <div className="flex flex-wrap gap-2">
                    {context.images.map((image, i) => (
                      <div key={i} className="flex items-center gap-1.5 rounded-md border border-border px-2 py-1.5 text-xs text-muted">
                        {image.url.startsWith("data:") ? (
                          <img src={image.url} alt="" className="h-8 w-8 shrink-0 rounded object-cover" />
                        ) : (
                          <ImageIcon size={12} className="shrink-0 text-faint" />
                        )}
                        {t("CheckpointPreview.imageAttachment")}
                      </div>
                    ))}
                  </div>
                </section>
              )}

              {/* Verification state */}
              {context.verify.length > 0 && (
                <section>
                  <h3 className="mb-1.5 text-xs font-semibold uppercase tracking-wide text-faint">
                    {t("CheckpointPreview.verifyHeading", { count: context.verify.length })}
                  </h3>
                  <ul className="flex flex-col gap-1">
                    {context.verify.map((notice, i) => (
                      <li key={i} className="flex items-center gap-2 rounded-md border border-border px-2 py-1.5 text-xs">
                        {notice.ok ? (
                          <CheckCircle2 size={13} className="shrink-0 text-success" />
                        ) : (
                          <XCircle size={13} className="shrink-0 text-danger" />
                        )}
                        <span className="min-w-0 flex-1 truncate font-mono">{notice.label}</span>
                        <span className="shrink-0 text-faint">{notice.ok ? t("CheckpointPreview.verifyPassed") : t("CheckpointPreview.verifyFailed")}</span>
                      </li>
                    ))}
                  </ul>
                </section>
              )}

              {/* Conversation state */}
              <section>
                <h3 className="mb-1.5 text-xs font-semibold uppercase tracking-wide text-faint">
                  {t("CheckpointPreview.conversationHeading")}
                </h3>
                <p className="text-xs text-muted">
                  {context.conversationRewindAvailable
                    ? t("CheckpointPreview.conversationAvailable")
                    : t("MessageList.checkpointRewindUnavailable")}
                </p>
              </section>

              {/* Rollback simulation */}
              {simulation && (
                <section>
                  <h3 className="mb-1.5 text-xs font-semibold uppercase tracking-wide text-faint">
                    {t("CheckpointPreview.simulationHeading")}
                  </h3>
                  {simulation.alreadyReverted ? (
                    <p className="text-xs text-muted">{t("CheckpointPreview.simulationAlreadyReverted")}</p>
                  ) : simulation.files.length === 0 ? (
                    <p className="text-xs text-muted">{t("CheckpointPreview.simulationNothingToDo")}</p>
                  ) : (
                    <ul className="flex flex-col gap-1">
                      {simulation.files.map((plan) => (
                        <li key={plan.path} className="flex items-center gap-2 rounded-md border border-border px-2 py-1.5 text-xs">
                          <span className="min-w-0 flex-1 truncate font-mono" title={plan.path}>
                            {basename(plan.path)}
                          </span>
                          <StatusPill tone={plan.action === "noOp" ? "neutral" : plan.action === "delete" ? "danger" : "warning"}>
                            {t(`CheckpointPreview.restoreAction.${plan.action}`)}
                          </StatusPill>
                          {plan.drifted && (
                            <span
                              className="flex items-center gap-1 text-warning"
                              title={t("CheckpointPreview.driftedTooltip")}
                            >
                              <AlertTriangle size={12} />
                            </span>
                          )}
                        </li>
                      ))}
                    </ul>
                  )}
                  {/* The recorded effects, which outlive the transcript the
                      warning above is derived from. Each carries the reason
                      nothing undoes it, rather than one generic caveat. */}
                  {simulation.externalEffects.length > 0 && (
                    <ul className="mt-2 flex flex-col gap-1 border-t border-border pt-2">
                      {simulation.externalEffects.map((effect) => (
                        <li
                          key={effect.kind}
                          /* An effect with a real undo is not a warning. Colouring
                             it the same as the ones nothing can reverse is what
                             would make the warnings stop being read. */
                          className={`text-[11px] leading-snug ${
                            effect.compensation.kind === "undo" ? "text-muted" : "text-warning"
                          }`}
                        >
                          <span className="font-medium">
                            {t(`CheckpointPreview.effectKind.${effect.kind}`)}
                          </span>
                          {" — "}
                          {effect.compensation.kind === "undo"
                            ? t("CheckpointPreview.willUndo", { action: effect.compensation.action })
                            : effect.compensation.reason}
                        </li>
                      ))}
                    </ul>
                  )}
                </section>
              )}
            </div>
          )}
        </div>

        {!loading && !loadError && context && !checkpoint.reverted && (
          <div className="flex flex-col gap-2 border-t border-border px-4 py-3">
            {actionError && <p className="text-xs text-danger">{actionError}</p>}
            <div className="flex flex-wrap items-center justify-end gap-2">
              <Button variant="secondary" size="sm" onClick={onClose} disabled={busy}>
                {t("CheckpointPreview.cancelButton")}
              </Button>
              <Button variant="secondary" size="sm" onClick={() => void runRestore("files")} disabled={busy}>
                <Undo2 size={12} />
                {t("MessageList.checkpointRestoreFiles")}
              </Button>
              <Button
                variant="secondary"
                size="sm"
                onClick={() => void runRestore("conversation")}
                disabled={busy || !context.conversationRewindAvailable}
                title={context.conversationRewindAvailable ? undefined : t("MessageList.checkpointRewindUnavailable")}
              >
                <MessageSquareX size={12} />
                {t("MessageList.checkpointRewindConversation")}
              </Button>
              <Button variant="primary" size="sm" onClick={() => void runRestore("both")} disabled={busy}>
                {busy ? t("MessageList.checkpointReverting") : t("MessageList.checkpointRestoreBoth")}
              </Button>
            </div>
          </div>
        )}
      </div>
    </div>
  );
}

export default CheckpointPreviewModal;
