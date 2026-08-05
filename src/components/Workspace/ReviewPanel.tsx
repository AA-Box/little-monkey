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
  Rows3,
  RefreshCw,
  Search,
  SquareSplitVertical,
  StretchVertical,
  X,
} from "lucide-react";

import { useT } from "../../lib/i18n";
import { Button, IconButton } from "../ui";
import { CriteriaCoverageSection } from "./CriteriaCoverageSection";
import { DiffViewer, computeDiff, type DiffLine } from "./DiffViewer";
import type { ReviewCoverageInput } from "../../lib/reviewCoverage";

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

/** Mirrors Rust `GitChangedFile` in src-tauri/src/git.rs (camelCase there). */
interface GitChangedFile {
  path: string;
  status: string;
}

/** Mirrors Rust `GitFileDiff` in src-tauri/src/git.rs (camelCase there). */
interface GitFileDiff {
  original: string;
  current: string;
  binary: boolean;
  oversize: boolean;
}

type ReviewBase = "branch" | "working";
type DiffLayout = "unified" | "split";
/**
 * How the changed files are laid out. `continuous` stacks every file's diff in
 * one scroll; `single` shows one selected file at a time, which is what the
 * separate Diff panel used to be before it folded into this component — the
 * `diff` right-sidebar tab still opens here, just with `single` as its default.
 */
export type ReviewView = "continuous" | "single";

const LAYOUT_STORAGE_KEY = "little-monkey-review-diff-layout";
/** Runs of unchanged lines longer than this collapse behind a
 * "N unmodified lines" bar (context rows stay visible on each side). */
const COLLAPSE_RUN_THRESHOLD = 8;
const COLLAPSE_CONTEXT = 3;

const STATUS_CLASSES: Record<string, string> = {
  A: "text-success",
  M: "text-warning",
  D: "text-danger",
  R: "text-accent",
};

function readInitialLayout(): DiffLayout {
  try {
    return localStorage.getItem(LAYOUT_STORAGE_KEY) === "split" ? "split" : "unified";
  } catch {
    return "unified";
  }
}

/**
 * One row this panel can render. `git_review` carries content for the files it
 * returns, but it stops at `MAX_REVIEW_FILES` (300, src-tauri/src/git.rs) — in
 * `working` mode the complete list comes from the uncapped `git_changed_files`
 * instead, and anything past the cap arrives with `content: null` and is
 * fetched per file from `git_file_diff` when the user actually opens it. That
 * lazy path is why folding the old Diff panel in here lost nothing: it is the
 * same two commands that panel used, on the same HEAD-vs-disk base.
 */
interface ReviewFileRow {
  path: string;
  /** `A`/`M`/`D`/`R`. Real porcelain output in `working` mode; derived from the
   * payload's own content and counts in `branch` mode, which has no porcelain
   * equivalent (a file committed on the branch is clean in the worktree). */
  status: string;
  added: number;
  deleted: number;
  binary: boolean;
  oversize: boolean;
  /** null while unfetched — see the note above. */
  content: { old: string; new: string } | null;
}

/** `git_review` reports binary and oversized files under one `binary` flag
 * (git.rs collapses them), so a payload row can only ever say "binary". A
 * lazily fetched row keeps them apart, because `git_file_diff` does. */
function rowFromPayload(file: ReviewFilePayload): ReviewFileRow {
  return {
    path: file.path,
    status: deriveStatus(file),
    added: file.added,
    deleted: file.deleted,
    binary: file.binary,
    oversize: false,
    content: file.binary ? null : { old: file.old_content, new: file.new_content },
  };
}

/** Status letter for a file `git_changed_files` cannot speak about. Content
 * decides it when there is content; counts decide it for a binary file.
 * Exported for `ReviewPanel.test.ts` — this repo has no DOM test harness, so
 * the pure functions that decide what renders are tested directly, as
 * `PermissionModal` does with `canRememberForSession`. */
export function deriveStatus(file: ReviewFilePayload): string {
  if (!file.binary) {
    if (file.old_content === "" && file.new_content !== "") return "A";
    if (file.new_content === "" && file.old_content !== "") return "D";
    return "M";
  }
  if (file.deleted === 0 && file.added > 0) return "A";
  if (file.added === 0 && file.deleted > 0) return "D";
  return "M";
}

/**
 * The rows to render for one refresh. `changed` is `git_changed_files`'s
 * uncapped list, passed only in `working` mode; `null` in `branch` mode, where
 * porcelain output describes a different set of files entirely.
 *
 * This is the function that makes folding the old Diff panel in here
 * lossless: with `changed` present, every path porcelain reports gets a row —
 * including the ones `git_review` dropped at its 300-file cap, which arrive
 * with `content: null` and load on open. Exported so a test can pin that.
 */
