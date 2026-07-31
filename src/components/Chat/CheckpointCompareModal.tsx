import { useEffect, useState } from "react";
import { ChevronDown, ChevronRight, FileText, X } from "lucide-react";

import { useT } from "../../lib/i18n";
import { StatusPill, type PillTone } from "../ui";
import type { CheckpointInfo } from "../../store/checkpointStore";
import {
  fetchCheckpointCompare,
  type CheckpointCompareResult,
  type CompareFileEntry,
  type DiffResult,
} from "../../lib/checkpointPreview";
import { errorMessage } from "../../lib/errors";

const MAX_RENDERED_DIFF_LINES = 600;

function basename(path: string): string {
  return path.split(/[\\/]/).filter(Boolean).pop() ?? path;
}

function DiffView({ diff, t }: { diff: DiffResult | null; t: ReturnType<typeof useT>["t"] }) {
  if (!diff) {
    return <p className="px-2 py-1.5 text-xs text-faint">{t("CheckpointCompare.diffUnavailable")}</p>;
  }
  if (diff.truncated) {
    return <p className="px-2 py-1.5 text-xs text-faint">{t("CheckpointPreview.diffTooLarge")}</p>;
  }
  if (diff.lines.length === 0) {
    return <p className="px-2 py-1.5 text-xs text-faint">{t("CheckpointCompare.identical")}</p>;
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

function presenceTone(entry: CompareFileEntry): PillTone {
  if (entry.inA && entry.inB) return "neutral";
  return "warning";
}

function CompareFileRow({ entry, expanded, onToggle, t }: { entry: CompareFileEntry; expanded: boolean; onToggle: () => void; t: ReturnType<typeof useT>["t"] }) {
  const presenceLabel = entry.inA && entry.inB
    ? t("CheckpointCompare.presenceBoth")
    : entry.inA
      ? t("CheckpointCompare.presenceOnlyA")
      : t("CheckpointCompare.presenceOnlyB");
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
        <StatusPill tone={presenceTone(entry)}>{presenceLabel}</StatusPill>
      </button>
      {expanded && (
        <div className="px-2 pb-2">
          <DiffView diff={entry.between} t={t} />
        </div>
      )}
    </div>
  );
}

export interface CheckpointCompareModalProps {
  checkpoints: CheckpointInfo[];
  /** Pre-selected pair, newest first (e.g. the row the user clicked
   * "Compare" from, and the checkpoint just above it) — both dropdowns still
   * let the user pick any other pair from the session afterward. */
  initial?: { a: string; b: string };
  onClose: () => void;
}

/**
 * Read-only side-by-side comparison of any two checkpoints in a session, via
 * `checkpoint_compare`. Neither checkpoint is restored — this only reads
 * each one's own backups.
 */
export function CheckpointCompareModal({ checkpoints, initial, onClose }: CheckpointCompareModalProps) {
  const { t } = useT();
  const [idA, setIdA] = useState(initial?.a ?? checkpoints[1]?.id ?? checkpoints[0]?.id ?? "");
  const [idB, setIdB] = useState(initial?.b ?? checkpoints[0]?.id ?? "");
  const [result, setResult] = useState<CheckpointCompareResult | null>(null);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [expanded, setExpanded] = useState<Set<string>>(new Set());

  useEffect(() => {
    if (!idA || !idB) return;
    let cancelled = false;
    setLoading(true);
    setError(null);
    fetchCheckpointCompare(idA, idB)
      .then((r) => {
        if (!cancelled) setResult(r);
      })
      .catch((err) => {
        if (!cancelled) setError(errorMessage(err));
      })
      .finally(() => {
        if (!cancelled) setLoading(false);
      });
    return () => {
      cancelled = true;
    };
  }, [idA, idB]);

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

  const labelFor = (id: string) => checkpoints.find((c) => c.id === id)?.label || t("CheckpointTimeline.untitledLabel");

  return (
    <div
      className="fixed inset-0 z-50 flex items-center justify-center bg-black/40 p-4 backdrop-blur-[2px]"
      role="dialog"
      aria-modal="true"
      aria-labelledby="checkpoint-compare-title"
      onClick={(e) => {
        if (e.target === e.currentTarget) onClose();
      }}
    >
      <div className="flex max-h-[85vh] w-full max-w-2xl flex-col rounded-xl border border-border bg-background shadow-xl">
        <div className="flex items-center justify-between gap-2 border-b border-border px-4 py-3">
          <h2 id="checkpoint-compare-title" className="text-sm font-semibold text-foreground">
            {t("CheckpointCompare.title")}
          </h2>
          <button
            type="button"
            onClick={onClose}
            aria-label={t("CheckpointPreview.closeAriaLabel")}
            className="flex h-7 w-7 shrink-0 cursor-pointer items-center justify-center rounded-md text-muted hover:bg-surface-2 hover:text-foreground"
          >
            <X size={16} />
          </button>
        </div>

        <div className="flex flex-col gap-2 border-b border-border px-4 py-3 sm:flex-row sm:items-center">
          <label className="flex flex-1 flex-col gap-1 text-xs text-faint">
            {t("CheckpointCompare.checkpointALabel")}
            <select
              value={idA}
              onChange={(e) => setIdA(e.target.value)}
              className="rounded-md border border-border bg-surface-2 px-2 py-1.5 text-xs text-foreground"
            >
              {checkpoints.map((c) => (
                <option key={c.id} value={c.id}>
                  {c.label || t("CheckpointTimeline.untitledLabel")}
                </option>
              ))}
            </select>
          </label>
          <label className="flex flex-1 flex-col gap-1 text-xs text-faint">
            {t("CheckpointCompare.checkpointBLabel")}
            <select
              value={idB}
              onChange={(e) => setIdB(e.target.value)}
              className="rounded-md border border-border bg-surface-2 px-2 py-1.5 text-xs text-foreground"
            >
              {checkpoints.map((c) => (
                <option key={c.id} value={c.id}>
                  {c.label || t("CheckpointTimeline.untitledLabel")}
                </option>
              ))}
            </select>
          </label>
        </div>

        <div className="min-h-0 flex-1 overflow-y-auto px-4 py-3">
          {loading && <p className="py-6 text-center text-xs text-faint">{t("CheckpointPreview.loading")}</p>}
          {error && <p className="py-6 text-center text-xs text-danger">{error}</p>}
          {!loading && !error && result && (
            <div className="flex flex-col gap-2">
              <p className="text-xs text-faint">
                {t("CheckpointCompare.comparingLabel", { a: labelFor(idA), b: labelFor(idB) })}
              </p>
              {result.files.length === 0 ? (
                <p className="py-4 text-center text-xs text-faint">{t("CheckpointCompare.noFiles")}</p>
              ) : (
                <div className="overflow-hidden rounded-lg border border-border">
                  {result.files.map((entry) => (
                    <CompareFileRow key={entry.path} entry={entry} expanded={expanded.has(entry.path)} onToggle={() => toggle(entry.path)} t={t} />
                  ))}
                </div>
              )}
            </div>
          )}
        </div>
      </div>
    </div>
  );
}

export default CheckpointCompareModal;
