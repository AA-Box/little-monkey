import { useState } from "react";
import ReactMarkdown from "react-markdown";
import {
  AlertTriangle,
  ArrowDown,
  ArrowUp,
  Copy,
  Database,
  FileSpreadsheet,
  FileText,
  Play,
  Plus,
  RefreshCw,
  Trash2,
  Upload,
  X,
} from "lucide-react";

import { useT } from "../../lib/i18n";
import { PROSE_CLASSES } from "../Chat/MessageBubble";
import { useDataNotebookStore } from "../../store/dataNotebookStore";
import type { Notebook, NotebookCell } from "../../lib/dataNotebook";
import { Button, IconButton } from "../ui";

export interface DataNotebookPanelProps {
  onClose: () => void;
}

/** Renders one SQL cell's last-run output: a result table, an "N row(s)
 * affected" note for DML with no result set, an error, or "not yet run". */
function CellOutput({ cell, t }: { cell: NotebookCell; t: ReturnType<typeof useT>["t"] }) {
  if (cell.error) {
    return (
      <div role="alert" className="flex items-start gap-2 rounded-md border border-danger/30 bg-danger-soft px-3 py-2 text-xs text-danger">
        <AlertTriangle size={14} className="mt-0.5 shrink-0" aria-hidden="true" />
        <span>{t("DataNotebookPanel.cell.error", { error: cell.error })}</span>
      </div>
    );
  }
  if (!cell.output) {
    return <p className="px-1 text-xs text-faint">{t("DataNotebookPanel.cell.notRun")}</p>;
  }
  const { output } = cell;
  if (output.columns.length === 0) {
    return (
      <p className="px-1 text-xs text-muted">
        {output.rowsAffected > 0
          ? t("DataNotebookPanel.cell.rowsAffected", { count: output.rowsAffected })
          : t("DataNotebookPanel.cell.noResults")}
      </p>
    );
  }
  return (
    <div className="flex flex-col gap-1.5">
      <div className="overflow-x-auto rounded-md border border-border">
        <table className="w-full min-w-max text-left text-xs">
          <thead className="bg-surface-2 text-faint">
            <tr>
              {output.columns.map((col, i) => (
                <th key={`${col}-${i}`} className="whitespace-nowrap px-2.5 py-1.5 font-medium">
                  {col}
                </th>
              ))}
            </tr>
          </thead>
          <tbody>
            {output.rows.map((row, rowIndex) => (
              <tr key={rowIndex} className="border-t border-border">
                {row.map((value, colIndex) => (
                  <td key={colIndex} className="whitespace-nowrap px-2.5 py-1.5 text-foreground">
                    {value === null || value === undefined ? <span className="text-faint">null</span> : String(value)}
                  </td>
                ))}
              </tr>
            ))}
          </tbody>
        </table>
      </div>
      {output.truncated && (
        <p className="px-1 text-xs text-faint">
          {t("DataNotebookPanel.cell.truncated", { shown: output.rows.length, total: output.rowCount })}
        </p>
      )}
    </div>
  );
}

