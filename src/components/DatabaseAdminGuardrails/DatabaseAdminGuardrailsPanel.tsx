import { useState } from "react";
import {
  AlertTriangle,
  Database,
  FolderOpen,
  History,
  KeyRound,
  Loader2,
  Play,
  ShieldCheck,
  Sparkles,
  X,
  XCircle,
} from "lucide-react";

import { useT } from "../../lib/i18n";
import { useDbAdminGuardrailsStore } from "../../store/dbAdminGuardrailsStore";
import { Button, IconButton, StatusPill } from "../ui";
import { formatBytes } from "../../lib/format";

interface DatabaseAdminGuardrailsPanelProps {
  onClose: () => void;
}


function formatCellValue(value: unknown): string {
  if (value === null || value === undefined) return "NULL";
  if (value instanceof Uint8Array) return `<${value.byteLength} bytes>`;
  return String(value);
}

export function DatabaseAdminGuardrailsPanel({ onClose }: DatabaseAdminGuardrailsPanelProps) {
  const { t } = useT();
  const store = useDbAdminGuardrailsStore();
  const [applyConfirmOpen, setApplyConfirmOpen] = useState(false);

  const piiTableNames = new Set(
    store.tables.filter((table) => table.columns.some((col) => col.pii)).map((table) => table.name),
  );

  return (
    <section className="flex h-full min-h-0 flex-col" aria-labelledby="db-admin-guardrails-title">
      <header className="flex shrink-0 items-start justify-between gap-3 border-b border-border px-5 py-4">
        <div>
          <h2 id="db-admin-guardrails-title" className="text-sm font-semibold text-foreground">
            {t("DbAdminGuardrails.title")}
          </h2>
          <p className="mt-1 max-w-2xl text-xs leading-5 text-muted">{t("DbAdminGuardrails.subtitle")}</p>
        </div>
        <IconButton
          size="sm"
          aria-label={t("DbAdminGuardrails.close")}
          title={t("DbAdminGuardrails.close")}
          onClick={onClose}
        >
          <X size={15} />
        </IconButton>
      </header>

      <div className="flex shrink-0 items-center justify-between gap-2 border-b border-border px-5 py-2.5">
        <div className="flex min-w-0 items-center gap-2">
          <Database size={14} className="shrink-0 text-faint" />
          {store.fileName ? (
            <>
              <span className="truncate font-mono text-xs text-foreground">{store.fileName}</span>
              {store.fileSizeBytes !== null && (
                <span className="shrink-0 text-[11px] text-faint">{formatBytes(store.fileSizeBytes)}</span>
              )}
            </>
          ) : (
            <span className="text-xs text-faint">{t("DbAdminGuardrails.noFileOpen")}</span>
          )}
        </div>
        <div className="flex shrink-0 items-center gap-2">
          {store.fileName && (
            <Button size="sm" variant="ghost" onClick={() => store.closeFile()}>
              {t("DbAdminGuardrails.closeFileButton")}
            </Button>
          )}
          <Button size="sm" disabled={store.loadingFile} onClick={() => void store.openFile()}>
            {store.loadingFile ? <Loader2 className="animate-spin" size={13} /> : <FolderOpen size={13} />}
            {store.fileName ? t("DbAdminGuardrails.openDifferentFileButton") : t("DbAdminGuardrails.openFileButton")}
          </Button>
        </div>
      </div>

      {!store.fileName ? (
        <div className="flex flex-1 items-center justify-center p-8">
          <p className="max-w-md text-center text-xs leading-5 text-faint">{t("DbAdminGuardrails.emptyState")}</p>
        </div>
      ) : (
        <div className="grid min-h-0 flex-1 gap-4 overflow-hidden p-5 xl:grid-cols-[minmax(14rem,.8fr)_minmax(0,1.4fr)]">
          {/* Schema browser */}
          <div className="min-h-0 overflow-y-auto rounded-lg border border-border bg-surface p-3">
            <h3 className="text-xs font-semibold text-foreground">{t("DbAdminGuardrails.schemaHeading")}</h3>
            <div className="mt-2 space-y-2">
              {store.tables.length === 0 && (
                <p className="rounded-md border border-dashed border-border p-5 text-center text-xs text-faint">
                  {t("DbAdminGuardrails.noTables")}
                </p>
              )}
              {store.tables.map((table) => (
                <div key={table.name} className="rounded-md border border-border bg-background p-2.5">
                  <div className="flex items-center justify-between gap-2">
                    <p className="truncate font-mono text-xs font-medium text-foreground">{table.name}</p>
                    {piiTableNames.has(table.name) && (
                      <StatusPill tone="warning">{t("DbAdminGuardrails.piiColumnBadge")}</StatusPill>
                    )}
                  </div>
                  <p className="mt-0.5 text-[11px] text-faint">
                    {t("DbAdminGuardrails.columnsCount", { count: table.columns.length })}
                  </p>
                  <ul className="mt-1.5 space-y-0.5">
                    {table.columns.map((col) => (
                      <li key={col.name} className="flex items-center gap-1.5 font-mono text-[11px] text-muted">
                        {col.primaryKey && <KeyRound size={10} className="shrink-0 text-faint" />}
                        <span className="truncate">{col.name}</span>
                        <span className="shrink-0 text-faint">{col.type || "?"}</span>
                        {col.pii && (
                          <span className="shrink-0 rounded bg-warning/15 px-1 text-[9px] font-semibold uppercase tracking-wide text-warning">
                            {t("DbAdminGuardrails.piiColumnBadge")}
                          </span>
                        )}
                      </li>
                    ))}
                  </ul>
                </div>
              ))}
            </div>

            {store.history.length > 0 && (
              <div className="mt-4">
                <h3 className="flex items-center gap-1.5 text-xs font-semibold text-foreground">
                  <History size={13} /> {t("DbAdminGuardrails.historyHeading")}
                </h3>
                <div className="mt-2 space-y-1.5">
                  {store.history.map((entry) => (
                    <div key={entry.id} className="rounded-md border border-border bg-background p-2 text-[11px]">
                      <p className="truncate font-mono text-foreground">{entry.sql}</p>
                      <p className="mt-0.5 text-faint">
                        {t("DbAdminGuardrails.historyRowsAffected", { count: entry.rowsAffected })} ·{" "}
                        {new Date(entry.appliedAt).toLocaleString()}
                      </p>
                      <p className="mt-0.5 truncate font-mono text-faint">{entry.backupPath}</p>
                    </div>
                  ))}
                </div>
              </div>
            )}
          </div>

          {/* Request + proposal + results/dry-run */}
          <div className="min-h-0 overflow-y-auto rounded-lg border border-border bg-surface p-4">
            <div className="space-y-2">
              <label htmlFor="db-admin-nl-request" className="text-xs font-medium text-foreground">
                {t("DbAdminGuardrails.requestLabel")}
              </label>
              <textarea
                id="db-admin-nl-request"
                className="h-20 w-full resize-y rounded-md border border-border bg-background px-2.5 py-2 text-xs text-foreground outline-none focus:border-accent focus:ring-1 focus:ring-accent"
                placeholder={t("DbAdminGuardrails.requestPlaceholder")}
                value={store.nlRequest}
                onChange={(event) => store.setNlRequest(event.target.value)}
              />
              <div className="flex justify-end">
                <Button
                  variant="primary"
                  size="sm"
                  disabled={store.proposing || !store.nlRequest.trim()}
                  onClick={() => void store.propose()}
                >
                  {store.proposing ? <Loader2 className="animate-spin" size={13} /> : <Sparkles size={13} />}
                  {t("DbAdminGuardrails.proposeButton")}
                </Button>
              </div>
              {store.proposalError && (
                <div role="alert" className="rounded-md border border-danger/40 bg-danger/10 p-2.5 text-xs text-danger">
                  {store.proposalError}
                </div>
              )}
            </div>

            {store.proposedSql && (
              <div className="mt-4 space-y-3 border-t border-border pt-4">
                <div>
                  <div className="flex items-center justify-between gap-2">
                    <h4 className="text-xs font-semibold text-foreground">{t("DbAdminGuardrails.proposedSqlHeading")}</h4>
                    <StatusPill tone={store.statementKind === "select" ? "neutral" : store.statementKind === "write" ? "warning" : "danger"}>
                      {store.statementKind === "select"
                        ? t("DbAdminGuardrails.selectKindBadge")
                        : store.statementKind === "write"
                          ? t("DbAdminGuardrails.writeKindBadge")
                          : t("DbAdminGuardrails.unsupportedStatement")}
                    </StatusPill>
                  </div>
                  <pre className="mt-1.5 overflow-x-auto rounded-md border border-border bg-background p-2.5 font-mono text-[11px] text-foreground">
                    {store.proposedSql}
                  </pre>
                  {store.proposalExplanation && (
                    <p className="mt-1.5 text-[11px] text-muted">{store.proposalExplanation}</p>
                  )}
                </div>

                {store.statementKind === "select" && store.selectResult && (
                  <div>
                    <h4 className="text-xs font-semibold text-foreground">{t("DbAdminGuardrails.resultsHeading")}</h4>
                    {store.selectResult.rows.length === 0 ? (
                      <p className="mt-2 rounded-md border border-dashed border-border p-3 text-center text-[11px] text-faint">
                        {t("DbAdminGuardrails.noRows")}
                      </p>
                    ) : (
                      <div className="mt-2 max-h-72 overflow-auto rounded-md border border-border">
                        <table className="w-full border-collapse text-[11px]">
                          <thead className="sticky top-0 bg-surface-2">
                            <tr>
                              {store.selectResult.columns.map((col) => (
                                <th key={col} className="border-b border-border px-2 py-1 text-left font-mono font-medium text-foreground">
                                  {col}
                                </th>
                              ))}
                            </tr>
                          </thead>
                          <tbody>
                            {store.selectResult.rows.map((row, rowIndex) => (
                              <tr key={rowIndex} className="odd:bg-background even:bg-surface">
                                {row.map((cell, cellIndex) => (
                                  <td key={cellIndex} className="border-b border-border px-2 py-1 font-mono text-muted">
                                    {formatCellValue(cell)}
                                  </td>
                                ))}
                              </tr>
                            ))}
                          </tbody>
                        </table>
                      </div>
                    )}
                    <p className="mt-1.5 text-[11px] text-faint">
                      {t("DbAdminGuardrails.rowCountSuffix", { count: store.selectResult.rows.length })}
                    </p>
                  </div>
                )}

                {store.statementKind === "unsupported" && (
                  <div className="flex items-start gap-2 rounded-md border border-danger/40 bg-danger/10 p-2.5 text-xs text-danger">
                    <XCircle size={14} className="mt-0.5 shrink-0" />
                    <p>{t("DbAdminGuardrails.unsupportedStatementHint")}</p>
                  </div>
                )}

                {store.statementKind === "write" && (
                  <div className="space-y-3 rounded-md border border-warning/40 bg-warning/5 p-3">
                    <div className="flex items-start gap-2 text-xs text-warning">
                      <AlertTriangle size={14} className="mt-0.5 shrink-0" />
                      <p>{t("DbAdminGuardrails.writeWarningBanner")}</p>
                    </div>

                    {!store.dryRun ? (
                      <Button size="sm" disabled={store.dryRunning} onClick={() => void store.runDryRun()}>
                        {store.dryRunning ? <Loader2 className="animate-spin" size={13} /> : <Play size={13} />}
                        {t("DbAdminGuardrails.runDryRunButton")}
                      </Button>
                    ) : (
                      <div className="space-y-2 rounded-md border border-border bg-background p-2.5">
                        <h5 className="text-[11px] font-semibold text-foreground">{t("DbAdminGuardrails.dryRunHeading")}</h5>
                        <p className="text-[11px] text-foreground">
                          {t("DbAdminGuardrails.dryRunRowsAffected", { count: store.dryRun.rowsAffected })}
                        </p>
                        <div>
                          <p className="text-[11px] font-medium text-foreground">{t("DbAdminGuardrails.dryRunPiiHeading")}</p>
                          {store.dryRun.piiColumns.length === 0 ? (
                            <p className="text-[11px] text-faint">{t("DbAdminGuardrails.dryRunNoPii")}</p>
                          ) : (
                            <div className="mt-1 flex flex-wrap gap-1">
                              {store.dryRun.piiColumns.map((col) => (
                                <span
                                  key={col}
                                  className="rounded bg-warning/15 px-1.5 py-0.5 font-mono text-[10px] font-semibold text-warning"
                                >
                                  {col}
                                </span>
                              ))}
                            </div>
                          )}
                        </div>
                        <div>
                          <p className="text-[11px] font-medium text-foreground">{t("DbAdminGuardrails.rollbackPlanHeading")}</p>
                          <p className="mt-0.5 text-[11px] text-muted">
                            {t("DbAdminGuardrails.rollbackPlanText", { path: `${store.filePath ?? ""}.bak-…` })}
                          </p>
                        </div>
                      </div>
                    )}

                    {store.dryRunError && (
                      <div role="alert" className="rounded-md border border-danger/40 bg-danger/10 p-2.5 text-xs text-danger">
                        {store.dryRunError}
                      </div>
                    )}
                    {store.applyError && (
                      <div role="alert" className="rounded-md border border-danger/40 bg-danger/10 p-2.5 text-xs text-danger">
                        {store.applyError}
                      </div>
                    )}

                    <div className="flex flex-wrap items-center gap-2">
                      {store.dryRun && !applyConfirmOpen && (
                        <Button variant="primary" size="sm" onClick={() => setApplyConfirmOpen(true)}>
                          <ShieldCheck size={13} /> {t("DbAdminGuardrails.approveButton")}
                        </Button>
                      )}
                      {store.dryRun && applyConfirmOpen && (
                        <>
                          <span className="text-[11px] text-foreground">{t("DbAdminGuardrails.confirmApproveText")}</span>
                          <Button
                            variant="primary"
                            size="sm"
                            disabled={store.applying}
                            onClick={() => {
                              setApplyConfirmOpen(false);
                              void store.approveApply();
                            }}
                          >
                            {store.applying ? <Loader2 className="animate-spin" size={13} /> : <ShieldCheck size={13} />}
                            {t("DbAdminGuardrails.confirmApproveButton")}
                          </Button>
                          <Button size="sm" variant="ghost" onClick={() => setApplyConfirmOpen(false)}>
                            {t("DbAdminGuardrails.cancelButton")}
                          </Button>
                        </>
                      )}
                      {!applyConfirmOpen && (
                        <Button
                          size="sm"
                          variant="ghost"
                          onClick={() => {
                            setApplyConfirmOpen(false);
                            store.cancelProposal();
                          }}
                        >
                          {t("DbAdminGuardrails.cancelButton")}
                        </Button>
                      )}
                    </div>
                  </div>
                )}
              </div>
            )}
          </div>
        </div>
      )}
    </section>
  );
}

export default DatabaseAdminGuardrailsPanel;