export function buildRows(
  files: ReviewFilePayload[],
  changed: GitChangedFile[] | null,
): ReviewFileRow[] {
  if (changed === null) return files.map(rowFromPayload);
  const byPath = new Map(files.map((file) => [file.path, file]));
  return changed.map((entry) => {
    const file = byPath.get(entry.path);
    // Porcelain's letter wins over the derived one — it is the real thing.
    return file
      ? { ...rowFromPayload(file), status: entry.status }
      : {
          path: entry.path,
          status: entry.status,
          added: 0,
          deleted: 0,
          binary: false,
          oversize: false,
          content: null,
        };
  });
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

/** The unavailable-content notice a row shows instead of a diff, or null when
 * the row does have content. Kept in one place so the continuous and single
 * views cannot drift into disagreeing about why a file has no diff. */
export function unavailableKey(row: ReviewFileRow, loading: boolean): string | null {
  if (row.binary) return "ReviewPanel.binaryFile";
  if (row.oversize) return "ReviewPanel.oversizeFile";
  if (row.content === null) return loading ? "ReviewPanel.loadingFile" : "ReviewPanel.notLoadedFile";
  return null;
}

function FileDiff({ row, layout, expanded, loading, onToggle, t }: {
  row: ReviewFileRow;
  layout: DiffLayout;
  expanded: boolean;
  loading: boolean;
  onToggle: () => void;
  t: (key: string, vars?: Record<string, string | number>) => string;
}) {
  const [expandedRuns, setExpandedRuns] = useState<Set<number>>(new Set());
  const segments = useMemo(
    () => (row.content === null ? [] : segmentDiff(computeDiff(row.content.old, row.content.new))),
    [row.content],
  );
  const unavailable = unavailableKey(row, loading);

  return (
    <section className="border-b border-border">
      <button
        type="button"
        onClick={onToggle}
        className="sticky top-0 z-10 flex w-full items-center gap-1.5 border-b border-border bg-surface px-3 py-2 text-left hover:bg-surface-2"
      >
        {expanded ? <ChevronDown size={13} className="shrink-0 text-faint" /> : <ChevronRight size={13} className="shrink-0 text-faint" />}
        <FileText size={13} className="shrink-0 text-faint" />
        <span className="min-w-0 flex-1 truncate font-mono text-xs text-foreground">{row.path}</span>
        <span className="shrink-0 font-mono text-[11px] text-success">+{row.added}</span>
        <span className="shrink-0 font-mono text-[11px] text-danger">-{row.deleted}</span>
      </button>

      {expanded && (
        unavailable !== null ? (
          <p className="px-4 py-3 text-xs text-faint">{t(unavailable)}</p>
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
 * Branch-review surface for the right sidebar's "Review" and "Diff" tabs: full
 * diff of the working tree against the branch's merge-base with its upstream
 * (or against HEAD in "working" mode), with per-file collapse, collapsible
 * unmodified runs, unified/split layouts, a filterable file list, criteria
 * coverage, and a compare-URL "Create PR" hand-off.
 *
 * Two view modes, because this panel absorbed the former standalone Diff
 * panel rather than replacing it: `continuous` stacks every file's diff in one
 * scroll, `single` shows one file at a time through `DiffViewer`. The `diff`
 * tab opens with `view="single"` and the `review` tab with `continuous`; either
 * can be switched from the toolbar, and both tabs keep their own shortcut.
 *
 * Reads `git_review`, plus — in `working` mode only — the uncapped
 * `git_changed_files` for the complete file list and `git_file_diff` for
 * content past `git_review`'s 300-file cap. All three are read-only.
 */
export function ReviewPanel({ onClose, view: initialView = "continuous" }: {
  onClose?: () => void;
  view?: ReviewView;
}) {
  const { t } = useT();
  const [review, setReview] = useState<ReviewPayload | null>(null);
  const [rows, setRows] = useState<ReviewFileRow[]>([]);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [base, setBase] = useState<ReviewBase>("branch");
  const [baseMenuOpen, setBaseMenuOpen] = useState(false);
  const [layout, setLayoutState] = useState<DiffLayout>(readInitialLayout);
  const [view, setView] = useState<ReviewView>(initialView);
  const [filesPaneOpen, setFilesPaneOpen] = useState(true);
  const [filter, setFilter] = useState("");
  const [allExpanded, setAllExpanded] = useState(true);
  /** Per-file overrides on top of `allExpanded` — cleared when it flips. */
  const [expandOverrides, setExpandOverrides] = useState<Record<string, boolean>>({});
  /** Selected path in `single` view. */
  const [selectedPath, setSelectedPath] = useState<string | null>(null);
  /** Paths with a `git_file_diff` fetch in flight, so a row can say "loading"
   * rather than "not loaded" and a second open cannot double-fetch. */
  const [fetching, setFetching] = useState<Record<string, boolean>>({});
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
      setFetching({});

      // `git status --porcelain` is HEAD-relative, which is exactly what
      // "working" means here — so it can supply the complete list `git_review`
      // caps, and its real status letters. It is meaningless in "branch" mode:
      // a file committed on the branch is clean in the worktree and would be
      // missing from porcelain entirely.
      const changed = nextBase === "working"
        ? await invoke<GitChangedFile[]>("git_changed_files")
        : null;
      setRows(buildRows(payload.files, changed));
    } catch (invokeError) {
      setError(String(invokeError));
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => {
    void refresh(base);
  }, [base, refresh]);

  /** Fills one row's content from `git_file_diff`. Only reachable in `working`
   * mode, where HEAD-vs-disk is the right base. */
  const loadRow = useCallback((path: string) => {
    setFetching((current) => {
      if (current[path]) return current;
      void invoke<GitFileDiff>("git_file_diff", { path })
        .then((diff) => {
          setRows((currentRows) => currentRows.map((row) => (
            row.path === path
              ? {
                  ...row,
                  binary: diff.binary,
                  oversize: diff.oversize,
                  content: diff.binary || diff.oversize ? null : { old: diff.original, new: diff.current },
                }
              : row
          )));
        })
        .catch((fetchError) => setError(String(fetchError)))
        .finally(() => setFetching((inFlight) => {
          const next = { ...inFlight };
          delete next[path];
          return next;
        }));
      return { ...current, [path]: true };
    });
  }, []);

  const filteredRows = useMemo(() => {
    const needle = filter.trim().toLocaleLowerCase();
    if (!needle) return rows;
    return rows.filter((row) => row.path.toLocaleLowerCase().includes(needle));
  }, [rows, filter]);

  const isExpanded = useCallback(
    (path: string) => expandOverrides[path] ?? allExpanded,
    [allExpanded, expandOverrides],
  );

  // Any row that is on screen and still unfetched gets fetched: every visible
  // row in `continuous`, the selected one in `single`.
  useEffect(() => {
    if (base !== "working") return;
    const wanted = view === "single"
      ? filteredRows.filter((row) => row.path === selectedPath)
      : filteredRows.filter((row) => isExpanded(row.path));
    for (const row of wanted) {
      if (row.content === null && !row.binary && !row.oversize) loadRow(row.path);
    }
  }, [base, view, filteredRows, selectedPath, isExpanded, loadRow]);

  // Keep a selection in `single` view: the current one while it still exists,
  // otherwise the first row, so the pane opens straight onto a diff.
  useEffect(() => {
    if (view !== "single") return;
    setSelectedPath((current) =>
      current !== null && filteredRows.some((row) => row.path === current)
        ? current
        : (filteredRows[0]?.path ?? null),
    );
  }, [view, filteredRows]);

  const toggleFile = useCallback((path: string) => {
    setExpandOverrides((current) => ({ ...current, [path]: !(current[path] ?? allExpanded) }));
  }, [allExpanded]);
  const toggleAll = useCallback(() => {
    setAllExpanded((value) => !value);
    setExpandOverrides({});
  }, []);

  const revealFile = useCallback((path: string) => {
    setSelectedPath(path);
    setExpandOverrides((current) => ({ ...current, [path]: true }));
    fileRefs.current[path]?.scrollIntoView({ behavior: "smooth", block: "start" });
  }, []);

  const openPr = useCallback(() => {
    if (review?.pr_url) window.open(review.pr_url, "_blank", "noopener,noreferrer");
  }, [review?.pr_url]);

  /** What the coverage pass reads. A row with no fetched content is passed as
   * `binary` — `reviewCoverage.ts` already treats that as "no content to
   * cite" and reports it in `uncitableFilePaths`, which is exactly true here
   * rather than a fudge. */
  const coverageInput = useMemo<ReviewCoverageInput | null>(() => {
    if (review === null) return null;
    return {
      branch: review.branch,
      target: review.target,
      total_added: review.total_added,
      total_deleted: review.total_deleted,
      files: rows.map((row) => ({
        path: row.path,
        old_content: row.content?.old ?? "",
        new_content: row.content?.new ?? "",
        added: row.added,
        deleted: row.deleted,
        binary: row.content === null,
      })),
    };
  }, [review, rows]);

  const selectedRow = filteredRows.find((row) => row.path === selectedPath) ?? null;

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
          onClick={() => setView(view === "continuous" ? "single" : "continuous")}
          aria-label={t(view === "continuous" ? "ReviewPanel.switchToSingle" : "ReviewPanel.switchToContinuous")}
          title={t(view === "continuous" ? "ReviewPanel.switchToSingle" : "ReviewPanel.switchToContinuous")}
        >
          {view === "continuous" ? <SquareSplitVertical size={14} /> : <StretchVertical size={14} />}
        </IconButton>
        {view === "continuous" && (
          <IconButton
            size="sm"
            variant="ghost"
            onClick={toggleAll}
            aria-label={t(allExpanded ? "ReviewPanel.collapseAllDiffs" : "ReviewPanel.expandAllDiffs")}
            title={t(allExpanded ? "ReviewPanel.collapseAllDiffs" : "ReviewPanel.expandAllDiffs")}
          >
            {allExpanded ? <FoldVertical size={14} /> : <ListCollapse size={14} />}
          </IconButton>
        )}
        {view === "continuous" && (
          <IconButton
            size="sm"
            variant="ghost"
            onClick={() => setLayout(layout === "unified" ? "split" : "unified")}
            aria-label={t(layout === "unified" ? "ReviewPanel.switchToSplit" : "ReviewPanel.switchToUnified")}
            title={t(layout === "unified" ? "ReviewPanel.switchToSplit" : "ReviewPanel.switchToUnified")}
          >
            {layout === "unified" ? <Columns2 size={14} /> : <Rows3 size={14} />}
          </IconButton>
        )}
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

      {/* Criteria coverage over the whole change, so it spans both columns. */}
      <CriteriaCoverageSection review={coverageInput} mode={base} t={t} onRevealPath={revealFile} />

      <div className="flex min-h-0 flex-1">
        {/* Main: every file's diff stacked, or just the selected one. */}
        <div className="min-h-0 flex-1 overflow-auto [overscroll-behavior:contain]">
          {review && !review.is_repo ? (
            <p className="p-4 text-sm text-faint">{t("ReviewPanel.notARepo")}</p>
          ) : filteredRows.length === 0 ? (
            <p className="p-4 text-sm text-faint">{loading ? t("ReviewPanel.loading") : t("ReviewPanel.noChanges")}</p>
          ) : view === "single" ? (
            selectedRow === null ? (
              <p className="p-4 text-sm text-muted">{t("ReviewPanel.selectHint")}</p>
            ) : (
              (() => {
                const unavailable = unavailableKey(selectedRow, Boolean(fetching[selectedRow.path]));
                return unavailable !== null ? (
                  <p className="p-4 text-sm text-muted">{t(unavailable)}</p>
                ) : (
                  <div className="p-3">
                    <DiffViewer
                      fileName={selectedRow.path}
                      oldValue={selectedRow.content?.old ?? ""}
                      newValue={selectedRow.content?.new ?? ""}
                      oldTitle={t(base === "branch" ? "ReviewPanel.oldTitleBranch" : "ReviewPanel.oldTitle")}
                      newTitle={t("ReviewPanel.newTitle")}
                    />
                  </div>
                );
              })()
            )
          ) : (
            filteredRows.map((row) => (
              <div key={row.path} ref={(node) => { fileRefs.current[row.path] = node; }}>
                <FileDiff
                  row={row}
                  layout={layout}
                  expanded={isExpanded(row.path)}
                  loading={Boolean(fetching[row.path])}
                  onToggle={() => toggleFile(row.path)}
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
              {filteredRows.map((row) => {
                const segments = row.path.split("/");
                const name = segments.pop() ?? row.path;
                const dir = segments.join("/");
                const selected = view === "single" && row.path === selectedPath;
                return (
                  <button
                    key={row.path}
                    type="button"
                    onClick={() => revealFile(row.path)}
                    title={row.path}
                    aria-current={selected ? "true" : undefined}
                    className={`flex w-full items-center gap-1.5 rounded-md px-2 py-1.5 text-left text-xs ${selected ? "bg-surface-2 text-foreground" : "hover:bg-surface-2"}`}
                  >
                    <span className={`w-3 shrink-0 text-center font-mono font-semibold ${STATUS_CLASSES[row.status] ?? "text-faint"}`}>
                      {row.status}
                    </span>
                    <span className="min-w-0 flex-1 truncate">
                      {dir && <span className="text-faint">{dir}/</span>}
                      <span className="text-foreground">{name}</span>
                    </span>
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