function CellCard({
  notebook,
  cell,
  index,
  running,
  t,
}: {
  notebook: Notebook;
  cell: NotebookCell;
  index: number;
  running: boolean;
  t: ReturnType<typeof useT>["t"];
}) {
  const updateCellSource = useDataNotebookStore((s) => s.updateCellSource);
  const removeCell = useDataNotebookStore((s) => s.removeCell);
  const moveCell = useDataNotebookStore((s) => s.moveCell);
  const runCell = useDataNotebookStore((s) => s.runCell);
  const [previewMarkdown, setPreviewMarkdown] = useState(true);

  return (
    <div className="flex flex-col gap-2 rounded-lg border border-border bg-background p-3">
      <div className="flex items-center justify-between gap-2">
        <span className="inline-flex items-center gap-1.5 rounded-full bg-surface-2 px-2 py-0.5 text-[11px] font-medium uppercase tracking-wide text-faint">
          {cell.type === "sql" ? <Database size={11} /> : <FileText size={11} />}
          {cell.type === "sql" ? t("DataNotebookPanel.cell.sqlLabel") : t("DataNotebookPanel.cell.markdownLabel")}
        </span>
        <div className="flex items-center gap-1">
          {cell.type === "markdown" && (
            <Button variant="ghost" size="sm" onClick={() => setPreviewMarkdown((v) => !v)}>
              {previewMarkdown ? t("DataNotebookPanel.cell.markdownEditToggle") : t("DataNotebookPanel.cell.markdownPreviewToggle")}
            </Button>
          )}
          {cell.type === "sql" && (
            <Button variant="secondary" size="sm" onClick={() => void runCell(notebook.id, cell.id)} disabled={running}>
              <Play size={12} /> {t("DataNotebookPanel.cell.run")}
            </Button>
          )}
          <IconButton
            size="sm"
            aria-label={t("DataNotebookPanel.cell.moveUp")}
            onClick={() => moveCell(notebook.id, cell.id, "up")}
            disabled={index === 0}
          >
            <ArrowUp size={14} />
          </IconButton>
          <IconButton
            size="sm"
            aria-label={t("DataNotebookPanel.cell.moveDown")}
            onClick={() => moveCell(notebook.id, cell.id, "down")}
            disabled={index === notebook.cells.length - 1}
          >
            <ArrowDown size={14} />
          </IconButton>
          <IconButton
            size="sm"
            variant="danger"
            aria-label={t("DataNotebookPanel.cell.delete")}
            onClick={() => removeCell(notebook.id, cell.id)}
          >
            <Trash2 size={14} />
          </IconButton>
        </div>
      </div>

      {cell.type === "markdown" && previewMarkdown && cell.source.trim().length > 0 ? (
        <div className={PROSE_CLASSES} onClick={() => setPreviewMarkdown(false)}>
          <ReactMarkdown>{cell.source}</ReactMarkdown>
        </div>
      ) : (
        <textarea
          value={cell.source}
          onChange={(e) => updateCellSource(notebook.id, cell.id, e.target.value)}
          placeholder={cell.type === "sql" ? t("DataNotebookPanel.cell.sqlPlaceholder") : t("DataNotebookPanel.cell.markdownPlaceholder")}
          rows={cell.type === "sql" ? 3 : 4}
          spellCheck={false}
          className="w-full resize-y rounded-md border border-border bg-surface px-2.5 py-2 font-mono text-xs text-foreground outline-none focus:border-border-strong"
        />
      )}

      {cell.type === "sql" && <CellOutput cell={cell} t={t} />}
    </div>
  );
}

function DatasetSection({ notebook, t }: { notebook: Notebook; t: ReturnType<typeof useT>["t"] }) {
  const importDataset = useDataNotebookStore((s) => s.importDataset);
  const clearDataset = useDataNotebookStore((s) => s.clearDataset);
  const importError = useDataNotebookStore((s) => s.importError);

  return (
    <section className="rounded-lg border border-border bg-background p-3">
      <div className="flex items-center justify-between gap-2">
        <h2 className="flex items-center gap-1.5 text-xs font-semibold uppercase tracking-wide text-faint">
          <FileSpreadsheet size={13} /> {t("DataNotebookPanel.datasetSection.title")}
        </h2>
        <div className="flex items-center gap-1.5">
          {notebook.dataset && (
            <Button variant="ghost" size="sm" onClick={() => clearDataset(notebook.id)}>
              {t("DataNotebookPanel.datasetSection.clear")}
            </Button>
          )}
          <Button variant="secondary" size="sm" onClick={() => void importDataset(notebook.id)}>
            <Upload size={12} /> {t("DataNotebookPanel.datasetSection.import")}
          </Button>
        </div>
      </div>
      <div className="mt-2 text-sm text-foreground">
        {notebook.dataset ? (
          t("DataNotebookPanel.datasetSection.summary", {
            name: notebook.dataset.name,
            rows: notebook.dataset.rowCount,
            table: notebook.dataset.tableName,
          })
        ) : (
          <span className="text-muted">{t("DataNotebookPanel.datasetSection.none")}</span>
        )}
      </div>
      {importError && (
        <p role="alert" className="mt-2 text-xs text-danger">
          {t("DataNotebookPanel.importError", { error: importError })}
        </p>
      )}
    </section>
  );
}

