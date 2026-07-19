import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { invoke, isTauri } from "@tauri-apps/api/core";
import {
  ChevronDown,
  ChevronRight,
  ChevronUp,
  Columns2,
  FileText,
  FoldVertical,
  Folder,
  GitPullRequest,
  ListCollapse,
  RefreshCw,
  Rows3,
  Search,
  X,
} from "lucide-react";

import { useT } from "../../lib/i18n";
import { Button, IconButton } from "../ui";
import { computeDiff, type DiffLine } from "./DiffViewer";

/** Mirrors Rust `ReviewFilePayload` / `ReviewPayload` in src-tauri/src/git.rs. */
interface ReviewFilePayload {
  path: string;
  old_content: string;
  new_content: string;
  added: number;
  deleted: number;
  binary: boolean;
}

interface ReviewPayload {
  is_repo: boolean;
  branch: string | null;
  target: string | null;
  total_added: number;
  total_deleted: number;
  files: ReviewFilePayload[];
  pr_url: string | null;
}

type ReviewBase = "branch" | "working";
type DiffLayout = "unified" | "split";

const LAYOUT_STORAGE_KEY = "little-monkey-review-diff-layout";
/** Runs of unchanged lines longer than this collapse behind a
 * "N unmodified lines" bar (context rows stay visible on each side). */
const COLLAPSE_RUN_THRESHOLD = 8;
const COLLAPSE_CONTEXT = 3;

function readInitialLayout(): DiffLayout {
  try {
    return localStorage.getItem(LAYOUT_STORAGE_KEY) === "split" ? "split" : "unified";
  } catch {
    return "unified";
  }
}

/** A renderable segment of one file's diff: either visible lines or a
 * collapsed unmodified run the user can expand in place. */
interface DiffSegment {
  kind: "lines" | "collapsed";
  lines: DiffLine[];
}

function segmentDiff(lines: DiffLine[]): DiffSegment[] {
  const segments: DiffSegment[] = [];
  let index = 0;
  while (index < lines.length) {
    if (lines[index].type !== "unchanged") {
      const start = index;
      while (index < lines.length && lines[index].type !== "unchanged") index += 1;
      segments.push({ kind: "lines", lines: lines.slice(start, index) });
      continue;
    }
    const start = index;
    while (index < lines.length && lines[index].type === "unchanged") index += 1;
    const run = lines.slice(start, index);
    const isFirst = start === 0;
    const isLast = index === lines.length;
    const leading = isFirst ? 0 : COLLAPSE_CONTEXT;
    const trailing = isLast ? 0 : COLLAPSE_CONTEXT;
    if (run.length > leading + trailing + COLLAPSE_RUN_THRESHOLD) {
      if (leading > 0) segments.push({ kind: "lines", lines: run.slice(0, leading) });
      segments.push({ kind: "collapsed", lines: run.slice(leading, run.length - trailing) });
      if (trailing > 0) segments.push({ kind: "lines", lines: run.slice(run.length - trailing) });
    } else {
      segments.push({ kind: "lines", lines: run });
    }
  }
  return segments;
}

/** One split-view row: an old-side and new-side line paired up. */
interface SplitRow {
  old: DiffLine | null;
  new: DiffLine | null;
}

function splitRows(lines: DiffLine[]): SplitRow[] {
  const rows: SplitRow[] = [];
  let index = 0;
  while (index < lines.length) {
    const line = lines[index];
    if (line.type === "unchanged") {
      rows.push({ old: line, new: line });
      index += 1;
      continue;
    }
    const removed: DiffLine[] = [];
    const added: DiffLine[] = [];
    while (index < lines.length && lines[index].type === "removed") {
      removed.push(lines[index]);
      index += 1;
    }
    while (index < lines.length && lines[index].type === "added") {
      added.push(lines[index]);
      index += 1;
    }
    const max = Math.max(removed.length, added.length);
    for (let i = 0; i < max; i += 1) {
      rows.push({ old: removed[i] ?? null, new: added[i] ?? null });
    }
  }
  return rows;
}

function lineBg(type: DiffLine["type"] | undefined): string {
  if (type === "added") return "bg-success-soft";
  if (type === "removed") return "bg-danger-soft";
  return "";
}

function SideCell({ line, side }: { line: DiffLine | null; side: "old" | "new" }) {
  const number = line ? (side === "old" ? line.oldLineNo : line.newLineNo) : null;
  return (
    <div className={`flex min-w-0 flex-1 ${lineBg(line?.type)}`}>
      <span className="w-10 shrink-0 select-none whitespace-nowrap pr-2 text-right text-faint">
        {number ?? ""}
      </span>
      <span className="min-w-0 flex-1 whitespace-pre-wrap break-all px-1.5 text-muted">
        {line ? (line.text.length > 0 ? line.text : " ") : ""}
      </span>
    </div>
  );
}

