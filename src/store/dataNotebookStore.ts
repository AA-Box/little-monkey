/**
 * Data Notebook and SQL Lab (ROADMAP.md Phase 7, item 18) — zustand store
 * wrapping `../lib/dataNotebook.ts`'s pure notebook model/execution engine
 * with persistence and the Tauri file-picker used to import a dataset.
 *
 * Persistence is a synchronous localStorage blob (same shape/precedent as
 * `shortcutStore.ts`) — notebooks are small, local, single-user artifacts,
 * so there's no need for the file-based cross-window sync `promptStore.ts`
 * or `sessionStore.ts` use for larger shared libraries. Everything needed to
 * reproduce a notebook (its cells' saved `source`, the imported dataset's
 * raw text, and the last-run `output`/`error` per cell) round-trips through
 * this one blob — see `dataNotebook.ts`'s `runAllCells` doc comment for why
 * that reproduces the saved outputs.
 */
import { create } from "zustand";
import { open } from "@tauri-apps/plugin-dialog";
import { readTextFile } from "@tauri-apps/plugin-fs";

import {
  DatasetParseError,
  buildDataset,
  createEmptyCell,
  createNotebookModel,
  exportNotebookAsMarkdownReport,
  runAllCells,
  runCellsUpTo,
  type Dataset,
  type DatasetFormat,
  type Notebook,
  type NotebookCellType,
} from "../lib/dataNotebook";
import { errorMessage } from "../lib/errors";

export const DATA_NOTEBOOK_STORAGE_KEY = "little-monkey-data-notebooks";
export const DATA_NOTEBOOK_STORAGE_VERSION = 1 as const;

interface PersistedShapeV1 {
  version: typeof DATA_NOTEBOOK_STORAGE_VERSION;
  notebooks: Notebook[];
  activeNotebookId: string | null;
}

function messageOf(err: unknown): string {
  return errorMessage(err);
}

/** Loose structural check on a hand-edited/corrupted persisted blob — good
 * enough to keep a malformed localStorage value from crashing hydration
 * (falls back to dropping just that notebook, not the whole store). */
function isNotebookShape(value: unknown): value is Notebook {
  if (!value || typeof value !== "object") return false;
  const n = value as Partial<Notebook>;
  return (
    typeof n.id === "string" &&
    typeof n.name === "string" &&
    typeof n.createdAt === "number" &&
    typeof n.updatedAt === "number" &&
    Array.isArray(n.cells) &&
    (n.dataset === null || (typeof n.dataset === "object" && n.dataset !== null))
  );
}

function hydrate(): { notebooks: Notebook[]; activeNotebookId: string | null } {
  try {
    const raw = localStorage.getItem(DATA_NOTEBOOK_STORAGE_KEY);
    if (!raw) return { notebooks: [], activeNotebookId: null };
    const parsed = JSON.parse(raw) as Partial<PersistedShapeV1> | null;
    if (!parsed || parsed.version !== DATA_NOTEBOOK_STORAGE_VERSION || !Array.isArray(parsed.notebooks)) {
      return { notebooks: [], activeNotebookId: null };
    }
    const notebooks = parsed.notebooks.filter(isNotebookShape);
    const activeNotebookId =
      typeof parsed.activeNotebookId === "string" && notebooks.some((n) => n.id === parsed.activeNotebookId)
        ? parsed.activeNotebookId
        : null;
    return { notebooks, activeNotebookId };
  } catch {
    return { notebooks: [], activeNotebookId: null };
  }
}

function persist(notebooks: Notebook[], activeNotebookId: string | null): string | null {
  const payload: PersistedShapeV1 = { version: DATA_NOTEBOOK_STORAGE_VERSION, notebooks, activeNotebookId };
  try {
    localStorage.setItem(DATA_NOTEBOOK_STORAGE_KEY, JSON.stringify(payload));
    return null;
  } catch (err) {
    // Best-effort persistence (e.g. quota exceeded on a large imported
    // dataset) — the in-memory state is still correct for this session, the
    // caller just surfaces `persistError` in the UI.
    return messageOf(err);
  }
}