function NotebookEditor({ notebook, t }: { notebook: Notebook; t: ReturnType<typeof useT>["t"] }) {
  const renameNotebook = useDataNotebookStore((s) => s.renameNotebook);
  const addCell = useDataNotebookStore((s) => s.addCell);
  const runAll = useDataNotebookStore((s) => s.runAll);
  const runningNotebookId = useDataNotebookStore((s) => s.runningNotebookId);
  const exportReport = useDataNotebookStore((s) => s.exportReport);
  const [reportOpen, setReportOpen] = useState(false);
  const [copied, setCopied] = useState(false);
  const running = runningNotebookId === notebook.id;

  const handleCopyReport = async () => {
    const report = exportReport(notebook.id);
    if (!report) return;
    try {
      await navigator.clipboard.writeText(report);
      setCopied(true);
      setTimeout(() => setCopied(false), 1500);
    } catch {
      // Best-effort clipboard copy; the report text is still visible below.
    }
  };

  return (
    <div className="flex min-h-0 flex-1 flex-col gap-3 overflow-y-auto p-4">
      <input
        value={notebook.name}
        onChange={(e) => renameNotebook(notebook.id, e.target.value)}
        aria-label={t("DataNotebookPanel.renameNotebook")}
        className="w-full rounded-md border border-transparent bg-transparent px-1 text-lg font-semibold text-foreground outline-none hover:border-border focus:border-border-strong"
      />

      <DatasetSection notebook={notebook} t={t} />

      <div className="flex flex-wrap items-center gap-2">
        <Button variant="secondary" size="sm" onClick={() => addCell(notebook.id, "sql")}>
          <Plus size={12} /> {t("DataNotebookPanel.addSqlCell")}
        </Button>
        <Button variant="secondary" size="sm" onClick={() => addCell(notebook.id, "markdown")}>
          <Plus size={12} /> {t("DataNotebookPanel.addMarkdownCell")}
        </Button>
        <Button variant="primary" size="sm" onClick={() => void runAll(notebook.id)} disabled={running}>
          <RefreshCw size={12} className={running ? "animate-spin" : ""} />
          {running ? t("DataNotebookPanel.running") : t("DataNotebookPanel.runAll")}
        </Button>
        <Button variant="ghost" size="sm" onClick={() => setReportOpen((v) => !v)}>
          {t("DataNotebookPanel.exportReport")}
        </Button>
      </div>

      {reportOpen && (
        <section className="rounded-lg border border-border bg-background p-3">
          <div className="flex items-center justify-between gap-2">
            <h2 className="text-xs font-semibold uppercase tracking-wide text-faint">{t("DataNotebookPanel.reportModal.title")}</h2>
            <Button variant="ghost" size="sm" onClick={() => void handleCopyReport()}>
              <Copy size={12} /> {copied ? t("DataNotebookPanel.reportModal.copied") : t("DataNotebookPanel.reportModal.copy")}
            </Button>
          </div>
          <pre className="mt-2 max-h-64 overflow-auto whitespace-pre-wrap rounded-md bg-surface-2 p-2.5 font-mono text-xs text-foreground">
            {exportReport(notebook.id)}
          </pre>
        </section>
      )}

      <div className="flex flex-col gap-3">
        {notebook.cells.length === 0 ? (
          <p className="rounded-lg border border-dashed border-border p-4 text-center text-sm text-faint">
            {t("DataNotebookPanel.noCells")}
          </p>
        ) : (
          notebook.cells.map((cell, index) => (
            <CellCard key={cell.id} notebook={notebook} cell={cell} index={index} running={running} t={t} />
          ))
        )}
      </div>
    </div>
  );
}

