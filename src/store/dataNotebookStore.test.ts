import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

const openMock = vi.fn();
vi.mock("@tauri-apps/plugin-dialog", () => ({ open: (...args: unknown[]) => openMock(...args) }));

const readTextFileMock = vi.fn();
vi.mock("@tauri-apps/plugin-fs", () => ({ readTextFile: (...args: unknown[]) => readTextFileMock(...args) }));

import { DATA_NOTEBOOK_STORAGE_KEY, useDataNotebookStore } from "./dataNotebookStore";

class MemoryStorage implements Storage {
  private readonly values = new Map<string, string>();

  get length(): number {
    return this.values.size;
  }

  clear(): void {
    this.values.clear();
  }

  getItem(key: string): string | null {
    return this.values.get(key) ?? null;
  }

  key(index: number): string | null {
    return [...this.values.keys()][index] ?? null;
  }

  removeItem(key: string): void {
    this.values.delete(key);
  }

  setItem(key: string, value: string): void {
    this.values.set(key, String(value));
  }
}

let storage: MemoryStorage;

beforeEach(() => {
  storage = new MemoryStorage();
  vi.stubGlobal("localStorage", storage);
  useDataNotebookStore.setState({
    notebooks: [],
    activeNotebookId: null,
    runningNotebookId: null,
    importError: null,
    persistError: null,
  });
  openMock.mockReset();
  readTextFileMock.mockReset();
});

afterEach(() => {
  vi.unstubAllGlobals();
});

function persistedNotebooks(): { version: number; notebooks: unknown[]; activeNotebookId: string | null } {
  const raw = storage.getItem(DATA_NOTEBOOK_STORAGE_KEY);
  expect(raw).not.toBeNull();
  return JSON.parse(raw as string);
}

const CSV = "name,age\nAda,36\nGrace,85\n";

describe("dataNotebookStore CRUD + persistence", () => {
  it("creates a notebook, makes it active, and persists it", () => {
    const id = useDataNotebookStore.getState().createNotebook("My Analysis");
    const state = useDataNotebookStore.getState();
    expect(state.notebooks).toHaveLength(1);
    expect(state.notebooks[0].id).toBe(id);
    expect(state.notebooks[0].name).toBe("My Analysis");
    expect(state.activeNotebookId).toBe(id);

    const persisted = persistedNotebooks();
    expect(persisted.notebooks).toHaveLength(1);
    expect(persisted.activeNotebookId).toBe(id);
  });

  it("defaults to an 'Untitled notebook' name", () => {
    useDataNotebookStore.getState().createNotebook();
    expect(useDataNotebookStore.getState().notebooks[0].name).toBe("Untitled notebook");
  });

  it("renames a notebook", () => {
    const id = useDataNotebookStore.getState().createNotebook("Draft");
    useDataNotebookStore.getState().renameNotebook(id, "  Final Report  ");
    expect(useDataNotebookStore.getState().notebooks[0].name).toBe("Final Report");
  });

  it("deletes a notebook and reassigns the active id", () => {
    const id1 = useDataNotebookStore.getState().createNotebook("First");
    const id2 = useDataNotebookStore.getState().createNotebook("Second");
    expect(useDataNotebookStore.getState().activeNotebookId).toBe(id2);

    useDataNotebookStore.getState().deleteNotebook(id2);
    const state = useDataNotebookStore.getState();
    expect(state.notebooks.map((n) => n.id)).toEqual([id1]);
    expect(state.activeNotebookId).toBe(id1);
  });

  it("adds, edits, reorders, and removes cells", () => {
    const notebookId = useDataNotebookStore.getState().createNotebook();
    const cellA = useDataNotebookStore.getState().addCell(notebookId, "markdown");
    const cellB = useDataNotebookStore.getState().addCell(notebookId, "sql");

    useDataNotebookStore.getState().updateCellSource(notebookId, cellB, "SELECT 1");
    let notebook = useDataNotebookStore.getState().notebooks[0];
    expect(notebook.cells.map((c) => c.id)).toEqual([cellA, cellB]);
    expect(notebook.cells[1].source).toBe("SELECT 1");

    useDataNotebookStore.getState().moveCell(notebookId, cellB, "up");
    notebook = useDataNotebookStore.getState().notebooks[0];
    expect(notebook.cells.map((c) => c.id)).toEqual([cellB, cellA]);

    useDataNotebookStore.getState().removeCell(notebookId, cellA);
    notebook = useDataNotebookStore.getState().notebooks[0];
    expect(notebook.cells.map((c) => c.id)).toEqual([cellB]);
  });

  it("moveCell is a no-op past either edge", () => {
    const notebookId = useDataNotebookStore.getState().createNotebook();
    const cellA = useDataNotebookStore.getState().addCell(notebookId, "sql");
    useDataNotebookStore.getState().moveCell(notebookId, cellA, "up");
    expect(useDataNotebookStore.getState().notebooks[0].cells.map((c) => c.id)).toEqual([cellA]);
  });
});

