/**
 * Database Admin Guardrails (ROADMAP.md Phase 7, item 20) — explores the
 * schema of a LOCAL SQLite file (opened read-write via the existing native
 * file-open dialog, never a live network connection to a production
 * database), turns a natural-language request into a proposed SQL statement
 * using the existing local-model-call pattern, and gates every WRITE
 * statement (INSERT/UPDATE/DELETE/ALTER/DROP/CREATE/REPLACE/TRUNCATE) behind
 * a dry run (a wrapped transaction that always rolls back, so the preview
 * never actually mutates the file), a heuristic PII-column flag on the
 * statement's target table, and a rollback plan (a full copy of the original
 * file taken before any real write — see `dbAdminGuardrailsStore.ts`'s
 * `approveApply`) — nothing here ever runs a write statement for real
 * without all three plus an explicit user approval.
 *
 * Uses the `sql.js` WASM SQLite engine so the whole file loads into memory
 * and every statement (read or write) runs synchronously against that
 * in-memory copy; the store is the only thing that ever persists a change
 * back to disk (via `db.export()` + `writeFile`).
 *
 * Model-facing shape mirrors `sopCompiler.ts`/`riskJudge.ts`'s
 * dependency-injection pattern (a `callModel` closure passed in, not
 * `attemptStream` imported directly) so this file stays pure TS with no
 * store/React import — `dbAdminGuardrailsStore.ts` builds the closure around
 * `agentLoop.ts`'s `resolveTarget` and `turnEngine.ts`'s `attemptStream`,
 * exactly like `sopCompilerStore.ts`'s `compile` does for `compileSop`.
 */
import initSqlJs, { type Database, type SqlJsStatic } from 'sql.js';
import type { ChatMessage } from './llamaClient';

// ---------------------------------------------------------------------------
// Engine bootstrap
// ---------------------------------------------------------------------------

let sqlJsPromise: Promise<SqlJsStatic> | null = null;

/**
 * Loads the sql.js WASM engine exactly once per process. In a browser/webview
 * (this app's real runtime) the WASM binary is served as a static asset from
 * `public/sql-wasm.wasm` (copied there at build time from `sql.js/dist`,
 * since Vite bundling moves the JS glue away from the file sql.js's own
 * default relative-fetch would otherwise look next to) — `locateFile` points
 * at that fixed absolute path. Under Vitest's `node` test environment
 * `window`/`document` don't exist, so this falls back to sql.js's own
 * default Node loading (reads the WASM straight off disk via `fs`), which
 * needs no `locateFile` override at all.
 */
function loadSqlJs(): Promise<SqlJsStatic> {
  if (!sqlJsPromise) {
    const isBrowserRuntime = typeof window !== 'undefined' && typeof document !== 'undefined';
    sqlJsPromise = initSqlJs(isBrowserRuntime ? { locateFile: () => '/sql-wasm.wasm' } : undefined);
  }
  return sqlJsPromise;
}

/** Opens a SQLite file's raw bytes as an in-memory `sql.js` `Database`. */
export async function openDatabaseFromBytes(bytes: Uint8Array): Promise<Database> {
  const SQL = await loadSqlJs();
  return new SQL.Database(bytes);
}

// ---------------------------------------------------------------------------
// Schema introspection
// ---------------------------------------------------------------------------

export interface ColumnInfo {
  name: string;
  type: string;
  notNull: boolean;
  primaryKey: boolean;
  /** Heuristic-only flag (see `isPiiColumnName`) — a column NAME match, not
   * content inspection. Always label this as a heuristic to the user; it can
   * both over-flag (e.g. `product_name`) and under-flag (an oddly-named PII
   * column) and is not a substitute for a real data-classification pass. */
  pii: boolean;
}

export interface TableInfo {
  name: string;
  columns: ColumnInfo[];
}

/** Double-quotes a SQLite identifier for safe interpolation into `PRAGMA`
 * statements (which don't support `?` parameter binding for identifiers),
 * escaping any embedded `"` per SQLite's own quoting rule. */
function quoteIdent(name: string): string {
  return `"${name.replace(/"/g, '""')}"`;
}

/** Column-name keywords that heuristically suggest personally identifiable
 * information — matched as whole "words" within an underscore/space/case
 * normalized column name (so `first_name` and `firstName` both match `name`,
 * but this stays a heuristic: it never inspects actual row data). */