function CollapsedBar({ count, onExpand, label }: { count: number; onExpand: () => void; label: string }) {
  return (
    <button
      type="button"
      onClick={onExpand}
      className="flex w-full items-center gap-2 bg-surface-2 px-3 py-1 text-left text-[11px] text-muted hover:bg-surface hover:text-foreground"
    >
      <span className="flex flex-col text-faint">
        <ChevronUp size={11} />
        <ChevronDown size={11} />
      </span>
      {label.replace("{count}", String(count))}
    </button>
  );
}

function FileDiff({ file, layout, expanded, onToggle, t }: {
  file: ReviewFilePayload;
  layout: DiffLayout;
  expanded: boolean;
  onToggle: () => void;
  t: (key: string, vars?: Record<string, string | number>) => string;
}) {
  const [expandedRuns, setExpandedRuns] = useState<Set<number>>(new Set());
  const segments = useMemo(
    () => (file.binary ? [] : segmentDiff(computeDiff(file.old_content, file.new_content))),
    [file],
  );

  return (
    <section className="border-b border-border">
      <button
        type="button"
        onClick={onToggle}
        className="sticky top-0 z-10 flex w-full items-center gap-1.5 border-b border-border bg-surface px-3 py-2 text-left hover:bg-surface-2"
      >
        {expanded ? <ChevronDown size={13} className="shrink-0 text-faint" /> : <ChevronRight size={13} className="shrink-0 text-faint" />}
        <FileText size={13} className="shrink-0 text-faint" />
        <span className="min-w-0 flex-1 truncate font-mono text-xs text-foreground">{file.path}</span>
        <span className="shrink-0 font-mono text-[11px] text-success">+{file.added}</span>
        <span className="shrink-0 font-mono text-[11px] text-danger">-{file.deleted}</span>
      </button>

      {expanded && (
        file.binary ? (
          <p className="px-4 py-3 text-xs text-faint">{t("ReviewPanel.binaryFile")}</p>
        ) : (
          <div className="font-mono text-xs leading-relaxed">
            {segments.map((segment, segmentIndex) => {
              if (segment.kind === "collapsed" && !expandedRuns.has(segmentIndex)) {
                return (
                  <CollapsedBar
                    key={segmentIndex}
                    count={segment.lines.length}
                    label={t("ReviewPanel.unmodifiedLines")}
                    onExpand={() => setExpandedRuns((current) => new Set(current).add(segmentIndex))}
                  />
                );
              }
              if (layout === "split") {
                return splitRows(segment.lines).map((row, rowIndex) => (
                  <div key={`${segmentIndex}:${rowIndex}`} className="flex border-b border-transparent">
                    <SideCell line={row.old} side="old" />
                    <div className="w-px shrink-0 bg-border" />
                    <SideCell line={row.new} side="new" />
                  </div>
                ));
              }
              return segment.lines.map((line, lineIndex) => (
                <div key={`${segmentIndex}:${lineIndex}`} className={`flex ${lineBg(line.type)}`}>
                  <span className="w-10 shrink-0 select-none whitespace-nowrap pr-2 text-right text-faint">{line.oldLineNo ?? ""}</span>
                  <span className="w-10 shrink-0 select-none whitespace-nowrap pr-2 text-right text-faint">{line.newLineNo ?? ""}</span>
                  <span className={`w-4 shrink-0 select-none text-center ${line.type === "added" ? "text-success" : line.type === "removed" ? "text-danger" : "text-faint"}`}>
                    {line.type === "added" ? "+" : line.type === "removed" ? "-" : " "}
                  </span>
                  <span className="min-w-0 flex-1 whitespace-pre-wrap break-all px-2 text-muted">
                    {line.text.length > 0 ? line.text : " "}
                  </span>
                </div>
              ));
            })}
          </div>
        )
      )}
    </section>
  );
}

/**
 * Branch-review surface for the right sidebar's "Review" tab: full diff of
 * the working tree against the branch's merge-base with its upstream (or
 * against HEAD in "working" mode), with per-file collapse, collapsible
 * unmodified runs, unified/split layouts, a filterable file list, and a
 * compare-URL "Create PR" hand-off. Read-only over `git_review`.
 */
