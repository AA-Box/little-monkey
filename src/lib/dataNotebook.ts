/**
 * Data Notebook and SQL Lab (ROADMAP.md Phase 7, item 18) — core notebook
 * data model and execution engine. Pure TypeScript, no React, no Tauri
 * imports: every function here is deterministic given its inputs, which is
 * exactly what the acceptance criterion needs ("a data analysis can be
 * reproduced from saved cells, inputs, environment, and outputs") —
 * `runAllCells` re-seeds an in-memory SQLite database from the notebook's
 * saved `dataset.raw` text and re-runs every cell's saved `source` in order,
 * so a saved notebook reproduces its own saved outputs byte-for-byte as long
 * as the SQL doesn't call a non-deterministic function (`RANDOM()`,
 * `date('now')`, ...) — a documented, reasonable limitation for this MVP.
 *
 * MVP scope (see ROADMAP.md's Phase 7 guidance): SQL cells (via the sql.js
 * WASM SQLite engine, seeded by importing a local CSV/JSON file) and
 * Markdown cells only. Python/R cells are explicitly OUT of scope — real
 * execution needs a sandboxing foundation this app doesn't have yet — and
 * are not faked anywhere in this module.
 *
 * The one dataset table any given notebook can seed is always named after
 * `dataset.tableName`; multiple imports simply replace it (`DROP TABLE IF
 * EXISTS` in `buildCreateTableSql`), keeping the seeding step idempotent.
 */
import initSqlJs, { type Database, type SqlJsStatic, type SqlValue } from "sql.js";
// Vite's `?url` suffix resolves this to a fetchable asset URL in the actual
// app (dev server and production build both handle it — see sql.js's own
// Vite integration docs). Under Vitest's node environment this resolves to
// a path that doesn't correspond to a real file, which is fine: `loadSqlEngine`
// below only passes it through `locateFile` in a browser (`window` defined)
// context — in Node, sql.js's own `require`+`__dirname` resolution already
// finds the real `.wasm` file next to `sql-wasm.js` without any help.
import sqlWasmUrl from "sql.js/dist/sql-wasm.wasm?url";
import { errorMessage } from "./errors";

export type NotebookCellType = "sql" | "markdown";
export type SqlColumnType = "INTEGER" | "REAL" | "TEXT";
export type DatasetFormat = "csv" | "json";

/** Raised by the CSV/JSON parsers for input that can't be turned into a table. */
export class DatasetParseError extends Error {}

/** A parsed, ready-to-load table: columns, inferred SQLite types, and rows
 * already coerced to matching JS values. Purely a parsing-time
 * intermediate — never persisted itself (the notebook persists `dataset.raw`
 * and re-derives this on every seed, which is what makes re-runs reproducible
 * from the saved source rather than from a frozen snapshot). */
export interface DatasetTable {
  tableName: string;
  columns: string[];
  columnTypes: SqlColumnType[];
  rows: SqlValue[][];
}

/** The imported dataset a notebook's SQL cells run against. `raw` is the
 * original file text verbatim — the "input" half of the reproducibility
 * story (the other half is each cell's saved `source`). */
export interface Dataset {
  /** Original file name, e.g. "orders.csv" — display only. */
  name: string;
  /** Sanitized SQL identifier the file is loaded into, e.g. `orders`. */
  tableName: string;
  format: DatasetFormat;
  raw: string;
  importedAt: number;
  rowCount: number;
  columns: string[];
}

export interface NotebookCellOutput {
  columns: string[];
  /** Row sample, capped at `MAX_OUTPUT_ROWS` so a huge result set can't blow
   * up localStorage persistence. */
  rows: SqlValue[][];
  /** The true result-set size, even when `rows` was capped. */
  rowCount: number;
  /** True when `rows` is a prefix of a larger result set. */
  truncated: boolean;
  /** Rows inserted/updated/deleted by the statement (0 for a plain SELECT). */
  rowsAffected: number;
}

