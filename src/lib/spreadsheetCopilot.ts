/**
 * Spreadsheet Copilot (ROADMAP.md Phase 7, item 19) — MVP scope is CSV only,
 * parsed/written as plain text: no new dependency, no XLSX/Google Sheets
 * support. Those are explicitly out of scope for this MVP (XLSX needs a
 * workbook-parsing dependency, Google Sheets needs OAuth) — follow-ups, not
 * silently expanded scope.
 *
 * This module is pure TS (no React, no Tauri import) so it stays unit
 * testable without a WebView, mirroring `sopCompiler.ts`'s shape: a
 * `callModel` closure is injected by `spreadsheetCopilotStore.ts` (which
 * builds it around `agentLoop.ts`'s `resolveTarget` + `turnEngine.ts`'s
 * `attemptStream`, the same one-shot local-model-call pattern
 * `sopCompilerStore.ts` already uses for its own compiler call) rather than
 * importing either directly here.
 *
 * Core acceptance criterion this file exists to satisfy: "Spreadsheet
 * changes cite exact cells/ranges and ask before mutating live workbooks."
 * Every `SpreadsheetOperationProposal` this module produces carries a
 * non-empty `citedRanges` array (always includes the exact write range,
 * computed here rather than trusted from the model, plus whatever read
 * ranges the model cited and this module could validate against the actual
 * table bounds) and is returned as a diff-able `proposedTable` + `diff` —
 * never applied to a file itself. Only the store's `approve()` (driven by an
 * explicit user click in `SpreadsheetCopilotPanel.tsx`) ever writes the CSV
 * back to disk.
 */
import type { ChatMessage } from './llamaClient';
import { parseModelJsonCandidates } from './modelJson';

/** Caps how much of a table's data is sent to the model in one turn — a
 * bounded sample (header + first N rows) is enough for it to infer a
 * per-row transform, cleanup, or aggregate; the full table never needs to
 * round-trip through the model just to describe an operation. */
export const MAX_SAMPLE_ROWS = 25;

export interface SpreadsheetTable {
  headers: string[];
  /** Every row is padded/truncated to `headers.length` by `parseCsv` and by
   * `applyWrites` below, so callers can always index `rows[i][j]` safely. */
  rows: string[][];
}

export type SpreadsheetOperationKind = 'derived_column' | 'cleanup' | 'aggregate_summary';

/** A single cell write, A1-style. `row` is 1-indexed against the CSV's own
 * rows (row 1 = header row, row 2 = the first data row), matching how a
 * human reading the CSV alongside a spreadsheet app would point at it. */
export interface CellWrite {
  ref: string;
  value: string;
}

export interface CellDiffEntry {
  ref: string;
  /** `null` when the cell did not exist in the table before this operation
   * (a brand-new column or a newly appended row) — never fabricated as `""`
   * so the panel can render "new cell" distinctly from "cleared cell". */
  before: string | null;
  after: string;
}

export interface SpreadsheetOperationProposal {
  kind: SpreadsheetOperationKind;
  title: string;
  explanation: string;
  /** Always non-empty — see this file's top doc comment. Every entry is a
   * validated A1 cell or range string ("F2" or "B2:B11"). */
  citedRanges: string[];
  /** The full table AFTER applying this operation's writes — what
   * `SpreadsheetCopilotPanel.tsx` previews and, on approval, serializes back
   * to disk. Never mutates the `SpreadsheetTable` passed into
   * `proposeOperation`. */
  proposedTable: SpreadsheetTable;
  /** Cell-level diff vs. the table BEFORE this operation, in write order. */
  diff: CellDiffEntry[];
}

/** The minimal subset of `turnEngine.ts`'s `AttemptResult` this module needs
 * from `callModel` — dependency-injected rather than imported, exactly like
 * `sopCompiler.ts`'s `SopCompilerCallResult` / `riskJudge.ts`'s pattern, so
 * this file stays pure TS with no store/React import. */
export interface SpreadsheetCopilotCallResult {
  content: string;
  streamError: string | null;
}

// ---------------------------------------------------------------------------
// CSV parse / serialize (RFC 4126-ish: quoted fields, escaped quotes via "",
// commas/newlines inside quotes). No new dependency — this is intentionally
// small and scoped to what a CSV export from Excel/Sheets/Numbers produces.
// ---------------------------------------------------------------------------

