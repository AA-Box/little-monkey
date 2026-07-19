/**
 * Database Admin Guardrails (ROADMAP.md Phase 7, item 20) — English source
 * strings, spread into every locale's dictionary below with a REAL
 * translation per locale (see `de.ts`/`fr.ts`/etc.), matching this app's
 * normal per-locale i18n convention (`sopCompiler.ts` is the reference this
 * mirrors).
 */
export const dbAdminGuardrailsLocale: Record<string, string> = {
  "AppMenu.dbAdminGuardrails": "Database Admin Guardrails",
  "DbAdminGuardrails.title": "Database Admin Guardrails",
  "DbAdminGuardrails.subtitle": "Explore a local SQLite file's schema, propose SQL from a plain-language request, and require a dry run, a backup, and explicit approval before any write ever touches the file.",
  "DbAdminGuardrails.close": "Close Database Admin Guardrails",
  "DbAdminGuardrails.noFileOpen": "No database file open",
  "DbAdminGuardrails.openFileButton": "Open database file",
  "DbAdminGuardrails.openDifferentFileButton": "Open a different file",
  "DbAdminGuardrails.closeFileButton": "Close file",
  "DbAdminGuardrails.emptyState": "Open a local .sqlite/.db file to browse its schema and propose queries. Nothing about this connects to a live network database — it's a local file, opened read-write, with every write statement gated behind a dry run, a backup, and your explicit approval.",
  "DbAdminGuardrails.schemaHeading": "Schema",
  "DbAdminGuardrails.noTables": "This database has no user tables.",
  "DbAdminGuardrails.columnsCount": "{{count}} column(s)",
  "DbAdminGuardrails.piiColumnBadge": "PII",
  "DbAdminGuardrails.historyHeading": "Applied writes",
  "DbAdminGuardrails.historyRowsAffected": "{{count}} row(s) affected",
  "DbAdminGuardrails.requestLabel": "Describe the query or change you want",
  "DbAdminGuardrails.requestPlaceholder": "e.g. \"show me the 10 most recent orders\" or \"delete customers with no orders in the last year\"…",
  "DbAdminGuardrails.proposeButton": "Propose SQL",
  "DbAdminGuardrails.proposedSqlHeading": "Proposed SQL",
  "DbAdminGuardrails.selectKindBadge": "Read-only — runs immediately",
  "DbAdminGuardrails.writeKindBadge": "Write — approval required",
  "DbAdminGuardrails.unsupportedStatement": "Unsupported statement",
  "DbAdminGuardrails.unsupportedStatementHint": "This statement isn't recognized as a read or a write this tool can gate — try rephrasing your request.",
  "DbAdminGuardrails.resultsHeading": "Results",
  "DbAdminGuardrails.noRows": "No rows returned.",
  "DbAdminGuardrails.rowCountSuffix": "{{count}} row(s)",
  "DbAdminGuardrails.writeWarningBanner": "This is a write statement. It will not run for real until you preview a dry run, review the PII flags and rollback plan below, and explicitly approve it.",
  "DbAdminGuardrails.runDryRunButton": "Run dry-run preview",
  "DbAdminGuardrails.dryRunHeading": "Dry-run preview (rolled back — nothing was changed)",
  "DbAdminGuardrails.dryRunRowsAffected": "Would affect {{count}} row(s)",
  "DbAdminGuardrails.dryRunPiiHeading": "PII-flagged columns on the target table",
  "DbAdminGuardrails.dryRunNoPii": "No PII-shaped column names detected on the target table.",
  "DbAdminGuardrails.rollbackPlanHeading": "Rollback plan",
  "DbAdminGuardrails.rollbackPlanText": "Before this runs for real, a full copy of the original file is saved to {{path}} — restore that file to undo this change.",
  "DbAdminGuardrails.approveButton": "Approve write",
  "DbAdminGuardrails.confirmApproveText": "This will back up the file and apply the write for real. Are you sure?",
  "DbAdminGuardrails.confirmApproveButton": "Yes, apply now",
  "DbAdminGuardrails.cancelButton": "Cancel",
};
