import { useMemo } from "react";
import { useT } from "../../lib/i18n";

export interface DiffViewerProps {
  /** Original / "before" text. */
  oldValue: string;
  /** Modified / "after" text. */
  newValue: string;
  /** Label for the left/old side, e.g. "on disk". */
  oldTitle?: string;
  /** Label for the right/new side, e.g. "proposed". */
  newTitle?: string;
  /** Optional file path/name shown in the header. */
  fileName?: string;
  className?: string;
}

type DiffLineType = "unchanged" | "added" | "removed";

interface DiffLine {
  type: DiffLineType;
  oldLineNo: number | null;
  newLineNo: number | null;
  text: string;
}

/** Guard against pathological O(n*m) blowups on very large files. */
const LCS_CELL_BUDGET = 4_000_000;

function splitLines(text: string): string[] {
  if (text === "") return [];
  // Normalize CRLF so Windows-authored files don't show every line as changed.
  return text.replace(/\r\n/g, "\n").split("\n");
}

/** Classic LCS-based line diff — O(n*m) time/space, fine for typical source files. */
function lcsDiff(a: string[], b: string[]): DiffLine[] {
  const n = a.length;
  const m = b.length;
  const dp: number[][] = Array.from({ length: n + 1 }, () => new Array<number>(m + 1).fill(0));

  for (let i = n - 1; i >= 0; i--) {
    for (let j = m - 1; j >= 0; j--) {
      dp[i][j] = a[i] === b[j] ? dp[i + 1][j + 1] + 1 : Math.max(dp[i + 1][j], dp[i][j + 1]);
    }
  }

  const result: DiffLine[] = [];
  let i = 0;
  let j = 0;
  let oldNo = 1;
  let newNo = 1;

  while (i < n && j < m) {
    if (a[i] === b[j]) {
      result.push({ type: "unchanged", oldLineNo: oldNo++, newLineNo: newNo++, text: a[i] });
      i++;
      j++;
    } else if (dp[i + 1][j] >= dp[i][j + 1]) {
      result.push({ type: "removed", oldLineNo: oldNo++, newLineNo: null, text: a[i] });
      i++;
    } else {
      result.push({ type: "added", oldLineNo: null, newLineNo: newNo++, text: b[j] });
      j++;
    }
  }
  while (i < n) {
    result.push({ type: "removed", oldLineNo: oldNo++, newLineNo: null, text: a[i] });
    i++;
  }
  while (j < m) {
    result.push({ type: "added", oldLineNo: null, newLineNo: newNo++, text: b[j] });
    j++;
  }
  return result;
}

/**
 * Fallback for huge inputs: trims the common prefix/suffix and treats the
 * remaining middle block as one wholesale removal + addition. Cheap (O(n+m))
 * and still gives a useful, if coarser, diff.
 */
function naiveDiff(a: string[], b: string[]): DiffLine[] {
  const n = a.length;
  const m = b.length;
  const maxPrefix = Math.min(n, m);

  let prefix = 0;
  while (prefix < maxPrefix && a[prefix] === b[prefix]) prefix++;

  let suffix = 0;
  const maxSuffix = maxPrefix - prefix;
  while (suffix < maxSuffix && a[n - 1 - suffix] === b[m - 1 - suffix]) suffix++;

  const result: DiffLine[] = [];
  for (let k = 0; k < prefix; k++) {
    result.push({ type: "unchanged", oldLineNo: k + 1, newLineNo: k + 1, text: a[k] });
  }
  for (let k = prefix; k < n - suffix; k++) {
    result.push({ type: "removed", oldLineNo: k + 1, newLineNo: null, text: a[k] });
  }
  for (let k = prefix; k < m - suffix; k++) {
    result.push({ type: "added", oldLineNo: null, newLineNo: k + 1, text: b[k] });
  }
  for (let k = 0; k < suffix; k++) {
    const oldIdx = n - suffix + k;
    const newIdx = m - suffix + k;
    result.push({ type: "unchanged", oldLineNo: oldIdx + 1, newLineNo: newIdx + 1, text: a[oldIdx] });
  }
  return result;
}

function computeDiff(oldText: string, newText: string): DiffLine[] {
  const a = splitLines(oldText);
  const b = splitLines(newText);
  if (a.length * b.length > LCS_CELL_BUDGET) {
    return naiveDiff(a, b);
  }
  return lcsDiff(a, b);
}

function lineNoCol(n: number | null): string {
  return n === null ? "" : String(n);
}

export function DiffViewer({
  oldValue,
  newValue,
  oldTitle = "before",
  newTitle = "after",
  fileName,
  className = "",
}: DiffViewerProps) {
  const lines = useMemo(() => computeDiff(oldValue, newValue), [oldValue, newValue]);

  const added = useMemo(() => lines.filter((l) => l.type === "added").length, [lines]);
  const removed = useMemo(() => lines.filter((l) => l.type === "removed").length, [lines]);
  const { t } = useT();
  const hasChanges = added > 0 || removed > 0;

  return (
    <div
      className={`flex h-full min-h-0 flex-col overflow-hidden rounded-md border border-border ${className}`}
    >
      <div className="flex shrink-0 flex-wrap items-center justify-between gap-2 border-b border-border bg-surface-2 px-3 py-2">
        <span className="truncate font-mono text-xs text-faint">
          {fileName ?? t("DiffViewer.diffHeaderTitle", { oldTitle, newTitle })}
        </span>
        <div className="flex shrink-0 items-center gap-3 font-mono text-[11px]">
          <span className="text-success">+{added}</span>
          <span className="text-danger">-{removed}</span>
        </div>
      </div>

      <div className="min-h-0 flex-1 overflow-auto font-mono text-xs leading-relaxed">
        {!hasChanges ? (
          <p className="p-4 text-sm text-faint">{t("DiffViewer.noDifferences")}</p>
        ) : (
          <div>
            {lines.map((line, idx) => {
              const rowClass =
                line.type === "added" ? "bg-success-soft" : line.type === "removed" ? "bg-danger-soft" : "";
              const textClass =
                line.type === "added"
                  ? "text-success"
                  : line.type === "removed"
                    ? "text-danger"
                    : "text-muted";
              const marker = line.type === "added" ? "+" : line.type === "removed" ? "-" : " ";

              return (
                <div key={idx} className={`flex ${rowClass}`}>
                  <span className="w-10 shrink-0 select-none whitespace-nowrap pr-2 text-right text-faint">
                    {lineNoCol(line.oldLineNo)}
                  </span>
                  <span className="w-10 shrink-0 select-none whitespace-nowrap pr-2 text-right text-faint">
                    {lineNoCol(line.newLineNo)}
                  </span>
                  <span className={`w-4 shrink-0 select-none text-center ${textClass}`}>{marker}</span>
                  <span className={`min-w-0 flex-1 whitespace-pre-wrap break-all px-2 ${textClass}`}>
                    {line.text.length > 0 ? line.text : " "}
                  </span>
                </div>
              );
            })}
          </div>
        )}
      </div>
    </div>
  );
}

export default DiffViewer;