/** Parses CSV text into rows of raw string fields (no header handling) —
 * shared by `parseCsv` (which splits the first row off as headers) and
 * available standalone for anything that wants raw rows. */
function parseCsvRows(text: string): string[][] {
  const rows: string[][] = [];
  let row: string[] = [];
  let field = '';
  let inQuotes = false;
  let i = 0;
  const len = text.length;

  const endField = () => {
    row.push(field);
    field = '';
  };
  const endRow = () => {
    endField();
    rows.push(row);
    row = [];
  };

  while (i < len) {
    const char = text[i];
    if (inQuotes) {
      if (char === '"') {
        if (text[i + 1] === '"') {
          field += '"';
          i += 2;
          continue;
        }
        inQuotes = false;
        i += 1;
        continue;
      }
      field += char;
      i += 1;
      continue;
    }
    if (char === '"') {
      inQuotes = true;
      i += 1;
      continue;
    }
    if (char === ',') {
      endField();
      i += 1;
      continue;
    }
    if (char === '\r') {
      // Swallow bare \r and \r\n alike — the following check for \n handles
      // the row break either way.
      i += 1;
      continue;
    }
    if (char === '\n') {
      endRow();
      i += 1;
      continue;
    }
    field += char;
    i += 1;
  }
  // Trailing field/row (a file not ending in a newline).
  if (field.length > 0 || row.length > 0) endRow();

  return rows;
}

/** Parses a full CSV document into a `SpreadsheetTable`. The first
 * non-skipped row becomes `headers`; every following row is padded/truncated
 * to that width so `rows[i][j]` is always safe to index. An empty/whitespace
 * source produces a single-column `{ headers: ["A"], rows: [] }` table
 * rather than throwing — an empty CSV is a valid (if trivial) starting
 * point for the copilot, not an error. */
export function parseCsv(text: string): SpreadsheetTable {
  if (!text.trim()) return { headers: ['A'], rows: [] };
  const rawRows = parseCsvRows(text).filter((row) => !(row.length === 1 && row[0] === ''));
  if (rawRows.length === 0) return { headers: ['A'], rows: [] };
  const [headerRow, ...dataRows] = rawRows;
  const width = Math.max(1, headerRow.length);
  const headers = padRow(headerRow, width);
  const rows = dataRows.map((row) => padRow(row, width));
  return { headers, rows };
}

function padRow(row: string[], width: number): string[] {
  if (row.length === width) return row.slice();
  const padded = row.slice(0, width);
  while (padded.length < width) padded.push('');
  return padded;
}