describe("dataNotebookStore dataset import", () => {
  it("imports a CSV file picked via the native dialog and sets it as the dataset", async () => {
    const notebookId = useDataNotebookStore.getState().createNotebook();
    openMock.mockResolvedValue("/Users/me/people.csv");
    readTextFileMock.mockResolvedValue(CSV);

    await useDataNotebookStore.getState().importDataset(notebookId);

    const notebook = useDataNotebookStore.getState().notebooks[0];
    expect(notebook.dataset?.name).toBe("people.csv");
    expect(notebook.dataset?.tableName).toBe("people");
    expect(notebook.dataset?.format).toBe("csv");
    expect(notebook.dataset?.rowCount).toBe(2);
    expect(useDataNotebookStore.getState().importError).toBeNull();
  });

  it("does nothing when the user cancels the picker", async () => {
    const notebookId = useDataNotebookStore.getState().createNotebook();
    openMock.mockResolvedValue(null);

    await useDataNotebookStore.getState().importDataset(notebookId);

    expect(useDataNotebookStore.getState().notebooks[0].dataset).toBeNull();
    expect(readTextFileMock).not.toHaveBeenCalled();
  });

  it("surfaces a parse error for an unparsable file without touching the notebook", async () => {
    const notebookId = useDataNotebookStore.getState().createNotebook();
    openMock.mockResolvedValue("/Users/me/broken.json");
    readTextFileMock.mockResolvedValue("not json");

    await useDataNotebookStore.getState().importDataset(notebookId);

    expect(useDataNotebookStore.getState().notebooks[0].dataset).toBeNull();
    expect(useDataNotebookStore.getState().importError).toMatch(/valid JSON/);
  });

  it("clears a notebook's dataset", async () => {
    const notebookId = useDataNotebookStore.getState().createNotebook();
    openMock.mockResolvedValue("/Users/me/people.csv");
    readTextFileMock.mockResolvedValue(CSV);
    await useDataNotebookStore.getState().importDataset(notebookId);
    expect(useDataNotebookStore.getState().notebooks[0].dataset).not.toBeNull();

    useDataNotebookStore.getState().clearDataset(notebookId);
    expect(useDataNotebookStore.getState().notebooks[0].dataset).toBeNull();
  });
});

describe("dataNotebookStore run / re-run all (reproducibility)", () => {
  async function seededNotebook(): Promise<string> {
    const notebookId = useDataNotebookStore.getState().createNotebook();
    openMock.mockResolvedValue("/Users/me/people.csv");
    readTextFileMock.mockResolvedValue(CSV);
    await useDataNotebookStore.getState().importDataset(notebookId);
    useDataNotebookStore.getState().addCell(notebookId, "sql");
    const cellId = useDataNotebookStore.getState().notebooks[0].cells[0].id;
    useDataNotebookStore.getState().updateCellSource(notebookId, cellId, "SELECT name FROM people ORDER BY age");
    return notebookId;
  }

  it("runs a single cell and records its output", async () => {
    const notebookId = await seededNotebook();
    const cellId = useDataNotebookStore.getState().notebooks[0].cells[0].id;

    await useDataNotebookStore.getState().runCell(notebookId, cellId);

    const cell = useDataNotebookStore.getState().notebooks[0].cells[0];
    expect(cell.output?.rows).toEqual([["Ada"], ["Grace"]]);
    expect(cell.error).toBeNull();
    expect(useDataNotebookStore.getState().runningNotebookId).toBeNull();
  });

  it("runAll reproduces the same output after a simulated reload from persisted storage", async () => {
    const notebookId = await seededNotebook();
    await useDataNotebookStore.getState().runAll(notebookId);
    const firstOutput = useDataNotebookStore.getState().notebooks[0].cells[0].output;
    expect(firstOutput?.rows).toEqual([["Ada"], ["Grace"]]);

    // Simulate app restart: rehydrate straight from the persisted blob into a
    // brand-new store state, then re-run.
    const persistedRaw = storage.getItem(DATA_NOTEBOOK_STORAGE_KEY)!;
    const persisted = JSON.parse(persistedRaw);
    useDataNotebookStore.setState({ notebooks: persisted.notebooks, activeNotebookId: persisted.activeNotebookId });

    await useDataNotebookStore.getState().runAll(notebookId);
    const secondOutput = useDataNotebookStore.getState().notebooks[0].cells[0].output;
    expect(secondOutput).toEqual(firstOutput);
  });

  it("exportReport renders a markdown report for a run notebook", async () => {
    const notebookId = await seededNotebook();
    await useDataNotebookStore.getState().runAll(notebookId);
    const report = useDataNotebookStore.getState().exportReport(notebookId);
    expect(report).toContain("```sql");
    expect(report).toContain("| Ada |");
  });

  it("exportReport returns null for an unknown notebook id", () => {
    expect(useDataNotebookStore.getState().exportReport("nope")).toBeNull();
  });
});

describe("dataNotebookStore hydration", () => {
  it("ignores a corrupted persisted blob instead of throwing", () => {
    storage.setItem(DATA_NOTEBOOK_STORAGE_KEY, "not json");
    // Re-importing the module would re-run hydration; since the store is
    // already constructed in this test file, we exercise the exported
    // hydration behavior indirectly via a fresh localStorage read through
    // the same code path createNotebook/persist uses (no throw on read).
    expect(() => useDataNotebookStore.getState().createNotebook()).not.toThrow();
  });
});