export function DataNotebookPanel({ onClose }: DataNotebookPanelProps) {
  const { t } = useT();
  const notebooks = useDataNotebookStore((s) => s.notebooks);
  const activeNotebookId = useDataNotebookStore((s) => s.activeNotebookId);
  const setActiveNotebook = useDataNotebookStore((s) => s.setActiveNotebook);
  const createNotebook = useDataNotebookStore((s) => s.createNotebook);
  const deleteNotebook = useDataNotebookStore((s) => s.deleteNotebook);
  const persistError = useDataNotebookStore((s) => s.persistError);

  const activeNotebook = notebooks.find((n) => n.id === activeNotebookId) ?? null;

  return (
    <section className="flex min-h-0 flex-1 flex-col" aria-labelledby="data-notebook-title">
      <header className="flex shrink-0 items-center justify-between gap-3 border-b border-border px-4 py-3">
        <div className="min-w-0">
          <h1 id="data-notebook-title" className="text-base font-semibold text-foreground">
            {t("DataNotebookPanel.title")}
          </h1>
          <p className="truncate text-xs text-muted">{t("DataNotebookPanel.subtitle")}</p>
        </div>
        <IconButton size="sm" onClick={onClose} aria-label={t("DataNotebookPanel.close")}>
          <X size={16} />
        </IconButton>
      </header>

      {persistError && (
        <div role="alert" className="border-b border-danger/30 bg-danger-soft px-4 py-2 text-xs text-danger">
          {t("DataNotebookPanel.persistError", { error: persistError })}
        </div>
      )}

      <div className="flex min-h-0 flex-1">
        <aside className="flex w-56 shrink-0 flex-col border-r border-border">
          <div className="flex items-center justify-between gap-2 border-b border-border px-3 py-2">
            <span className="text-[11px] font-semibold uppercase tracking-wider text-faint">
              {t("DataNotebookPanel.notebookListTitle")}
            </span>
            <IconButton
              size="sm"
              aria-label={t("DataNotebookPanel.newNotebook")}
              onClick={() => createNotebook(t("DataNotebookPanel.defaultNotebookName"))}
            >
              <Plus size={14} />
            </IconButton>
          </div>
          <div className="min-h-0 flex-1 overflow-y-auto">
            {notebooks.length === 0 ? (
              <p className="p-3 text-xs text-faint">{t("DataNotebookPanel.emptyState")}</p>
            ) : (
              notebooks.map((notebook) => (
                <button
                  key={notebook.id}
                  type="button"
                  onClick={() => setActiveNotebook(notebook.id)}
                  className={`group flex w-full items-center justify-between gap-2 px-3 py-2 text-left text-sm hover:bg-surface-2 ${
                    notebook.id === activeNotebookId ? "bg-surface-2 text-foreground" : "text-muted"
                  }`}
                >
                  <span className="min-w-0 flex-1 truncate">{notebook.name}</span>
                  <span
                    role="button"
                    tabIndex={0}
                    aria-label={t("DataNotebookPanel.deleteNotebook")}
                    onClick={(e) => {
                      e.stopPropagation();
                      deleteNotebook(notebook.id);
                    }}
                    onKeyDown={(e) => {
                      if (e.key === "Enter" || e.key === " ") {
                        e.stopPropagation();
                        e.preventDefault();
                        deleteNotebook(notebook.id);
                      }
                    }}
                    className="shrink-0 rounded p-0.5 text-faint opacity-0 hover:text-danger group-hover:opacity-100"
                  >
                    <Trash2 size={13} />
                  </span>
                </button>
              ))
            )}
          </div>
        </aside>

        {activeNotebook ? (
          <NotebookEditor notebook={activeNotebook} t={t} />
        ) : (
          <div className="flex flex-1 items-center justify-center p-6">
            <p className="text-sm text-faint">{t("DataNotebookPanel.noActiveNotebook")}</p>
          </div>
        )}
      </div>
    </section>
  );
}

export default DataNotebookPanel;