export interface DataNotebookState {
  notebooks: Notebook[];
  activeNotebookId: string | null;
  /** Notebook id currently mid-run (Run/Run All in flight); disables re-entrant runs on it. */
  runningNotebookId: string | null;
  /** Last dataset-import failure, cleared at the start of the next import attempt. */
  importError: string | null;
  /** Last localStorage write failure, cleared on the next successful write. */
  persistError: string | null;

  createNotebook: (name?: string) => string;
  deleteNotebook: (id: string) => void;
  renameNotebook: (id: string, name: string) => void;
  setActiveNotebook: (id: string | null) => void;

  addCell: (notebookId: string, type: NotebookCellType) => string;
  updateCellSource: (notebookId: string, cellId: string, source: string) => void;
  removeCell: (notebookId: string, cellId: string) => void;
  moveCell: (notebookId: string, cellId: string, direction: "up" | "down") => void;

  /** Opens the native file picker (CSV/JSON only), reads and parses the
   * chosen file, and sets it as `notebookId`'s dataset. No-ops (leaves
   * `importError` untouched) if the user cancels the picker. */
  importDataset: (notebookId: string) => Promise<void>;
  clearDataset: (notebookId: string) => void;

  /** Re-runs cells 0..cellId (inclusive) — see `runCellsUpTo`. */
  runCell: (notebookId: string, cellId: string) => Promise<void>;
  /** Re-runs every cell in the notebook, in order — "Re-run all". */
  runAll: (notebookId: string) => Promise<void>;

  /** Renders the notebook's cells + last-run outputs as a Markdown report;
   * `null` if the notebook doesn't exist. */
  exportReport: (notebookId: string) => string | null;
}

const initial = hydrate();