const PII_KEYWORDS = [
  'name',
  'email',
  'mail',
  'phone',
  'mobile',
  'ssn',
  'social_security',
  'passport',
  'national_id',
  'credit_card',
  'card_number',
  'cvv',
  'password',
  'passwd',
  'secret',
  'dob',
  'birth',
  'address',
  'street',
  'zip',
  'postal',
  'gender',
  'iban',
  'account_number',
];

function normalizeColumnName(name: string): string {
  return name
    .replace(/([a-z0-9])([A-Z])/g, '$1_$2')
    .toLowerCase()
    .replace(/[^a-z0-9]+/g, '_');
}

export function isPiiColumnName(columnName: string): boolean {
  const normalized = `_${normalizeColumnName(columnName)}_`;
  return PII_KEYWORDS.some((keyword) => normalized.includes(`_${keyword}_`) || normalized.includes(`_${keyword}s_`));
}

/** Introspects every user table (skipping SQLite's own `sqlite_%` internal
 * tables) via `sqlite_master` + `PRAGMA table_info`. Read-only — never
 * mutates `db`. */
export function introspectSchema(db: Database): TableInfo[] {
  const tables: TableInfo[] = [];
  const tableRes = db.exec(
    "SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%' ORDER BY name",
  );
  const tableNames = (tableRes[0]?.values ?? []).map((row) => String(row[0]));

  for (const tableName of tableNames) {
    const pragma = db.exec(`PRAGMA table_info(${quoteIdent(tableName)})`);
    const columns: ColumnInfo[] = (pragma[0]?.values ?? []).map((row) => {
      // PRAGMA table_info columns: cid, name, type, notnull, dflt_value, pk
      const name = String(row[1]);
      return {
        name,
        type: String(row[2] ?? ''),
        notNull: Number(row[3]) === 1,
        primaryKey: Number(row[5] ?? 0) > 0,
        pii: isPiiColumnName(name),
      };
    });
    tables.push({ name: tableName, columns });
  }
  return tables;
}

export function buildSchemaSummary(tables: TableInfo[]): string {
  if (tables.length === 0) return '(no user tables)';
  return tables
    .map((table) => {
      const cols = table.columns
        .map((col) => `${col.name} ${col.type || 'UNKNOWN'}${col.primaryKey ? ' PK' : ''}${col.pii ? ' [possible PII]' : ''}`)
        .join(', ');
      return `${table.name}(${cols})`;
    })
    .join('\n');
}

// ---------------------------------------------------------------------------
// Statement classification
// ---------------------------------------------------------------------------

export type StatementKind = 'select' | 'write' | 'unsupported';

const WRITE_KEYWORDS = new Set(['insert', 'update', 'delete', 'alter', 'drop', 'create', 'replace', 'truncate']);
const SELECT_KEYWORDS = new Set(['select', 'with', 'pragma', 'explain']);

/** Strips a single trailing `;` (and any surrounding whitespace) — proposals
 * are expected to be exactly one statement; see `isSingleStatement`. */
function stripTrailingSemicolon(sql: string): string {
  return sql.trim().replace(/;+\s*$/, '');
}

export function classifyStatement(sql: string): StatementKind {
  const first = stripTrailingSemicolon(sql).trim().split(/\s+/)[0]?.toLowerCase() ?? '';
  if (SELECT_KEYWORDS.has(first)) return 'select';
  if (WRITE_KEYWORDS.has(first)) return 'write';
  return 'unsupported';
}

/**
 * Rejects a proposal that smuggles more than one statement behind a
 * semicolon (e.g. a harmless-looking `SELECT ...; DROP TABLE ...`) — a naive
 * check (split on `;` outside of nothing fancier), but sufficient for an
 * MVP gate since every downstream dry-run/apply call only ever runs `sql`
 * verbatim through `db.run`/`db.exec` once.
 */
export function isSingleStatement(sql: string): boolean {
  const withoutTrailing = stripTrailingSemicolon(sql);
  const segments = withoutTrailing.split(';').map((s) => s.trim()).filter(Boolean);
  return segments.length <= 1;
}

