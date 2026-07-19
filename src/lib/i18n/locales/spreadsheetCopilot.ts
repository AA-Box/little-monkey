/**
 * Spreadsheet Copilot (ROADMAP.md Phase 7, item 19) — English source
 * strings, spread into every locale's dictionary below with a REAL
 * translation per locale (see `de.ts`/`fr.ts`/etc.), matching this app's
 * per-locale i18n convention (`knowledgeGraphExplorer.ts` is the reference
 * this mirrors).
 */
export const spreadsheetCopilotLocale: Record<string, string> = {
  "AppMenu.spreadsheetCopilot": "Spreadsheet Copilot",
  "SpreadsheetCopilot.title": "Spreadsheet Copilot",
  "SpreadsheetCopilot.subtitle": "Load a CSV, describe a computed column, cleanup step, or summary, and review the exact cells it cites before approving the write.",
  "SpreadsheetCopilot.close": "Close Spreadsheet Copilot",
  "SpreadsheetCopilot.loadButton": "Open CSV…",
  "SpreadsheetCopilot.noFileHint": "Open a CSV file to get started.",
  "SpreadsheetCopilot.requestLabel": "Describe an operation",
  "SpreadsheetCopilot.requestPlaceholder": "e.g. Add a column that multiplies quantity by price",
  "SpreadsheetCopilot.proposeButton": "Propose",
  "SpreadsheetCopilot.noProposalHint": "Describe an operation and click Propose to see a diff-able change here — nothing is written to the file until you approve it.",
  "SpreadsheetCopilot.citedRangesHeading": "Cited cells / ranges",
  "SpreadsheetCopilot.diffHeading": "Cell changes ({{count}})",
  "SpreadsheetCopilot.diffNewCell": "(new cell)",
  "SpreadsheetCopilot.diffBlank": "(blank)",
  "SpreadsheetCopilot.approveWarning": "Approving writes this change to the CSV file on disk. Reject if the cited cells or values look wrong.",
  "SpreadsheetCopilot.rejectButton": "Reject",
  "SpreadsheetCopilot.approveButton": "Approve & write file",
  "SpreadsheetCopilot.emptyState": "Open a CSV file above to load it into the grid.",
  "SpreadsheetCopilot.gridChangedCell": "Changed by the pending proposal",
  "SpreadsheetCopilot.gridCitedCell": "Cited by the pending proposal",
  "SpreadsheetCopilot.gridEmptyHeader": "blank",
  "SpreadsheetCopilot.gridTruncated": "Showing the first {{shown}} of {{total}} rows.",
};
