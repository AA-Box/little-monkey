/**
 * Database Admin Guardrails (ROADMAP.md Phase 7, item 20) — owns the
 * currently-open LOCAL SQLite file (opened read-write via the native
 * file-open dialog), its introspected schema, the natural-language ->
 * proposed-SQL flow (built around `agentLoop.ts`'s `resolveTarget` and
 * `turnEngine.ts`'s `attemptStream`, exactly like `sopCompilerStore.ts`'s
 * `compile` does for `compileSop`), and the dry-run/backup/approve gate for
 * every write statement.
 *
 * The live `sql.js` `Database` handle itself is NOT store state — zustand
 * state is meant to be plain/serializable-ish and re-rendered on every
 * change, neither of which fits a stateful WASM object — it lives in a
 * module-level variable (`currentDb`) alongside the original file's raw
 * bytes size, mirroring how `terminalStore`-style modules keep a live
 * process handle out of their observable state. Everything the UI actually
 * renders (file name/path, schema, proposal, dry-run preview, history) IS
 * plain store state.
 *
 * Acceptance gate this store enforces end-to-end: a write statement can only
 * reach `applyWrite` (real, committed) after `proposeSql` classified it as a
 * write, `runDryRun` produced a rolled-back preview (row estimate + PII
 * columns) for THIS exact statement, and `approveApply` has taken a full
 * backup copy of the original file — skipping straight from `propose` to
 * `approveApply` without an intervening dry run is rejected defensively (see
 * `approveApply`'s guard), and `cancelProposal`/opening a new file/editing
 * the request text all clear the dry run so a stale preview can never be
 * approved against a since-changed statement.
 */
import { create } from "zustand";
import { open } from "@tauri-apps/plugin-dialog";
import { readFile, writeFile, copyFile, stat } from "@tauri-apps/plugin-fs";
import type { Database } from "sql.js";

import { resolveTarget } from "../lib/agentLoop";
import { attemptStream } from "../lib/turnEngine";
import type { ChatMessage } from "../lib/llamaClient";
import { effortForTarget } from "./modelStore";
import {
  applyWrite,
  buildSchemaSummary,
  classifyStatement,
  dryRunWrite,
  introspectSchema,
  openDatabaseFromBytes,
  proposeSql,
  runSelect,
  suggestBackupPath,
  type DbProposalCallResult,
  type DryRunResult,
  type QueryResult,
  type StatementKind,
  type TableInfo,
} from "../lib/dbAdminGuardrails";

/** Fixed pseudo-session id for this feature's one-shot model calls — mirrors
 * `sopCompilerStore.ts`'s `SOP_COMPILER_SESSION_ID`; `recordUsage: false`
 * below means `attemptStream` never writes anything into `useUsageStore`
 * under it. */
const DB_ADMIN_SESSION_ID = "db-admin-guardrails";

/** Reject opening a file above this size outright — sql.js loads the whole
 * file into memory, so this keeps the feature to the "local SQLite file"
 * scope it was designed for rather than a multi-GB production dump. */
const MAX_DB_FILE_BYTES = 200 * 1024 * 1024;

export interface AppliedWriteEntry {
  id: string;
  sql: string;
  rowsAffected: number;
  backupPath: string;
  appliedAt: number;
}

interface DbAdminGuardrailsState {
  filePath: string | null;
  fileName: string | null;
  fileSizeBytes: number | null;
  loadingFile: boolean;
  tables: TableInfo[];

  nlRequest: string;
  proposing: boolean;
  proposalError: string | null;
  proposedSql: string | null;
  proposalExplanation: string | null;
  statementKind: StatementKind | null;

  selectResult: QueryResult | null;
  runningSelect: boolean;

  dryRun: DryRunResult | null;
  dryRunning: boolean;
  dryRunError: string | null;

  applying: boolean;
  applyError: string | null;
  lastBackupPath: string | null;
  history: AppliedWriteEntry[];

  openFile: () => Promise<void>;
  closeFile: () => void;
  setNlRequest: (text: string) => void;
  propose: () => Promise<void>;
  runDryRun: () => Promise<void>;
  approveApply: () => Promise<void>;
  cancelProposal: () => void;
}

let currentDb: Database | null = null;

function resetDbHandle() {
  try {
    currentDb?.close();
  } catch {
    // Already closed/never opened — nothing to clean up.
  }
  currentDb = null;
}

const initialProposalFields = {
  proposing: false,
  proposalError: null as string | null,
  proposedSql: null as string | null,
  proposalExplanation: null as string | null,
  statementKind: null as StatementKind | null,
  selectResult: null as QueryResult | null,
  runningSelect: false,
  dryRun: null as DryRunResult | null,
  dryRunning: false,
  dryRunError: null as string | null,
  applying: false,
  applyError: null as string | null,
};

