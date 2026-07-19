/**
 * Data Notebook and SQL Lab (ROADMAP.md Phase 7, item 18) — English source
 * of truth for the `DataNotebookPanel.*` / `AppMenu.dataNotebook` key
 * namespace. Copied and spread into every other locale file, then every key
 * below is overridden there with a real translation (see de.ts/fr.ts/etc.)
 * — mirrors the structure every other feature locale slice in this
 * directory uses (see dailyBrief.ts).
 */
export const dataNotebookLocale: Record<string, string> = {
  "AppMenu.dataNotebook": "Data Notebook",
  "DataNotebookPanel.title": "Data Notebook and SQL Lab",
  "DataNotebookPanel.subtitle": "SQL and Markdown cells over a local CSV/JSON dataset, reproducible from saved source",
  "DataNotebookPanel.close": "Close Data Notebook",
  "DataNotebookPanel.notebookListTitle": "Notebooks",
  "DataNotebookPanel.newNotebook": "New notebook",
  "DataNotebookPanel.defaultNotebookName": "Untitled notebook",
  "DataNotebookPanel.emptyState": "No notebooks yet. Create one to get started.",
  "DataNotebookPanel.deleteNotebook": "Delete notebook",
  "DataNotebookPanel.renameNotebook": "Notebook name",
  "DataNotebookPanel.noActiveNotebook": "Select or create a notebook to get started.",
  "DataNotebookPanel.persistError": "Some changes may not be saved: {{error}}",
  "DataNotebookPanel.datasetSection.title": "Dataset",
  "DataNotebookPanel.datasetSection.none": "No dataset imported yet.",
  "DataNotebookPanel.datasetSection.import": "Import CSV/JSON…",
  "DataNotebookPanel.datasetSection.clear": "Remove dataset",
  "DataNotebookPanel.datasetSection.summary": "{{name}} · {{rows}} row(s) · table `{{table}}`",
  "DataNotebookPanel.importError": "Import failed: {{error}}",
  "DataNotebookPanel.addSqlCell": "Add SQL cell",
  "DataNotebookPanel.addMarkdownCell": "Add Markdown cell",
  "DataNotebookPanel.runAll": "Re-run all",
  "DataNotebookPanel.running": "Running…",
  "DataNotebookPanel.exportReport": "Export report",
  "DataNotebookPanel.noCells": "No cells yet. Add a SQL or Markdown cell to get started.",
  "DataNotebookPanel.cell.sqlLabel": "SQL",
  "DataNotebookPanel.cell.markdownLabel": "Markdown",
  "DataNotebookPanel.cell.run": "Run",
  "DataNotebookPanel.cell.moveUp": "Move cell up",
  "DataNotebookPanel.cell.moveDown": "Move cell down",
  "DataNotebookPanel.cell.delete": "Delete cell",
  "DataNotebookPanel.cell.sqlPlaceholder": "SELECT * FROM ...",
  "DataNotebookPanel.cell.markdownPlaceholder": "Write markdown…",
  "DataNotebookPanel.cell.markdownPreviewToggle": "Preview",
  "DataNotebookPanel.cell.markdownEditToggle": "Edit",
  "DataNotebookPanel.cell.notRun": "Not yet run.",
  "DataNotebookPanel.cell.error": "Error: {{error}}",
  "DataNotebookPanel.cell.rowsAffected": "{{count}} row(s) affected.",
  "DataNotebookPanel.cell.noResults": "No result set.",
  "DataNotebookPanel.cell.truncated": "Showing first {{shown}} of {{total}} rows.",
  "DataNotebookPanel.reportModal.title": "Reproducible report",
  "DataNotebookPanel.reportModal.copy": "Copy to clipboard",
  "DataNotebookPanel.reportModal.copied": "Copied!",
};