export interface NotebookCell {
  id: string;
  type: NotebookCellType;
  source: string;
  output: NotebookCellOutput | null;
  error: string | null;
  /** ms timestamp of the last run attempt (success or failure); null if never run. */
  lastRunAt: number | null;
}

export interface Notebook {
  id: string;
  name: string;
  createdAt: number;
  updatedAt: number;
  dataset: Dataset | null;
  cells: NotebookCell[];
}

/** Row-sample cap for any single cell's persisted output — keeps the
 * localStorage-persisted notebook bounded regardless of how large the
 * imported dataset is. */
export const MAX_OUTPUT_ROWS = 500;

// ---------------------------------------------------------------------------
// CSV parsing
// ---------------------------------------------------------------------------

/** RFC4180-ish CSV tokenizer: quoted fields (with escaped `""`), commas and
 * newlines inside quotes, and both `\n`/`\r\n` line endings. Deliberately
 * hand-rolled rather than a dependency — this app's MVP scope is "CSV/JSON
 * only", not a general-purpose CSV toolkit. */
function parseCsvRows(text: string): string[][] {
  const rows: string[][] = [];
  let row: string[] = [];
  let field = "";
  let inQuotes = false;
  let i = 0;
  const len = text.length;
  let sawAnyField = false;

  while (i < len) {
    const ch = text[i];
    if (inQuotes) {
      if (ch === '"') {
        if (text[i + 1] === '"') {
          field += '"';
          i += 2;
          continue;
        }
        inQuotes = false;
        i += 1;
        continue;
      }
      field += ch;
      i += 1;
      continue;
    }
    if (ch === '"') {
      inQuotes = true;
      sawAnyField = true;
      i += 1;
      continue;
    }
    if (ch === ",") {
      row.push(field);
      field = "";
      sawAnyField = true;
      i += 1;
      continue;
    }
    if (ch === "\r") {
      i += 1;
      continue;
    }
    if (ch === "\n") {
      row.push(field);
      rows.push(row);
      row = [];
      field = "";
      sawAnyField = false;
      i += 1;
      continue;
    }
    field += ch;
    sawAnyField = true;
    i += 1;
  }
  if (sawAnyField || field.length > 0 || row.length > 0) {
    row.push(field);
    rows.push(row);
  }
  return rows;
}

function inferCsvColumnType(values: (string | undefined)[]): SqlColumnType {
  let sawReal = false;
  for (const raw of values) {
    if (raw === undefined || raw === "") continue;
    if (/^-?\d+$/.test(raw)) continue;
    if (/^-?\d*\.\d+$/.test(raw) || /^-?\d+\.\d*$/.test(raw)) {
      sawReal = true;
      continue;
    }
    return "TEXT";
  }
  return sawReal ? "REAL" : "INTEGER";
}

function coerceCsvValue(raw: string | undefined, type: SqlColumnType): SqlValue {
  if (raw === undefined || raw === "") return null;
  if (type === "INTEGER") return Number.parseInt(raw, 10);
  if (type === "REAL") return Number.parseFloat(raw);
  return raw;
}

/** Parses CSV text into a loadable table. The first row is always treated
 * as the header. Throws `DatasetParseError` for empty input. */
export function parseCsvDataset(raw: string, tableName: string): DatasetTable {
  const rawRows = parseCsvRows(raw).filter((r) => !(r.length === 1 && r[0].trim() === ""));
  if (rawRows.length === 0) {
    throw new DatasetParseError("The CSV file is empty.");
  }
  const [header, ...dataRows] = rawRows;
  const columns = header.map((h, i) => (h.trim().length > 0 ? h.trim() : `column_${i + 1}`));
  const columnTypes = columns.map((_, colIndex) => inferCsvColumnType(dataRows.map((r) => r[colIndex])));
  const rows: SqlValue[][] = dataRows.map((r) => columns.map((_, colIndex) => coerceCsvValue(r[colIndex], columnTypes[colIndex])));
  return { tableName, columns, columnTypes, rows };
}

// ---------------------------------------------------------------------------
// JSON parsing
// ---------------------------------------------------------------------------