export const useDbAdminGuardrailsStore = create<DbAdminGuardrailsState>((set, get) => ({
  filePath: null,
  fileName: null,
  fileSizeBytes: null,
  loadingFile: false,
  tables: [],

  nlRequest: "",
  ...initialProposalFields,

  lastBackupPath: null,
  history: [],

  openFile: async () => {
    set({ loadingFile: true, proposalError: null });
    try {
      const selected = await open({
        title: "Open a local SQLite database file",
        multiple: false,
        directory: false,
        filters: [{ name: "SQLite database", extensions: ["sqlite", "sqlite3", "db"] }],
      });
      if (typeof selected !== "string") {
        set({ loadingFile: false });
        return;
      }
      const info = await stat(selected);
      if (info.size > MAX_DB_FILE_BYTES) {
        throw new Error(
          `That file is larger than ${Math.floor(MAX_DB_FILE_BYTES / (1024 * 1024))}MB — this tool is scoped to local development/admin databases, not multi-GB production dumps.`,
        );
      }
      const bytes = await readFile(selected);
      resetDbHandle();
      currentDb = await openDatabaseFromBytes(bytes);
      const tables = introspectSchema(currentDb);
      const fileName = selected.split(/[\\/]/).pop() ?? selected;
      set({
        filePath: selected,
        fileName,
        fileSizeBytes: info.size,
        tables,
        loadingFile: false,
        nlRequest: "",
        lastBackupPath: null,
        history: [],
        ...initialProposalFields,
      });
    } catch (err) {
      resetDbHandle();
      set({
        loadingFile: false,
        filePath: null,
        fileName: null,
        fileSizeBytes: null,
        tables: [],
        proposalError: err instanceof Error ? err.message : String(err),
      });
    }
  },

  closeFile: () => {
    resetDbHandle();
    set({
      filePath: null,
      fileName: null,
      fileSizeBytes: null,
      tables: [],
      nlRequest: "",
      lastBackupPath: null,
      history: [],
      ...initialProposalFields,
    });
  },

  setNlRequest: (text) => set({ nlRequest: text, proposalError: null }),

  propose: async () => {
    const { filePath, tables, nlRequest } = get();
    if (!filePath || !currentDb) {
      set({ proposalError: "Open a SQLite database file first." });
      return;
    }
    if (!nlRequest.trim()) {
      set({ proposalError: "Describe the query or change you want before proposing SQL." });
      return;
    }
    set({
      proposing: true,
      proposalError: null,
      proposedSql: null,
      proposalExplanation: null,
      statementKind: null,
      selectResult: null,
      dryRun: null,
      dryRunError: null,
      applyError: null,
    });
    try {
      const schemaSummary = buildSchemaSummary(tables);
      const target = await resolveTarget();
      const callModel = async (messages: ChatMessage[], signal?: AbortSignal): Promise<DbProposalCallResult> => {
        const result = await attemptStream(
          target,
          messages,
          [],
          signal,
          effortForTarget(target),
          DB_ADMIN_SESSION_ID,
          undefined,
          false,
        );
        return { content: result.content, streamError: result.streamError };
      };
      const proposal = await proposeSql(schemaSummary, nlRequest, callModel);
      const kind = classifyStatement(proposal.sql);

      if (kind === "select") {
        // Reads run immediately — no gate needed. A failing SELECT (bad
        // column name, etc.) surfaces as a normal error, not a crash.
        const result = runSelect(currentDb, proposal.sql);
        set({
          proposing: false,
          proposedSql: proposal.sql,
          proposalExplanation: proposal.explanation,
          statementKind: kind,
          selectResult: result,
        });
      } else {
        // write or unsupported — never executed here. `unsupported`
        // statements simply have no dry-run/apply path available in the UI.
        set({
          proposing: false,
          proposedSql: proposal.sql,
          proposalExplanation: proposal.explanation,
          statementKind: kind,
        });
      }
    } catch (err) {
      set({ proposing: false, proposalError: err instanceof Error ? err.message : String(err) });
    }
  },

  runDryRun: async () => {
    const { proposedSql, statementKind, tables } = get();
    if (!currentDb || !proposedSql || statementKind !== "write") return;
    set({ dryRunning: true, dryRunError: null });
    try {
      const result = dryRunWrite(currentDb, proposedSql, tables);
      set({ dryRunning: false, dryRun: result });
    } catch (err) {
      set({ dryRunning: false, dryRunError: err instanceof Error ? err.message : String(err) });
    }
  },

  /**
   * The ONLY path that ever writes real bytes back to disk. Refuses unless:
   * a database is open, a write statement was proposed, and `runDryRun` has
   * already produced a preview for that EXACT statement (`dryRun` is
   * cleared by `propose`/`cancelProposal`, so a stale preview from a
   * previous statement can never be reused here). Always backs up the
   * original file first.
   */
  approveApply: async () => {
    const { filePath, proposedSql, statementKind, dryRun } = get();
    if (!currentDb || !filePath || !proposedSql || statementKind !== "write") {
      set({ applyError: "Nothing to apply — propose a write statement first." });
      return;
    }
    if (!dryRun) {
      set({ applyError: "Run the dry-run preview before approving this write." });
      return;
    }
    set({ applying: true, applyError: null });
    try {
      const backupPath = suggestBackupPath(filePath);
      await copyFile(filePath, backupPath);

      const rowsAffected = applyWrite(currentDb, proposedSql);
      const updatedBytes = currentDb.export();
      await writeFile(filePath, updatedBytes);

      const tables = introspectSchema(currentDb);
      const entry: AppliedWriteEntry = {
        id: crypto.randomUUID(),
        sql: proposedSql,
        rowsAffected,
        backupPath,
        appliedAt: Date.now(),
      };
      set((state) => ({
        ...initialProposalFields,
        lastBackupPath: backupPath,
        history: [entry, ...state.history],
        tables,
        fileSizeBytes: updatedBytes.byteLength,
        nlRequest: "",
      }));
    } catch (err) {
      set({ applying: false, applyError: err instanceof Error ? err.message : String(err) });
    }
  },

  cancelProposal: () => set({ ...initialProposalFields }),
}));