const TARGET_TABLE_PATTERNS: RegExp[] = [
  /insert\s+(?:or\s+\w+\s+)?into\s+["'`]?([A-Za-z0-9_]+)["'`]?/i,
  /update\s+["'`]?([A-Za-z0-9_]+)["'`]?/i,
  /delete\s+from\s+["'`]?([A-Za-z0-9_]+)["'`]?/i,
  /alter\s+table\s+["'`]?([A-Za-z0-9_]+)["'`]?/i,
  /drop\s+table\s+(?:if\s+exists\s+)?["'`]?([A-Za-z0-9_]+)["'`]?/i,
  /create\s+table\s+(?:if\s+not\s+exists\s+)?["'`]?([A-Za-z0-9_]+)["'`]?/i,
  /replace\s+into\s+["'`]?([A-Za-z0-9_]+)["'`]?/i,
  /truncate\s+(?:table\s+)?["'`]?([A-Za-z0-9_]+)["'`]?/i,
];

/** Naive best-effort extraction of the single table a write statement
 * targets — used only to look up that table's PII-flagged columns for the
 * dry-run preview, never to validate or rewrite the SQL itself. */
export function extractTargetTable(sql: string): string | null {
  const cleaned = stripTrailingSemicolon(sql);
  for (const pattern of TARGET_TABLE_PATTERNS) {
    const match = cleaned.match(pattern);
    if (match) return match[1];
  }
  return null;
}

// ---------------------------------------------------------------------------
// Read path — SELECT runs immediately, no gate
// ---------------------------------------------------------------------------

export interface QueryResult {
  columns: string[];
  rows: unknown[][];
}

export function runSelect(db: Database, sql: string): QueryResult {
  const res = db.exec(sql);
  if (res.length === 0) return { columns: [], rows: [] };
  return { columns: res[0].columns, rows: res[0].values };
}

// ---------------------------------------------------------------------------
// Write path — dry run (always rolled back) + real apply
// ---------------------------------------------------------------------------

export interface DryRunResult {
  rowsAffected: number;
  targetTable: string | null;
  piiColumns: string[];
}

/**
 * Runs `sql` inside `BEGIN ... ROLLBACK` and reports what it WOULD have
 * changed — SQLite supports transactional DDL, so this rolls back
 * ALTER/DROP/CREATE just as cleanly as INSERT/UPDATE/DELETE. The database is
 * left byte-for-byte as it was before this call returns (rollback runs in a
 * `finally` so a mid-statement error still reverts it).
 */
export function dryRunWrite(db: Database, sql: string, tables: TableInfo[]): DryRunResult {
  const targetTable = extractTargetTable(sql);
  const tableInfo = targetTable
    ? tables.find((t) => t.name.toLowerCase() === targetTable.toLowerCase())
    : undefined;
  const piiColumns = tableInfo ? tableInfo.columns.filter((c) => c.pii).map((c) => c.name) : [];

  db.run('BEGIN');
  let rowsAffected = 0;
  try {
    db.run(sql);
    rowsAffected = db.getRowsModified();
  } finally {
    db.run('ROLLBACK');
  }
  return { rowsAffected, targetTable, piiColumns };
}

/**
 * Runs `sql` for real inside `BEGIN ... COMMIT` (rolling back instead on
 * error, so a failing write never leaves the in-memory database
 * half-mutated). The caller (`dbAdminGuardrailsStore.ts`'s `approveApply`)
 * is responsible for taking a backup copy of the ORIGINAL file and gating
 * this behind an explicit approval — this function has no knowledge of
 * dry-runs, backups, or approvals, it only ever executes exactly the
 * statement it's given.
 */
export function applyWrite(db: Database, sql: string): number {
  db.run('BEGIN');
  try {
    db.run(sql);
    const rowsAffected = db.getRowsModified();
    db.run('COMMIT');
    return rowsAffected;
  } catch (err) {
    db.run('ROLLBACK');
    throw err;
  }
}

/** Suggests a sibling backup filename next to the original path — the
 * store copies the ORIGINAL (pre-write) file bytes here before ever calling
 * `applyWrite`, so this is the rollback plan shown in the approval gate and
 * the file a user restores from if a real write turns out to be wrong. */
export function suggestBackupPath(originalPath: string): string {
  const stamp = new Date().toISOString().replace(/[:.]/g, '-');
  return `${originalPath}.bak-${stamp}`;
}

// ---------------------------------------------------------------------------
// Natural-language -> SQL proposal (model call, dependency-injected)
// ---------------------------------------------------------------------------

export interface SqlProposal {
  sql: string;
  explanation: string;
}

/** The minimal subset of `turnEngine.ts`'s `AttemptResult` this module needs
 * from `callModel` — same shape as `sopCompiler.ts`'s `SopCompilerCallResult`. */
export interface DbProposalCallResult {
  content: string;
  streamError: string | null;
}

/** Caps how much schema text is sent to the model in one turn — generous for
 * a normal app database, bounded so a huge schema can't blow up a local
 * model's context window. */
export const MAX_SCHEMA_SUMMARY_CHARS = 8_000;

export function buildProposalMessages(schemaSummary: string, request: string): ChatMessage[] {
  const truncatedSchema =
    schemaSummary.length > MAX_SCHEMA_SUMMARY_CHARS
      ? `${schemaSummary.slice(0, MAX_SCHEMA_SUMMARY_CHARS)}…`
      : schemaSummary;
  return [
    {
      role: 'system',
      content: [
        'You propose a single SQLite SQL statement for a database administration tool that gates every write behind a human approval — you never execute anything yourself.',
        'You are given the exact table/column schema of a local SQLite file (columns marked "[possible PII]" are a heuristic-only guess based on column NAME, not actual data) and a natural-language request.',
        'Reply with ONLY a single-line JSON object of this exact shape, no markdown, no other text:',
        '{"sql":"...","explanation":"..."}',
        'The "sql" value must be exactly ONE SQLite statement (no semicolon-separated multi-statement scripts), using only tables/columns that actually appear in the given schema — never invent a table or column name.',
        'The "explanation" value is a short, plain-language sentence describing what the statement does and, if it writes data, what would change.',
        'If the request is ambiguous or cannot be expressed as a single valid SQLite statement against this schema, propose the closest safe read-only SELECT that helps the user investigate instead, and say why in the explanation.',
      ].join(' '),
    },
    {
      role: 'user',
      content: `Schema:\n${truncatedSchema}\n\nRequest: ${request.trim()}`,
    },
  ];
}

function asNonEmptyString(value: unknown): string | null {
  return typeof value === 'string' && value.trim().length > 0 ? value.trim() : null;
}

/** Strict parse of the proposal reply, mirroring `sopCompiler.ts`'s
 * `parseSopCompilerResponse`: tries the raw trimmed content first, then
 * falls back to the first `{...}` span (small local models sometimes wrap
 * otherwise valid JSON in a sentence or code fence). Returns `null` on
 * anything malformed. */
export function parseProposalResponse(content: string): SqlProposal | null {
  const candidates = [content.trim()];
  const embedded = content.match(/\{[\s\S]*\}/);
  if (embedded) candidates.push(embedded[0]);

  for (const candidate of candidates) {
    let parsed: unknown;
    try {
      parsed = JSON.parse(candidate);
    } catch {
      continue;
    }
    if (!parsed || typeof parsed !== 'object') continue;
    const record = parsed as Record<string, unknown>;
    const sql = asNonEmptyString(record.sql);
    const explanation = asNonEmptyString(record.explanation) ?? '';
    if (!sql) continue;
    return { sql, explanation };
  }
  return null;
}

/**
 * Runs the one-shot, non-streaming, tool-less proposal call and returns a
 * validated `SqlProposal`. Fails closed with a real error (never silently
 * falls back to fabricating a statement) — the same posture as
 * `sopCompiler.ts`'s `compileSop`.
 */
export async function proposeSql(
  schemaSummary: string,
  request: string,
  callModel: (messages: ChatMessage[], signal?: AbortSignal) => Promise<DbProposalCallResult>,
  signal?: AbortSignal,
): Promise<SqlProposal> {
  if (!request.trim()) {
    throw new Error('Describe the query or change you want before proposing SQL.');
  }
  const result = await callModel(buildProposalMessages(schemaSummary, request), signal);
  if (result.streamError) {
    throw new Error(result.streamError);
  }
  const proposal = parseProposalResponse(result.content);
  if (!proposal) {
    throw new Error('The model did not return a usable SQL proposal. Try rephrasing your request.');
  }
  if (!isSingleStatement(proposal.sql)) {
    throw new Error('Only a single SQL statement is supported — the model proposed more than one.');
  }
  return proposal;
}