export function ReviewPanel({ onClose }: { onClose?: () => void }) {
  const { t } = useT();
  const [review, setReview] = useState<ReviewPayload | null>(null);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [base, setBase] = useState<ReviewBase>("branch");
  const [baseMenuOpen, setBaseMenuOpen] = useState(false);
  const [layout, setLayoutState] = useState<DiffLayout>(readInitialLayout);
  const [filesPaneOpen, setFilesPaneOpen] = useState(true);
  const [filter, setFilter] = useState("");
  const [allExpanded, setAllExpanded] = useState(true);
  /** Per-file overrides on top of `allExpanded` — cleared when it flips. */
  const [expandOverrides, setExpandOverrides] = useState<Record<string, boolean>>({});
  const fileRefs = useRef<Record<string, HTMLDivElement | null>>({});

  const setLayout = useCallback((value: DiffLayout) => {
    setLayoutState(value);
    try {
      localStorage.setItem(LAYOUT_STORAGE_KEY, value);
    } catch {
      // Best-effort persistence only.
    }
  }, []);

  // Deliberately no dependency on `t`: useT() returns a fresh function every
  // render, and depending on it would give `refresh` a new identity per
  // render — which made the `[base, refresh]` effect below refetch
  // `git_review` on every parent re-render (visibly "refreshing" during
  // sidebar resize drags). The error is stored as an i18n key and
  // translated at render time instead.
  const refresh = useCallback(async (nextBase: ReviewBase) => {
    if (!isTauri()) {
      setError("ReviewPanel.desktopOnly");
      return;
    }
    setLoading(true);
    setError(null);
    try {
      const payload = await invoke<ReviewPayload>("git_review", { mode: nextBase });
      setReview(payload);
    } catch (invokeError) {
      setError(String(invokeError));
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => {
    void refresh(base);
  }, [base, refresh]);

  const files = review?.files ?? [];
  const filteredFiles = useMemo(() => {
    const needle = filter.trim().toLocaleLowerCase();
    if (!needle) return files;
    return files.filter((file) => file.path.toLocaleLowerCase().includes(needle));
  }, [files, filter]);

  const isExpanded = useCallback(
    (path: string) => expandOverrides[path] ?? allExpanded,
    [allExpanded, expandOverrides],
  );
  const toggleFile = useCallback((path: string) => {
    setExpandOverrides((current) => ({ ...current, [path]: !(current[path] ?? allExpanded) }));
  }, [allExpanded]);
  const toggleAll = useCallback(() => {
    setAllExpanded((value) => !value);
    setExpandOverrides({});
  }, []);

  const scrollToFile = useCallback((path: string) => {
    setExpandOverrides((current) => ({ ...current, [path]: true }));
    fileRefs.current[path]?.scrollIntoView({ behavior: "smooth", block: "start" });
  }, []);

  const openPr = useCallback(() => {
    if (review?.pr_url) window.open(review.pr_url, "_blank", "noopener,noreferrer");
  }, [review?.pr_url]);

  return (
    <div className="flex min-h-0 flex-1 flex-col">
      {/* Toolbar: base selector, totals, branch → target, view controls. */}
      <div className="relative flex shrink-0 flex-wrap items-center gap-1.5 border-b border-border px-3 py-1.5">
        <button
          type="button"
          onClick={() => setBaseMenuOpen((open) => !open)}
          className="inline-flex items-center gap-1 rounded-md px-1.5 py-1 text-sm font-medium text-foreground hover:bg-surface-2"
        >
          {t(base === "branch" ? "ReviewPanel.baseBranch" : "ReviewPanel.baseWorking")}
          <ChevronDown size={13} className="text-faint" />
        </button>
        {baseMenuOpen && (
          <>
            <div className="fixed inset-0 z-30" onClick={() => setBaseMenuOpen(false)} />
            <div className="absolute left-3 top-9 z-40 w-56 rounded-xl border border-border bg-background p-1.5 shadow-xl">
              {(["branch", "working"] as const).map((option) => (
                <button
                  key={option}
                  type="button"
                  onClick={() => {
                    setBaseMenuOpen(false);
                    setBase(option);
                  }}
                  className={`flex w-full items-center rounded-lg px-2.5 py-2 text-left text-sm hover:bg-surface-2 ${option === base ? "text-foreground" : "text-muted"}`}
                >
                  {t(option === "branch" ? "ReviewPanel.baseBranch" : "ReviewPanel.baseWorking")}
                </button>
              ))}
            </div>
          </>
        )}
        <span className="shrink-0 font-mono text-xs">
          <span className="text-success">+{review?.total_added ?? 0}</span>{" "}
          <span className="text-danger">-{review?.total_deleted ?? 0}</span>
        </span>
        {review?.branch && (
          <span className="hidden min-w-0 items-center gap-1 truncate text-xs text-muted md:flex">
            <span className="truncate">{review.branch}</span>
            {review.target && (
              <>
                <span className="shrink-0 text-faint">→</span>
                <span className="shrink-0">{review.target}</span>
              </>
            )}
          </span>
        )}
        <div className="flex-1" />
        <IconButton size="sm" variant="ghost" onClick={() => void refresh(base)} aria-label={t("ReviewPanel.refresh")} title={t("ReviewPanel.refresh")}>
          <RefreshCw size={14} className={loading ? "animate-spin" : ""} />
        </IconButton>
        <IconButton
          size="sm"
          variant="ghost"
          onClick={toggleAll}
          aria-label={t(allExpanded ? "ReviewPanel.collapseAllDiffs" : "ReviewPanel.expandAllDiffs")}
          title={t(allExpanded ? "ReviewPanel.collapseAllDiffs" : "ReviewPanel.expandAllDiffs")}
        >
          {allExpanded ? <FoldVertical size={14} /> : <ListCollapse size={14} />}
        </IconButton>
        <IconButton
          size="sm"
          variant="ghost"
          onClick={() => setLayout(layout === "unified" ? "split" : "unified")}
          aria-label={t(layout === "unified" ? "ReviewPanel.switchToSplit" : "ReviewPanel.switchToUnified")}
          title={t(layout === "unified" ? "ReviewPanel.switchToSplit" : "ReviewPanel.switchToUnified")}
        >
          {layout === "unified" ? <Columns2 size={14} /> : <Rows3 size={14} />}
        </IconButton>
        {onClose && (
          <IconButton size="sm" variant="ghost" onClick={onClose} aria-label={t("App.closeRightTab")} title={t("App.closeRightTab")}>
            <X size={14} />
          </IconButton>
        )}
        <IconButton
          size="sm"
          variant={filesPaneOpen ? "secondary" : "ghost"}
          onClick={() => setFilesPaneOpen((open) => !open)}
          aria-label={t(filesPaneOpen ? "ReviewPanel.hideFiles" : "ReviewPanel.showFiles")}
          title={t(filesPaneOpen ? "ReviewPanel.hideFiles" : "ReviewPanel.showFiles")}
        >
          <Folder size={14} />
        </IconButton>
        <Button size="sm" variant="secondary" onClick={openPr} disabled={!review?.pr_url}>
          <GitPullRequest size={13} /> {t("ReviewPanel.createPr")}
        </Button>
      </div>

      {error && (
        <div className="border-b border-danger bg-danger-soft px-3 py-1.5 text-xs text-danger">
          {error.startsWith("ReviewPanel.") ? t(error) : error}
        </div>
      )}

      <div className="flex min-h-0 flex-1">
        {/* Main: per-file diffs. */}
        <div className="min-h-0 flex-1 overflow-auto [overscroll-behavior:contain]">
          {review && !review.is_repo ? (
            <p className="p-4 text-sm text-faint">{t("ReviewPanel.notARepo")}</p>
          ) : filteredFiles.length === 0 ? (
            <p className="p-4 text-sm text-faint">{loading ? t("ReviewPanel.loading") : t("ReviewPanel.noChanges")}</p>
          ) : (
            filteredFiles.map((file) => (
              <div key={file.path} ref={(node) => { fileRefs.current[file.path] = node; }}>
                <FileDiff
                  file={file}
                  layout={layout}
                  expanded={isExpanded(file.path)}
                  onToggle={() => toggleFile(file.path)}
                  t={t}
                />
              </div>
            ))
          )}
        </div>

        {/* Right: filterable changed-file list. */}
        {filesPaneOpen && (
          <div className="flex w-56 shrink-0 flex-col border-l border-border">
            <div className="relative shrink-0 p-2">
              <Search size={12} className="pointer-events-none absolute left-4 top-1/2 -translate-y-1/2 text-faint" />
              <input
                value={filter}
                onChange={(event) => setFilter(event.target.value)}
                placeholder={t("ReviewPanel.filterFiles")}
                aria-label={t("ReviewPanel.filterFiles")}
                className="w-full rounded-md border border-border bg-background py-1.5 pl-7 pr-2 text-xs text-foreground outline-none focus:border-accent"
              />
            </div>
            <div className="min-h-0 flex-1 overflow-y-auto px-2 pb-2 [overscroll-behavior:contain]">
              {filteredFiles.map((file) => {
                const segments = file.path.split("/");
                const name = segments.pop() ?? file.path;
                const dir = segments.join("/");
                return (
                  <button
                    key={file.path}
                    type="button"
                    onClick={() => scrollToFile(file.path)}
                    title={file.path}
                    className="flex w-full items-center gap-1.5 rounded-md px-2 py-1.5 text-left text-xs hover:bg-surface-2"
                  >
                    <FileText size={12} className="shrink-0 text-faint" />
                    <span className="min-w-0 flex-1 truncate">
                      {dir && <span className="text-faint">{dir}/</span>}
                      <span className="text-foreground">{name}</span>
                    </span>
                    <span className="h-1.5 w-1.5 shrink-0 rounded-full bg-warning" />
                  </button>
                );
              })}
            </div>
          </div>
        )}
      </div>
    </div>
  );
}

export default ReviewPanel;