function inferJsonColumnType(values: unknown[]): SqlColumnType {
  let sawReal = false;
  for (const value of values) {
    if (value === undefined || value === null) continue;
    if (typeof value === "number") {
      if (!Number.isInteger(value)) sawReal = true;
      continue;
    }
    if (typeof value === "boolean") continue; // stored as 0/1
    return "TEXT";
  }
  return sawReal ? "REAL" : "INTEGER";
}

function coerceJsonValue(value: unknown, type: SqlColumnType): SqlValue {
  if (value === undefined || value === null) return null;
  if (typeof value === "boolean") return value ? 1 : 0;
  if (type === "TEXT") return typeof value === "string" ? value : JSON.stringify(value);
  if (typeof value === "number") return value;
  const asNumber = Number(value);
  return Number.isNaN(asNumber) ? String(value) : asNumber;
}

/** Parses a JSON array of flat records into a loadable table. Columns are
 * the union of every record's keys, in first-seen order; a record missing a
 * key gets `null` for that column. Throws `DatasetParseError` for anything
 * that isn't a non-empty array of objects. */
export function parseJsonDataset(raw: string, tableName: string): DatasetTable {
  let parsed: unknown;
  try {
    parsed = JSON.parse(raw);
  } catch {
    throw new DatasetParseError("That file isn't valid JSON.");
  }
  if (!Array.isArray(parsed)) {
    throw new DatasetParseError("Only a JSON array of records is supported.");
  }
  const records = parsed.filter((r): r is Record<string, unknown> => !!r && typeof r === "object" && !Array.isArray(r));
  if (records.length === 0) {
    throw new DatasetParseError("The JSON file has no importable records.");
  }
  const columns: string[] = [];
  const seen = new Set<string>();
  for (const record of records) {
    for (const key of Object.keys(record)) {
      if (!seen.has(key)) {
        seen.add(key);
        columns.push(key);
      }
    }
  }
  const columnTypes = columns.map((col) => inferJsonColumnType(records.map((r) => r[col])));
  const rows: SqlValue[][] = records.map((record) => columns.map((col, i) => coerceJsonValue(record[col], columnTypes[i])));
  return { tableName, columns, columnTypes, rows };
}

/** Parses `dataset.raw` back into a `DatasetTable`, dispatching on
 * `dataset.format`. Always re-derives from the raw text (never trusts a
 * cached table) — that re-derivation on every seed is exactly what makes
 * `runAllCells` reproducible from the saved source. */
export function parseDataset(dataset: Dataset): DatasetTable {
  return dataset.format === "csv" ? parseCsvDataset(dataset.raw, dataset.tableName) : parseJsonDataset(dataset.raw, dataset.tableName);
}

/** Derives a valid, sanitized SQL table identifier from an imported file's
 * name: strips the extension and any directory, lowercases, collapses
 * non-alphanumerics to underscores, and guarantees a non-numeric leading
 * character (SQLite identifiers may start with a digit only when quoted;
 * this keeps unquoted references simple everywhere else in the app). */
export function sanitizeTableName(filename: string): string {
  const base = filename.replace(/\.[^./\\]+$/, "").split(/[\\/]/).pop() ?? "data";
  let ident = base.toLowerCase().replace(/[^a-z0-9_]+/g, "_").replace(/^_+|_+$/g, "");
  if (ident.length === 0) ident = "data";
  if (/^[0-9]/.test(ident)) ident = `t_${ident}`;
  return ident.slice(0, 64);
}

function quoteIdent(name: string): string {
  return `"${name.replace(/"/g, '""')}"`;
}

function buildCreateTableSql(table: DatasetTable): string {
  const cols = table.columns.map((c, i) => `${quoteIdent(c)} ${table.columnTypes[i]}`).join(", ");
  return `DROP TABLE IF EXISTS ${quoteIdent(table.tableName)}; CREATE TABLE ${quoteIdent(table.tableName)} (${cols});`;
}

