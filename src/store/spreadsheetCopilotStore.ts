/**
 * Spreadsheet Copilot (ROADMAP.md Phase 7, item 19) — loads a local CSV file,
 * drives one-shot model calls (the same `resolveTarget` + `attemptStream`
 * pattern `sopCompilerStore.ts` uses for its own compiler call) to propose a
 * SINGLE cell-cited operation at a time, and only ever writes the file back
 * to disk when the user explicitly clicks Approve in
 * `SpreadsheetCopilotPanel.tsx`. Nothing in this store mutates the loaded
 * file on its own: `propose()` only ever produces a diff-able
 * `SpreadsheetOperationProposal` held in memory, exactly mirroring the
 * acceptance criterion ("Spreadsheet changes cite exact cells/ranges and ask
 * before mutating live workbooks").
 *
 * MVP scope is CSV only (see `spreadsheetCopilot.ts`'s top doc comment) —
 * XLSX and Google Sheets are out of scope here, follow-ups needing a
 * workbook-parsing dependency and OAuth respectively.
 */
import { create } from "zustand";
import { open } from "@tauri-apps/plugin-dialog";
import { readTextFile, stat, writeTextFile } from "@tauri-apps/plugin-fs";

import { resolveTarget } from "../lib/agentLoop";
import { attemptStream } from "../lib/turnEngine";
import type { ChatMessage } from "../lib/llamaClient";
import { effortForTarget } from "./modelStore";
import {
  parseCsv,
  proposeOperation,
  serializeCsv,
  type SpreadsheetCopilotCallResult,
  type SpreadsheetOperationProposal,
  type SpreadsheetTable,
} from "../lib/spreadsheetCopilot";

/** Fixed pseudo-session id for the copilot's one-shot model calls — mirrors
 * `sopCompilerStore.ts`'s `SOP_COMPILER_SESSION_ID`: this never belongs to a
 * chat session, and `recordUsage: false` (the trailing `attemptStream`
 * argument below) means nothing is ever recorded under it in
 * `useUsageStore`; it only needs to be a stable, non-empty string. */
const SPREADSHEET_COPILOT_SESSION_ID = "spreadsheet-copilot";

/** Reject an imported file above this size outright — mirrors
 * `sopCompilerStore.ts`'s own `MAX_IMPORT_FILE_BYTES` guard (itself borrowed
 * from `EcosystemPackages.tsx`'s file-import guard). A spreadsheet this
 * large also wouldn't fit in a local model's context window as a sample. */
const MAX_IMPORT_FILE_BYTES = 10 * 1024 * 1024;

interface SpreadsheetCopilotStore {
  filePath: string | null;
  fileName: string | null;
  table: SpreadsheetTable | null;
  requestText: string;
  proposal: SpreadsheetOperationProposal | null;
  loadingFile: boolean;
  proposing: boolean;
  approving: boolean;
  error: string | null;

  loadFromFile: () => Promise<void>;
  setRequestText: (text: string) => void;
  propose: () => Promise<void>;
  approve: () => Promise<void>;
  reject: () => void;
  reset: () => void;
}

function errorMessage(err: unknown): string {
  return err instanceof Error ? err.message : String(err);
}

export const useSpreadsheetCopilotStore = create<SpreadsheetCopilotStore>((set, get) => ({
  filePath: null,
  fileName: null,
  table: null,
  requestText: "",
  proposal: null,
  loadingFile: false,
  proposing: false,
  approving: false,
  error: null,

  loadFromFile: async () => {
    set({ loadingFile: true, error: null });
    try {
      const selected = await open({
        title: "Open a CSV spreadsheet",
        multiple: false,
        directory: false,
        filters: [{ name: "CSV", extensions: ["csv"] }],
      });
      if (typeof selected !== "string") {
        set({ loadingFile: false });
        return;
      }
      const fileInfo = await stat(selected);
      if (fileInfo.size > MAX_IMPORT_FILE_BYTES) {
        throw new Error(`That file is larger than ${Math.floor(MAX_IMPORT_FILE_BYTES / (1024 * 1024))}MB — Spreadsheet Copilot's MVP is scoped to smaller CSV files.`);
      }
      const content = await readTextFile(selected);
      const table = parseCsv(content);
      const fileName = selected.split(/[\\/]/).pop() ?? selected;
      set({
        filePath: selected,
        fileName,
        table,
        proposal: null,
        requestText: "",
        loadingFile: false,
      });
    } catch (err) {
      set({ loadingFile: false, error: errorMessage(err) });
    }
  },

  setRequestText: (text) => set({ requestText: text, error: null }),

  propose: async () => {
    const { table, requestText, fileName } = get();
    if (!table) {
      set({ error: "Load a CSV file before requesting an operation." });
      return;
    }
    if (!requestText.trim()) {
      set({ error: "Describe the operation you want (a computed column, a cleanup step, or a summary)." });
      return;
    }
    set({ proposing: true, error: null });
    try {
      const target = await resolveTarget();
      const callModel = async (messages: ChatMessage[], signal?: AbortSignal): Promise<SpreadsheetCopilotCallResult> => {
        const result = await attemptStream(
          target,
          messages,
          [],
          signal,
          effortForTarget(target),
          SPREADSHEET_COPILOT_SESSION_ID,
          undefined,
          false,
        );
        return { content: result.content, streamError: result.streamError };
      };
      const proposal = await proposeOperation(table, requestText, callModel, fileName ?? undefined);
      set({ proposal, proposing: false });
    } catch (err) {
      set({ proposing: false, error: errorMessage(err) });
    }
  },

  /**
   * The ONLY place this store writes to disk — an explicit, human-initiated
   * action (the Approve button in `SpreadsheetCopilotPanel.tsx`), same
   * "human-initiated, not additionally permission-gated" precedent
   * `ArtifactPane.tsx`'s `handleSaveAs`/`handleOpenInBrowser` already use for
   * a file path the user themselves picked (here, via the earlier
   * `loadFromFile` dialog) — the review step IS the gate: the user has
   * already seen the cited ranges and the cell-level diff before this runs.
   */
  approve: async () => {
    const { filePath, proposal } = get();
    if (!filePath || !proposal) return;
    set({ approving: true, error: null });
    try {
      await writeTextFile(filePath, serializeCsv(proposal.proposedTable));
      set({
        table: proposal.proposedTable,
        proposal: null,
        requestText: "",
        approving: false,
      });
    } catch (err) {
      set({ approving: false, error: errorMessage(err) });
    }
  },

  reject: () => set({ proposal: null }),

  reset: () =>
    set({
      filePath: null,
      fileName: null,
      table: null,
      requestText: "",
      proposal: null,
      loadingFile: false,
      proposing: false,
      approving: false,
      error: null,
    }),
}));
