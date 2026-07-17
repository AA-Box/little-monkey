import { useMemo, useState } from "react";
import { AlertTriangle, Check, FileSpreadsheet, Loader2, Sparkles, X, XCircle } from "lucide-react";

import { useT } from "../../lib/i18n";
import {
  cellRef,
  columnLetters,
  parseRangeRef,
  type SpreadsheetTable,
} from "../../lib/spreadsheetCopilot";
import { useSpreadsheetCopilotStore } from "../../store/spreadsheetCopilotStore";
import { Button, IconButton } from "../ui";

interface SpreadsheetCopilotPanelProps {
  onClose: () => void;
}

/** Rendering every row of a huge CSV would make the grid unusable — this MVP
 * caps the preview, matching `spreadsheetCopilot.ts`'s own `MAX_SAMPLE_ROWS`
 * cap on what gets sent to the model (a larger cap here since rendering is
 * cheaper than a model call, but still bounded). */
const MAX_RENDERED_ROWS = 300;

function errorText(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

/** Expands every cited range/cell string into the individual A1 refs it
 * covers, so the grid can highlight exactly the cells a proposal cites —
 * silently skips any string that fails to parse (shouldn't happen, since
 * `spreadsheetCopilot.ts` only ever emits validated ranges, but the grid
 * must never crash on a malformed one). */
function expandRefs(ranges: string[]): Set<string> {
  const refs = new Set<string>();
  for (const range of ranges) {
    const parsed = parseRangeRef(range);
    if (!parsed) continue;
    const minRow = Math.min(parsed.start.sheetRow, parsed.end.sheetRow);
    const maxRow = Math.max(parsed.start.sheetRow, parsed.end.sheetRow);
    const minCol = Math.min(parsed.start.col, parsed.end.col);
    const maxCol = Math.max(parsed.start.col, parsed.end.col);
    for (let row = minRow; row <= maxRow; row += 1) {
      for (let col = minCol; col <= maxCol; col += 1) {
        refs.add(cellRef(row, col));
      }
    }
  }
  return refs;
}

interface SpreadsheetGridProps {
  table: SpreadsheetTable;
  citedRefs: Set<string>;
  changedRefs: Set<string>;
}

/** A plain, dependency-free grid view: column letters + row numbers as
 * sticky headers, with cells cited by the pending proposal (read or written)
 * highlighted — the "cited range highlighted" half of this feature's
 * acceptance criterion. */
function SpreadsheetGrid({ table, citedRefs, changedRefs }: SpreadsheetGridProps) {
  const { t } = useT();
  const truncated = table.rows.length > MAX_RENDERED_ROWS;
  const visibleRows = table.rows.slice(0, MAX_RENDERED_ROWS);

  return (
    <div className="min-h-0 flex-1 overflow-auto rounded-md border border-border">
      <table className="border-collapse text-[11px]">
        <thead>
          <tr>
            <th className="sticky left-0 top-0 z-20 min-w-8 border-b border-r border-border bg-surface-2 px-1.5 py-1 text-faint" />
            {table.headers.map((_, col) => (
              <th
                key={`letter-${col}`}
                className="sticky top-0 z-10 min-w-24 border-b border-r border-border bg-surface-2 px-2 py-1 text-center font-mono text-faint"
              >
                {columnLetters(col)}
              </th>
            ))}
          </tr>
          <tr>
            <th className="sticky left-0 top-6 z-20 border-b border-r border-border bg-surface-2 px-1.5 py-1 text-faint">1</th>
            {table.headers.map((header, col) => {
              const ref = cellRef(1, col);
              const isChanged = changedRefs.has(ref);
              const isCited = citedRefs.has(ref);
              return (
                <th
                  key={`header-${col}`}
                  className={`sticky top-6 z-10 min-w-24 border-b border-r border-border px-2 py-1 text-left font-semibold text-foreground ${
                    isChanged ? "bg-success/20" : isCited ? "bg-accent/10" : "bg-surface-2"
                  }`}
                  title={isChanged ? t("SpreadsheetCopilot.gridChangedCell") : isCited ? t("SpreadsheetCopilot.gridCitedCell") : undefined}
                >
                  {header || <span className="text-faint">({t("SpreadsheetCopilot.gridEmptyHeader")})</span>}
                </th>
              );
            })}
          </tr>
        </thead>
        <tbody>
          {visibleRows.map((row, rowIndex) => {
            const sheetRow = rowIndex + 2;
            return (
              <tr key={sheetRow}>
                <th className="sticky left-0 z-10 border-b border-r border-border bg-surface-2 px-1.5 py-1 text-faint">{sheetRow}</th>
                {row.map((value, col) => {
                  const ref = cellRef(sheetRow, col);
                  const isChanged = changedRefs.has(ref);
                  const isCited = citedRefs.has(ref);
                  return (
                    <td
                      key={ref}
                      className={`min-w-24 whitespace-nowrap border-b border-r border-border px-2 py-1 text-foreground ${
                        isChanged ? "bg-success/20" : isCited ? "bg-accent/10" : ""
                      }`}
                      title={isChanged ? t("SpreadsheetCopilot.gridChangedCell") : isCited ? t("SpreadsheetCopilot.gridCitedCell") : undefined}
                    >
                      {value}
                    </td>
                  );
                })}
              </tr>
            );
          })}
        </tbody>
      </table>
      {truncated && (
        <p className="border-t border-border bg-surface-2 px-2 py-1 text-[10px] text-faint">
          {t("SpreadsheetCopilot.gridTruncated", { shown: MAX_RENDERED_ROWS, total: table.rows.length })}
        </p>
      )}
    </div>
  );
}

export function SpreadsheetCopilotPanel({ onClose }: SpreadsheetCopilotPanelProps) {
  const { t } = useT();
  const store = useSpreadsheetCopilotStore();
  const [proposeError, setProposeError] = useState<string | null>(null);
  const [approveError, setApproveError] = useState<string | null>(null);

  const displayedTable = store.proposal?.proposedTable ?? store.table;
  const citedRefs = useMemo(
    () => (store.proposal ? expandRefs(store.proposal.citedRanges) : new Set<string>()),
    [store.proposal],
  );
  const changedRefs = useMemo(
    () => (store.proposal ? new Set(store.proposal.diff.map((entry) => entry.ref)) : new Set<string>()),
    [store.proposal],
  );

  const handlePropose = async () => {
    setProposeError(null);
    try {
      await store.propose();
    } catch (err) {
      setProposeError(errorText(err));
    }
  };

  const handleApprove = async () => {
    setApproveError(null);
    try {
      await store.approve();
    } catch (err) {
      setApproveError(errorText(err));
    }
  };

  return (
    <section className="flex h-full min-h-0 flex-col" aria-labelledby="spreadsheet-copilot-title">
      <header className="flex shrink-0 items-start justify-between gap-3 border-b border-border px-5 py-4">
        <div>
          <h2 id="spreadsheet-copilot-title" className="text-sm font-semibold text-foreground">
            {t("SpreadsheetCopilot.title")}
          </h2>
          <p className="mt-1 max-w-2xl text-xs leading-5 text-muted">{t("SpreadsheetCopilot.subtitle")}</p>
        </div>
        <IconButton size="sm" aria-label={t("SpreadsheetCopilot.close")} title={t("SpreadsheetCopilot.close")} onClick={onClose}>
          <X size={15} />
        </IconButton>
      </header>

      <div className="flex shrink-0 flex-wrap items-center gap-2 border-b border-border px-5 py-3">
        <Button size="sm" onClick={() => void store.loadFromFile()} disabled={store.loadingFile}>
          {store.loadingFile ? <Loader2 className="animate-spin" size={13} /> : <FileSpreadsheet size={13} />}
          {t("SpreadsheetCopilot.loadButton")}
        </Button>
        {store.fileName && (
          <span className="truncate rounded-md border border-border bg-surface px-2 py-1 font-mono text-[11px] text-muted">
            {store.fileName}
          </span>
        )}
        {!store.table && <p className="text-[11px] text-faint">{t("SpreadsheetCopilot.noFileHint")}</p>}
      </div>

      {store.error && (
        <div role="alert" className="mx-5 mt-3 rounded-md border border-danger/40 bg-danger/10 p-3 text-xs text-danger">
          {store.error}
        </div>
      )}

      {store.table && (
        <div className="grid min-h-0 flex-1 gap-4 overflow-hidden p-5 xl:grid-cols-[minmax(0,1.4fr)_minmax(18rem,.9fr)]">
          <div className="flex min-h-0 flex-col gap-3">
            <SpreadsheetGrid table={displayedTable!} citedRefs={citedRefs} changedRefs={changedRefs} />

            <form
              className="flex shrink-0 flex-wrap items-end gap-2"
              onSubmit={(event) => {
                event.preventDefault();
                void handlePropose();
              }}
            >
              <label className="min-w-64 flex-1 text-xs text-muted">
                {t("SpreadsheetCopilot.requestLabel")}
                <input
                  className="mt-1 w-full rounded-md border border-border bg-background px-2.5 py-2 text-sm text-foreground outline-none focus:border-accent focus:ring-1 focus:ring-accent"
                  placeholder={t("SpreadsheetCopilot.requestPlaceholder")}
                  value={store.requestText}
                  onChange={(event) => store.setRequestText(event.target.value)}
                  disabled={store.proposing}
                />
              </label>
              <Button type="submit" variant="primary" disabled={store.proposing || !store.requestText.trim()}>
                {store.proposing ? <Loader2 className="animate-spin" size={14} /> : <Sparkles size={14} />}
                {t("SpreadsheetCopilot.proposeButton")}
              </Button>
            </form>
            {proposeError && <p className="text-xs text-danger">{proposeError}</p>}
          </div>

          <div className="min-h-0 overflow-y-auto rounded-lg border border-border bg-surface p-4">
            {!store.proposal ? (
              <p className="p-6 text-center text-xs text-faint">{t("SpreadsheetCopilot.noProposalHint")}</p>
            ) : (
              <div className="space-y-3">
                <div>
                  <h3 className="text-sm font-semibold text-foreground">{store.proposal.title}</h3>
                  <p className="mt-1 text-xs leading-5 text-muted">{store.proposal.explanation}</p>
                </div>

                <div>
                  <h4 className="text-[11px] font-semibold uppercase tracking-wide text-faint">
                    {t("SpreadsheetCopilot.citedRangesHeading")}
                  </h4>
                  <div className="mt-1.5 flex flex-wrap gap-1.5">
                    {store.proposal.citedRanges.map((range) => (
                      <span
                        key={range}
                        className="rounded-md border border-accent/40 bg-accent/10 px-1.5 py-0.5 font-mono text-[10px] text-foreground"
                      >
                        {range}
                      </span>
                    ))}
                  </div>
                </div>

                <div>
                  <h4 className="text-[11px] font-semibold uppercase tracking-wide text-faint">
                    {t("SpreadsheetCopilot.diffHeading", { count: store.proposal.diff.length })}
                  </h4>
                  <div className="mt-1.5 max-h-48 space-y-1 overflow-y-auto">
                    {store.proposal.diff.map((entry) => (
                      <div key={entry.ref} className="rounded-md border border-border bg-background px-2 py-1 text-[11px]">
                        <span className="font-mono font-semibold text-foreground">{entry.ref}</span>{" "}
                        {entry.before === null ? (
                          <span className="text-faint">{t("SpreadsheetCopilot.diffNewCell")}</span>
                        ) : (
                          <span className="text-danger line-through">{entry.before || t("SpreadsheetCopilot.diffBlank")}</span>
                        )}{" "}
                        <span className="text-success">→ {entry.after || t("SpreadsheetCopilot.diffBlank")}</span>
                      </div>
                    ))}
                  </div>
                </div>

                <p className="rounded-md border border-dashed border-border p-2.5 text-[10px] leading-4 text-faint">
                  {t("SpreadsheetCopilot.approveWarning")}
                </p>

                {approveError && <p className="text-xs text-danger">{approveError}</p>}

                <div className="flex gap-2">
                  <Button size="sm" variant="danger" onClick={() => store.reject()} disabled={store.approving}>
                    <XCircle size={13} /> {t("SpreadsheetCopilot.rejectButton")}
                  </Button>
                  <Button size="sm" variant="primary" onClick={() => void handleApprove()} disabled={store.approving}>
                    {store.approving ? <Loader2 className="animate-spin" size={13} /> : <Check size={13} />}
                    {t("SpreadsheetCopilot.approveButton")}
                  </Button>
                </div>
              </div>
            )}
          </div>
        </div>
      )}

      {!store.table && !store.error && (
        <div className="flex flex-1 items-center justify-center p-8">
          <p className="flex max-w-md items-center gap-2 text-center text-sm text-faint">
            <AlertTriangle size={14} className="shrink-0" /> {t("SpreadsheetCopilot.emptyState")}
          </p>
        </div>
      )}
    </section>
  );
}

export default SpreadsheetCopilotPanel;