function buildInsertSql(table: DatasetTable): string {
  const cols = table.columns.map(quoteIdent).join(", ");
  const placeholders = table.columns.map(() => "?").join(", ");
  return `INSERT INTO ${quoteIdent(table.tableName)} (${cols}) VALUES (${placeholders});`;
}

// ---------------------------------------------------------------------------
// sql.js engine
// ---------------------------------------------------------------------------

let enginePromise: Promise<SqlJsStatic> | null = null;

/** Lazily loads (and caches) the sql.js WASM engine. Only passes
 * `locateFile` in a browser context — see the `sqlWasmUrl` import comment
 * above for why Node/Vitest must NOT get that override. */
export function loadSqlEngine(): Promise<SqlJsStatic> {
  if (!enginePromise) {
    enginePromise = initSqlJs(typeof window === "undefined" ? undefined : { locateFile: () => sqlWasmUrl as string });
  }
  return enginePromise;
}

/** Creates a fresh in-memory SQLite database, optionally seeded with
 * `dataset` re-parsed from its saved raw text. Callers own the returned
 * `Database` and must `.close()` it when done. */
export async function createSeededDatabase(dataset: Dataset | null): Promise<Database> {
  const SQL = await loadSqlEngine();
  const db = new SQL.Database();
  if (dataset) {
    const table = parseDataset(dataset);
    db.run(buildCreateTableSql(table));
    const insertSql = buildInsertSql(table);
    for (const row of table.rows) {
      db.run(insertSql, row);
    }
  }
  return db;
}

/** Builds the `Dataset` metadata record for a freshly-imported file — does
 * not touch a database, just parses+summarizes so the caller can persist it
 * on a notebook. Throws `DatasetParseError` (via `parseCsvDataset`/
 * `parseJsonDataset`) for unparsable input. */
export function buildDataset(fileName: string, raw: string, format: DatasetFormat): Dataset {
  const tableName = sanitizeTableName(fileName);
  const table = format === "csv" ? parseCsvDataset(raw, tableName) : parseJsonDataset(raw, tableName);
  return {
    name: fileName,
    tableName,
    format,
    raw,
    importedAt: Date.now(),
    rowCount: table.rows.length,
    columns: table.columns,
  };
}

/** Runs `source` (one or more `;`-separated SQL statements) against `db` and
 * returns the last statement's result set (if any), capped at
 * `MAX_OUTPUT_ROWS`. Throws whatever `db.exec` throws for invalid SQL —
 * callers catch this and record it as the cell's `error`. */
export function runSqlStatements(db: Database, source: string): NotebookCellOutput {
  const trimmed = source.trim();
  if (!trimmed) {
    return { columns: [], rows: [], rowCount: 0, truncated: false, rowsAffected: 0 };
  }
  const results = db.exec(trimmed);
  const last = results[results.length - 1];
  const columns = last?.columns ?? [];
  const allRows = last?.values ?? [];
  return {
    columns,
    rows: allRows.slice(0, MAX_OUTPUT_ROWS),
    rowCount: allRows.length,
    truncated: allRows.length > MAX_OUTPUT_ROWS,
    rowsAffected: db.getRowsModified(),
  };
}

function messageOf(err: unknown): string {
  return errorMessage(err);
}

// ---------------------------------------------------------------------------
// Notebook execution
// ---------------------------------------------------------------------------

/**
 * Re-seeds a fresh database from `notebook.dataset` and re-runs every cell
 * up to and including `cellId`, in saved order — "restart & run to here"
 * semantics, the simplest model that stays correct across cell reordering
 * and edits without keeping a long-lived mutable database around. Cells
 * after `cellId` are left untouched. If a cell errors, cells after it (up to
 * and including `cellId`) are left untouched too, since their preconditions
 * are no longer trustworthy.
 */