export const useDataNotebookStore = create<DataNotebookState>((set, get) => ({
  notebooks: initial.notebooks,
  activeNotebookId: initial.activeNotebookId,
  runningNotebookId: null,
  importError: null,
  persistError: null,

  createNotebook: (name) => {
    const notebook = createNotebookModel(name && name.trim().length > 0 ? name.trim() : "Untitled notebook");
    set((state) => {
      const notebooks = [...state.notebooks, notebook];
      const persistError = persist(notebooks, notebook.id);
      return { notebooks, activeNotebookId: notebook.id, persistError };
    });
    return notebook.id;
  },

  deleteNotebook: (id) => {
    set((state) => {
      if (!state.notebooks.some((n) => n.id === id)) return state;
      const notebooks = state.notebooks.filter((n) => n.id !== id);
      const activeNotebookId = state.activeNotebookId === id ? (notebooks[0]?.id ?? null) : state.activeNotebookId;
      const persistError = persist(notebooks, activeNotebookId);
      return { notebooks, activeNotebookId, persistError };
    });
  },

  renameNotebook: (id, name) => {
    const trimmed = name.trim();
    if (trimmed.length === 0) return;
    set((state) => {
      const notebooks = state.notebooks.map((n) => (n.id === id ? { ...n, name: trimmed, updatedAt: Date.now() } : n));
      const persistError = persist(notebooks, state.activeNotebookId);
      return { notebooks, persistError };
    });
  },

  setActiveNotebook: (id) => {
    set((state) => {
      if (id !== null && !state.notebooks.some((n) => n.id === id)) return state;
      const persistError = persist(state.notebooks, id);
      return { activeNotebookId: id, persistError };
    });
  },

  addCell: (notebookId, type) => {
    const cell = createEmptyCell(type);
    set((state) => {
      const notebooks = state.notebooks.map((n) =>
        n.id === notebookId ? { ...n, cells: [...n.cells, cell], updatedAt: Date.now() } : n,
      );
      const persistError = persist(notebooks, state.activeNotebookId);
      return { notebooks, persistError };
    });
    return cell.id;
  },

  updateCellSource: (notebookId, cellId, source) => {
    set((state) => {
      const notebooks = state.notebooks.map((n) => {
        if (n.id !== notebookId) return n;
        const cells = n.cells.map((c) => (c.id === cellId ? { ...c, source } : c));
        return { ...n, cells, updatedAt: Date.now() };
      });
      const persistError = persist(notebooks, state.activeNotebookId);
      return { notebooks, persistError };
    });
  },

  removeCell: (notebookId, cellId) => {
    set((state) => {
      const notebooks = state.notebooks.map((n) => {
        if (n.id !== notebookId) return n;
        return { ...n, cells: n.cells.filter((c) => c.id !== cellId), updatedAt: Date.now() };
      });
      const persistError = persist(notebooks, state.activeNotebookId);
      return { notebooks, persistError };
    });
  },

  moveCell: (notebookId, cellId, direction) => {
    set((state) => {
      const notebooks = state.notebooks.map((n) => {
        if (n.id !== notebookId) return n;
        const index = n.cells.findIndex((c) => c.id === cellId);
        if (index === -1) return n;
        const swapWith = direction === "up" ? index - 1 : index + 1;
        if (swapWith < 0 || swapWith >= n.cells.length) return n;
        const cells = [...n.cells];
        [cells[index], cells[swapWith]] = [cells[swapWith], cells[index]];
        return { ...n, cells, updatedAt: Date.now() };
      });
      const persistError = persist(notebooks, state.activeNotebookId);
      return { notebooks, persistError };
    });
  },

  importDataset: async (notebookId) => {
    set({ importError: null });
    let selected: string | null = null;
    try {
      const picked = await open({
        title: "Import dataset",
        multiple: false,
        directory: false,
        filters: [{ name: "CSV or JSON", extensions: ["csv", "json"] }],
      });
      if (typeof picked === "string") selected = picked;
    } catch (err) {
      set({ importError: messageOf(err) });
      return;
    }
    if (selected === null) return; // user cancelled the picker

    try {
      const raw = await readTextFile(selected);
      const format: DatasetFormat = selected.toLowerCase().endsWith(".json") ? "json" : "csv";
      const fileName = selected.split(/[\\/]/).pop() ?? selected;
      const dataset: Dataset = buildDataset(fileName, raw, format);
      set((state) => {
        const notebooks = state.notebooks.map((n) => (n.id === notebookId ? { ...n, dataset, updatedAt: Date.now() } : n));
        const persistError = persist(notebooks, state.activeNotebookId);
        return { notebooks, persistError };
      });
    } catch (err) {
      set({ importError: err instanceof DatasetParseError ? err.message : messageOf(err) });
    }
  },

  clearDataset: (notebookId) => {
    set((state) => {
      const notebooks = state.notebooks.map((n) => (n.id === notebookId ? { ...n, dataset: null, updatedAt: Date.now() } : n));
      const persistError = persist(notebooks, state.activeNotebookId);
      return { notebooks, persistError };
    });
  },

  runCell: async (notebookId, cellId) => {
    const notebook = get().notebooks.find((n) => n.id === notebookId);
    if (!notebook) return;
    set({ runningNotebookId: notebookId });
    try {
      const updated = await runCellsUpTo(notebook, cellId);
      set((state) => {
        const notebooks = state.notebooks.map((n) => (n.id === notebookId ? updated : n));
        const persistError = persist(notebooks, state.activeNotebookId);
        return { notebooks, persistError };
      });
    } finally {
      set((state) => (state.runningNotebookId === notebookId ? { runningNotebookId: null } : {}));
    }
  },

  runAll: async (notebookId) => {
    const notebook = get().notebooks.find((n) => n.id === notebookId);
    if (!notebook) return;
    set({ runningNotebookId: notebookId });
    try {
      const updated = await runAllCells(notebook);
      set((state) => {
        const notebooks = state.notebooks.map((n) => (n.id === notebookId ? updated : n));
        const persistError = persist(notebooks, state.activeNotebookId);
        return { notebooks, persistError };
      });
    } finally {
      set((state) => (state.runningNotebookId === notebookId ? { runningNotebookId: null } : {}));
    }
  },

  exportReport: (notebookId) => {
    const notebook = get().notebooks.find((n) => n.id === notebookId);
    return notebook ? exportNotebookAsMarkdownReport(notebook) : null;
  },
}));

export default useDataNotebookStore;