function csvEscapeField(value: string): string {
  if (/[",\n\r]/.test(value)) {
    return `"${value.replace(/"/g, '""')}"`;
  }
  return value;
}

/** Serializes a table back to CSV text, `\n`-terminated per row (including
 * the last), the same shape `writeTextFile` expects for a plain-text file. */
export function serializeCsv(table: SpreadsheetTable): string {
  const lines = [table.headers, ...table.rows].map((row) => row.map(csvEscapeField).join(','));
  return `${lines.join('\n')}\n`;
}

// ---------------------------------------------------------------------------
// A1-style cell/range references.
// ---------------------------------------------------------------------------

/** 0-indexed column number -> spreadsheet-style letters (0 -> "A", 25 -> "Z",
 * 26 -> "AA", …). */
export function columnLetters(index: number): string {
  let n = index + 1;
  let out = '';
  while (n > 0) {
    const rem = (n - 1) % 26;
    out = String.fromCharCode(65 + rem) + out;
    n = Math.floor((n - 1) / 26);
  }
  return out;
}

/** Spreadsheet-style letters -> 0-indexed column number, or `null` if not a
 * well-formed run of A-Z letters. */
export function columnIndex(letters: string): number | null {
  if (!/^[A-Za-z]+$/.test(letters)) return null;
  let n = 0;
  for (const char of letters.toUpperCase()) {
    n = n * 26 + (char.charCodeAt(0) - 64);
  }
  return n - 1;
}

/** Builds an A1-style ref for a table cell. `sheetRow` is 1-indexed
 * (row 1 = header), `col` is 0-indexed. */
export function cellRef(sheetRow: number, col: number): string {
  return `${columnLetters(col)}${sheetRow}`;
}

interface ParsedCellRef {
  sheetRow: number;
  col: number;
}

const CELL_REF_PATTERN = /^([A-Za-z]+)([1-9][0-9]*)$/;

/** Parses a single A1-style ref like "F2" — `null` for anything malformed
 * (no leading zero row numbers, no bare column, etc). */
export function parseCellRef(ref: string): ParsedCellRef | null {
  const match = CELL_REF_PATTERN.exec(ref.trim());
  if (!match) return null;
  const col = columnIndex(match[1]);
  if (col === null) return null;
  return { sheetRow: Number(match[2]), col };
}

/** Parses a range like "B2:B11" or a single cell "F1" (treated as a
 * zero-width range) — `null` if either side fails to parse as a cell ref. */
export function parseRangeRef(range: string): { start: ParsedCellRef; end: ParsedCellRef } | null {
  const [startRaw, endRaw] = range.split(':');
  const start = parseCellRef(startRaw ?? '');
  if (!start) return null;
  if (endRaw === undefined) return { start, end: start };
  const end = parseCellRef(endRaw);
  if (!end) return null;
  return { start, end };
}

/** Whether a parsed range's cells all fall within a table's current bounds
 * (header row 1 through `1 + table.rows.length`, columns 0 through
 * `table.headers.length - 1`) — used to reject a model-cited range that
 * points outside the sheet entirely rather than silently trusting it. */
export function rangeWithinTable(range: { start: ParsedCellRef; end: ParsedCellRef }, table: SpreadsheetTable): boolean {
  const maxRow = 1 + table.rows.length;
  const maxCol = table.headers.length - 1;
  const minRow = Math.min(range.start.sheetRow, range.end.sheetRow);
  const maxRowUsed = Math.max(range.start.sheetRow, range.end.sheetRow);
  const minCol = Math.min(range.start.col, range.end.col);
  const maxColUsed = Math.max(range.start.col, range.end.col);
  return minRow >= 1 && maxRowUsed <= maxRow && minCol >= 0 && maxColUsed <= maxCol;
}

/** Builds the smallest A1 range string covering every write ref — this is
 * the citation `proposeOperation` always appends itself (never trusting the
 * model to have cited its own write range correctly). Returns a single-cell
 * ref when there's exactly one write. */
export function boundingRange(refs: ParsedCellRef[]): string {
  const rows = refs.map((r) => r.sheetRow);
  const cols = refs.map((r) => r.col);
  const minRow = Math.min(...rows);
  const maxRow = Math.max(...rows);
  const minCol = Math.min(...cols);
  const maxCol = Math.max(...cols);
  const start = cellRef(minRow, minCol);
  const end = cellRef(maxRow, maxCol);
  return start === end ? start : `${start}:${end}`;
}

// ---------------------------------------------------------------------------
// Applying writes to produce a proposed table + diff.
// ---------------------------------------------------------------------------

/** Reads the current value at a ref against a table, or `null` if that cell
 * doesn't exist yet (new column/new row) — mirrors `CellDiffEntry.before`. */
function readCell(table: SpreadsheetTable, parsed: ParsedCellRef): string | null {
  if (parsed.sheetRow === 1) {
    return parsed.col < table.headers.length ? table.headers[parsed.col] : null;
  }
  const dataIndex = parsed.sheetRow - 2;
  if (dataIndex < 0 || dataIndex >= table.rows.length) return null;
  const row = table.rows[dataIndex];
  return parsed.col < row.length ? row[parsed.col] : null;
}

/**
 * Applies a list of cell writes to a table, returning a NEW table (the input
 * is never mutated) plus the cell-level diff. Writes may extend the table:
 * a column past the current width appends new (empty-until-written) columns
 * to every row, and a row past the current data extends `rows` with new
 * empty rows — this is how a `derived_column` operation adds a column and an
 * `aggregate_summary` operation can append a totals row.
 */
export function applyWrites(table: SpreadsheetTable, writes: CellWrite[]): { table: SpreadsheetTable; diff: CellDiffEntry[] } {
  // Snapshot of the table as it was BEFORE any write in this batch, used
  // only for `diff`'s `before` values — reading against the incrementally
  // mutated `headers`/`rows` below would report e.g. `""` instead of `null`
  // for a brand-new column's second write, once its first write has already
  // grown that column in on every row.
  const original: SpreadsheetTable = { headers: table.headers, rows: table.rows };
  let headers = table.headers.slice();
  let rows = table.rows.map((row) => row.slice());
  const diff: CellDiffEntry[] = [];

  for (const write of writes) {
    const parsed = parseCellRef(write.ref);
    if (!parsed) throw new Error(`Not a valid cell reference: "${write.ref}"`);
    const before = readCell(original, parsed);

    // Grow columns (on every existing row + the header) if this write is
    // past the current width.
    if (parsed.col >= headers.length) {
      const growBy = parsed.col + 1 - headers.length;
      headers = [...headers, ...Array.from({ length: growBy }, () => '')];
      rows = rows.map((row) => [...row, ...Array.from({ length: growBy }, () => '')]);
    }

    if (parsed.sheetRow === 1) {
      headers[parsed.col] = write.value;
    } else {
      const dataIndex = parsed.sheetRow - 2;
      if (dataIndex < 0) throw new Error(`Row ${parsed.sheetRow} is not a valid CSV row (rows start at 1).`);
      // Grow rows (appending blank ones) if this write is past the current
      // row count.
      while (rows.length <= dataIndex) {
        rows.push(Array.from({ length: headers.length }, () => ''));
      }
      // Widen this specific row if a prior write already grew the columns
      // after this row was appended blank at the old width.
      if (rows[dataIndex].length < headers.length) {
        rows[dataIndex] = [...rows[dataIndex], ...Array.from({ length: headers.length - rows[dataIndex].length }, () => '')];
      }
      rows[dataIndex][parsed.col] = write.value;
    }

    diff.push({ ref: write.ref, before, after: write.value });
  }

  // Final normalization pass: every row must match the final header width
  // (an earlier write could have grown columns after a later-indexed row
  // was already appended at the old width).
  rows = rows.map((row) => padRow(row, headers.length));

  return { table: { headers, rows }, diff };
}

// ---------------------------------------------------------------------------
// Model prompt + response parsing.
// ---------------------------------------------------------------------------

function tableSample(table: SpreadsheetTable): string {
  const headerLine = table.headers.map((h, i) => `${columnLetters(i)}="${h}"`).join(', ');
  const sampleRows = table.rows.slice(0, MAX_SAMPLE_ROWS);
  const rowLines = sampleRows.map((row, i) => {
    const sheetRow = i + 2;
    const cells = row.map((value, col) => `${cellRef(sheetRow, col)}=${JSON.stringify(value)}`).join(', ');
    return `Row ${sheetRow}: ${cells}`;
  });
  const truncatedNote = table.rows.length > MAX_SAMPLE_ROWS
    ? `\n(… ${table.rows.length - MAX_SAMPLE_ROWS} more data rows not shown)`
    : '';
  return [
    `Header row (row 1): ${headerLine}`,
    `Data rows: ${table.rows.length} total (row 2 through row ${table.rows.length + 1})`,
    rowLines.join('\n'),
  ].join('\n') + truncatedNote;
}

/** Builds the one-shot, tool-less operation-proposal prompt. Strict-JSON-only,
 * mirroring `sopCompiler.ts`'s `buildSopCompilerMessages`. */
export function buildSpreadsheetCopilotMessages(table: SpreadsheetTable, instruction: string, sourceLabel?: string): ChatMessage[] {
  return [
    {
      role: 'system',
      content: [
        'You are a spreadsheet copilot working on a CSV file loaded as a table with A1-style cell references (row 1 is the header row, column letters start at A). You NEVER modify the file yourself — you only propose ONE operation at a time for a human to review and approve.',
        'Supported operation kinds: "derived_column" (add a new computed column from existing columns), "cleanup" (fix specific existing cells — typos, inconsistent casing, blank/malformed values), "aggregate_summary" (write one or a few summary cells, e.g. a totals/average row appended after the last data row, or a single summary cell).',
        'Every cell you write must be given as an explicit A1 ref ("F2") and a plain string value — compute the actual value yourself (this app does not evaluate spreadsheet formula syntax), do not write a "=SUM(...)" formula string.',
        'You must also list "citedReadRanges": the exact A1 cell(s)/range(s) you actually read from to justify this operation (e.g. reading quantity and price columns to compute a total cites both of those columns\' data ranges). Never leave this empty.',
        'Reply with ONLY a single-line JSON object of this exact shape, no markdown, no other text:',
        '{"kind":"derived_column|cleanup|aggregate_summary","title":"...","explanation":"...","citedReadRanges":["B2:B11","C2:C11"],"writes":[{"ref":"D1","value":"Total"},{"ref":"D2","value":"19.98"}]}',
        '"writes" must be non-empty. For "derived_column", include a header write (row 1) for the new column plus one write per existing data row. For "cleanup", only include the specific cells that actually need to change. For "aggregate_summary", write to a new row after the last data row (or an existing summary cell if the user asked to update one).',
      ].join(' '),
    },
    {
      role: 'user',
      content: `${sourceLabel ? `Source file: ${sourceLabel}\n\n` : ''}${tableSample(table)}\n\nRequested operation: ${instruction.trim()}`,
    },
  ];
}

function asNonEmptyString(value: unknown): string | null {
  return typeof value === 'string' && value.trim().length > 0 ? value.trim() : null;
}

function asOperationKind(value: unknown): SpreadsheetOperationKind | null {
  return value === 'derived_column' || value === 'cleanup' || value === 'aggregate_summary' ? value : null;
}

/**
 * Strict parse + validation of the copilot's reply against the table it was
 * asked about, mirroring `sopCompiler.ts`'s `parseSopCompilerResponse`: tries
 * the raw trimmed content first, then complete embedded JSON objects.
 * Returns `null` on anything malformed, on an empty/invalid `writes` list, or
 * when every cited read range fails validation — callers must fail closed
 * (surface an error to the user), never fabricate a proposal from a bad
 * response or apply writes that don't parse as real cell refs.
 */
export function parseSpreadsheetCopilotResponse(content: string, table: SpreadsheetTable): SpreadsheetOperationProposal | null {
  for (const record of parseModelJsonCandidates(content, 'object')) {
    const kind = asOperationKind(record.kind);
    const title = asNonEmptyString(record.title);
    if (!kind || !title) continue;
    const explanation = asNonEmptyString(record.explanation) ?? '';

    const rawWrites = Array.isArray(record.writes) ? record.writes : [];
    const writes: CellWrite[] = [];
    let writesValid = rawWrites.length > 0;
    for (const entry of rawWrites) {
      const ref = asNonEmptyString((entry as { ref?: unknown })?.ref);
      const rawValue = (entry as { value?: unknown })?.value;
      const value = typeof rawValue === 'string' ? rawValue : typeof rawValue === 'number' ? String(rawValue) : null;
      if (!ref || value === null || !parseCellRef(ref)) {
        writesValid = false;
        break;
      }
      writes.push({ ref, value });
    }
    if (!writesValid) continue;

    let applied: { table: SpreadsheetTable; diff: CellDiffEntry[] };
    try {
      applied = applyWrites(table, writes);
    } catch {
      continue;
    }

    const writeRefs = writes.map((w) => parseCellRef(w.ref)).filter((r): r is ParsedCellRef => r !== null);
    const writeRange = boundingRange(writeRefs);

    const rawReadRanges = Array.isArray(record.citedReadRanges) ? record.citedReadRanges : [];
    const validatedReadRanges = rawReadRanges
      .filter((entry): entry is string => typeof entry === 'string' && entry.trim().length > 0)
      .map((entry) => entry.trim())
      .filter((entry) => {
        const range = parseRangeRef(entry);
        return range !== null && rangeWithinTable(range, table);
      });

    const citedRanges = Array.from(new Set([...validatedReadRanges, writeRange]));

    return {
      kind,
      title,
      explanation,
      citedRanges,
      proposedTable: applied.table,
      diff: applied.diff,
    };
  }
  return null;
}

/**
 * Runs the one-shot, non-streaming, tool-less proposal call and returns a
 * validated `SpreadsheetOperationProposal`. Like `sopCompiler.ts`'s
 * `compileSop`, a failure here is surfaced to the user as a real error —
 * nothing about "the model didn't return a usable proposal" is silently
 * swallowed.
 */
export async function proposeOperation(
  table: SpreadsheetTable,
  instruction: string,
  callModel: (messages: ChatMessage[], signal?: AbortSignal) => Promise<SpreadsheetCopilotCallResult>,
  sourceLabel?: string,
  signal?: AbortSignal,
): Promise<SpreadsheetOperationProposal> {
  if (!instruction.trim()) {
    throw new Error('Describe the operation you want (a computed column, a cleanup step, or a summary) before proposing it.');
  }
  const result = await callModel(buildSpreadsheetCopilotMessages(table, instruction, sourceLabel), signal);
  if (result.streamError) {
    throw new Error(result.streamError);
  }
  const proposal = parseSpreadsheetCopilotResponse(result.content, table);
  if (!proposal) {
    throw new Error('The model did not return a usable spreadsheet operation (with valid cell references and cited ranges). Try again, or rephrase the request.');
  }
  return proposal;
}