export async function runCellsUpTo(notebook: Notebook, cellId: string): Promise<Notebook> {
  const index = notebook.cells.findIndex((c) => c.id === cellId);
  if (index === -1) {
    throw new Error(`Cell not found: ${cellId}`);
  }
  const db = await createSeededDatabase(notebook.dataset);
  try {
    const cells = [...notebook.cells];
    for (let i = 0; i <= index; i++) {
      const cell = cells[i];
      if (cell.type === "markdown") {
        cells[i] = { ...cell, output: null, error: null, lastRunAt: Date.now() };
        continue;
      }
      try {
        const output = runSqlStatements(db, cell.source);
        cells[i] = { ...cell, output, error: null, lastRunAt: Date.now() };
      } catch (err) {
        cells[i] = { ...cell, output: null, error: messageOf(err), lastRunAt: Date.now() };
        break;
      }
    }
    return { ...notebook, cells, updatedAt: Date.now() };
  } finally {
    db.close();
  }
}

/** Re-runs every cell in the notebook, in order — the "Re-run all" action,
 * and the operation the acceptance criterion means by "reproduced from
 * saved cells, inputs, environment, and outputs": call this on a freshly
 * loaded (e.g. just-hydrated-from-localStorage) notebook and it reproduces
 * every cell's saved output from nothing but `dataset.raw` and each cell's
 * `source`. */
export async function runAllCells(notebook: Notebook): Promise<Notebook> {
  if (notebook.cells.length === 0) return { ...notebook, updatedAt: Date.now() };
  return runCellsUpTo(notebook, notebook.cells[notebook.cells.length - 1].id);
}

// ---------------------------------------------------------------------------
// Reproducible report export
// ---------------------------------------------------------------------------

function renderMarkdownTable(output: NotebookCellOutput): string {
  if (output.columns.length === 0) {
    return output.rowsAffected > 0 ? `_${output.rowsAffected} row(s) affected._` : "_No result set._";
  }
  const header = `| ${output.columns.join(" | ")} |`;
  const divider = `| ${output.columns.map(() => "---").join(" | ")} |`;
  const rows = output.rows.map((row) => `| ${row.map((v) => (v === null || v === undefined ? "" : String(v))).join(" | ")} |`);
  const lines = [header, divider, ...rows];
  if (output.truncated) {
    lines.push("", `_Showing first ${output.rows.length} of ${output.rowCount} rows._`);
  }
  return lines.join("\n");
}

/**
 * Renders a notebook (cells + last-run outputs) as a single self-contained
 * Markdown report — the "generate reproducible reports" half of the
 * acceptance criterion. Anyone who re-imports the same dataset file and
 * re-runs the same SQL will get the same tables shown here; the report
 * itself is just this notebook's saved state rendered as prose+tables, not a
 * separate mechanism.
 */
export function exportNotebookAsMarkdownReport(notebook: Notebook): string {
  const lines: string[] = [`# ${notebook.name}`, "", `Generated ${new Date().toISOString()}`, ""];
  if (notebook.dataset) {
    lines.push(
      `**Dataset:** ${notebook.dataset.name} (${notebook.dataset.format.toUpperCase()}, ${notebook.dataset.rowCount} row(s), table \`${notebook.dataset.tableName}\`)`,
      "",
    );
  }
  for (const cell of notebook.cells) {
    if (cell.type === "markdown") {
      lines.push(cell.source, "");
      continue;
    }
    lines.push("```sql", cell.source, "```", "");
    if (cell.error) {
      lines.push(`> **Error:** ${cell.error}`, "");
    } else if (cell.output) {
      lines.push(renderMarkdownTable(cell.output), "");
    } else {
      lines.push("_Not yet run._", "");
    }
  }
  return lines.join("\n");
}

// ---------------------------------------------------------------------------
// Cell/notebook construction helpers
// ---------------------------------------------------------------------------

export function createEmptyCell(type: NotebookCellType): NotebookCell {
  return {
    id: crypto.randomUUID(),
    type,
    source: "",
    output: null,
    error: null,
    lastRunAt: null,
  };
}

export function createNotebookModel(name: string): Notebook {
  const now = Date.now();
  return {
    id: crypto.randomUUID(),
    name,
    createdAt: now,
    updatedAt: now,
    dataset: null,
    cells: [],
  };
}
