import { describe, expect, it } from "vitest";

import {
  DatasetParseError,
  MAX_OUTPUT_ROWS,
  buildDataset,
  createEmptyCell,
  createNotebookModel,
  createSeededDatabase,
  exportNotebookAsMarkdownReport,
  parseCsvDataset,
  parseJsonDataset,
  runAllCells,
  runCellsUpTo,
  runSqlStatements,
  sanitizeTableName,
  type Notebook,
} from "./dataNotebook";

const CSV = `name,age,score\nAda,36,9.5\nGrace,85,10\nLinus,54,\n`;

const JSON_RECORDS = JSON.stringify([
  { name: "Ada", age: 36, score: 9.5 },
  { name: "Grace", age: 85, score: 10 },
  { name: "Linus", age: 54 },
]);

describe("sanitizeTableName", () => {
  it("strips extension and directory, lowercases, and collapses separators", () => {
    expect(sanitizeTableName("My Data-Set (final).csv")).toBe("my_data_set_final");
  });

  it("prefixes a leading digit so the identifier stays unquoted-safe", () => {
    expect(sanitizeTableName("2024-sales.csv")).toBe("t_2024_sales");
  });

  it("falls back to a default name for an empty/unusable filename", () => {
    expect(sanitizeTableName(".csv")).toBe("data");
  });
});

describe("parseCsvDataset", () => {
  it("parses header + rows, infers column types, and coerces values", () => {
    const table = parseCsvDataset(CSV, "people");
    expect(table.columns).toEqual(["name", "age", "score"]);
    expect(table.columnTypes).toEqual(["TEXT", "INTEGER", "REAL"]);
    expect(table.rows).toEqual([
      ["Ada", 36, 9.5],
      ["Grace", 85, 10],
      ["Linus", 54, null],
    ]);
  });

  it("handles quoted fields containing commas and escaped quotes", () => {
    const csv = 'title,note\n"Smith, John","She said ""hi"""\n';
    const table = parseCsvDataset(csv, "t");
    expect(table.rows).toEqual([["Smith, John", 'She said "hi"']]);
  });

  it("throws DatasetParseError for empty input", () => {
    expect(() => parseCsvDataset("", "t")).toThrow(DatasetParseError);
    expect(() => parseCsvDataset("   \n", "t")).toThrow(DatasetParseError);
  });
});

describe("parseJsonDataset", () => {
  it("parses an array of records, unioning keys and inferring types", () => {
    const table = parseJsonDataset(JSON_RECORDS, "people");
    expect(table.columns).toEqual(["name", "age", "score"]);
    expect(table.columnTypes).toEqual(["TEXT", "INTEGER", "REAL"]);
    expect(table.rows).toEqual([
      ["Ada", 36, 9.5],
      ["Grace", 85, 10],
      ["Linus", 54, null],
    ]);
  });

  it("throws DatasetParseError for invalid JSON or a non-array top level", () => {
    expect(() => parseJsonDataset("not json", "t")).toThrow(DatasetParseError);
    expect(() => parseJsonDataset("{}", "t")).toThrow(DatasetParseError);
    expect(() => parseJsonDataset("[]", "t")).toThrow(DatasetParseError);
  });
});

describe("buildDataset", () => {
  it("summarizes a parsed CSV file into a persistable Dataset", () => {
    const dataset = buildDataset("people.csv", CSV, "csv");
    expect(dataset.tableName).toBe("people");
    expect(dataset.rowCount).toBe(3);
    expect(dataset.columns).toEqual(["name", "age", "score"]);
    expect(dataset.raw).toBe(CSV);
  });
});

describe("SQL execution against a seeded sql.js database", () => {
  it("seeds a CSV dataset and answers a SELECT query", async () => {
    const dataset = buildDataset("people.csv", CSV, "csv");
    const db = await createSeededDatabase(dataset);
    try {
      const output = runSqlStatements(db, "SELECT name, age FROM people ORDER BY age ASC");
      expect(output.columns).toEqual(["name", "age"]);
      expect(output.rows).toEqual([
        ["Ada", 36],
        ["Linus", 54],
        ["Grace", 85],
      ]);
      expect(output.rowCount).toBe(3);
      expect(output.truncated).toBe(false);
    } finally {
      db.close();
    }
  });

  it("seeds a JSON dataset equivalently", async () => {
    const dataset = buildDataset("people.json", JSON_RECORDS, "json");
    const db = await createSeededDatabase(dataset);
    try {
      const output = runSqlStatements(db, "SELECT COUNT(*) AS n FROM people");
      expect(output.rows).toEqual([[3]]);
    } finally {
      db.close();
    }
  });

  it("reports rowsAffected for a DML statement and surfaces SQL errors", async () => {
    const dataset = buildDataset("people.csv", CSV, "csv");
    const db = await createSeededDatabase(dataset);
    try {
      const update = runSqlStatements(db, "UPDATE people SET age = age + 1 WHERE name = 'Ada'");
      expect(update.rowsAffected).toBe(1);
      expect(() => runSqlStatements(db, "SELECT * FROM nonexistent_table")).toThrow();
    } finally {
      db.close();
    }
  });

  it("caps large result sets at MAX_OUTPUT_ROWS and flags truncation", async () => {
    const rows = Array.from({ length: MAX_OUTPUT_ROWS + 50 }, (_, i) => `${i}`).join("\n");
    const csv = `n\n${rows}\n`;
    const dataset = buildDataset("nums.csv", csv, "csv");
    const db = await createSeededDatabase(dataset);
    try {
      const output = runSqlStatements(db, "SELECT n FROM nums");
      expect(output.rows.length).toBe(MAX_OUTPUT_ROWS);
      expect(output.rowCount).toBe(MAX_OUTPUT_ROWS + 50);
      expect(output.truncated).toBe(true);
    } finally {
      db.close();
    }
  });
});

function notebookWithCells(): Notebook {
  const notebook = createNotebookModel("Analysis");
  notebook.dataset = buildDataset("people.csv", CSV, "csv");
  const cell1 = createEmptyCell("markdown");
  cell1.source = "# People analysis";
  const cell2 = createEmptyCell("sql");
  cell2.source = "SELECT name, age FROM people WHERE age > 40 ORDER BY age";
  const cell3 = createEmptyCell("sql");
  cell3.source = "SELECT COUNT(*) AS total FROM people";
  notebook.cells = [cell1, cell2, cell3];
  return notebook;
}

describe("runAllCells / runCellsUpTo (notebook reproducibility)", () => {
  it("runs every cell in order and records outputs on fresh cells", async () => {
    const notebook = notebookWithCells();
    const executed = await runAllCells(notebook);

    expect(executed.cells[0].output).toBeNull(); // markdown cell
    expect(executed.cells[0].error).toBeNull();
    expect(executed.cells[0].lastRunAt).not.toBeNull();

    expect(executed.cells[1].output?.rows).toEqual([
      ["Linus", 54],
      ["Grace", 85],
    ]);
    expect(executed.cells[2].output?.rows).toEqual([[3]]);
  });

  it("reproduces byte-identical outputs when re-run from the saved notebook state (the acceptance criterion)", async () => {
    const notebook = notebookWithCells();
    const firstRun = await runAllCells(notebook);

    // Simulate "reload from persisted storage": a plain JSON round-trip of
    // the executed notebook, exactly like localStorage persistence does.
    const reloaded = JSON.parse(JSON.stringify(firstRun)) as Notebook;

    const secondRun = await runAllCells(reloaded);

    expect(secondRun.cells[1].output).toEqual(firstRun.cells[1].output);
    expect(secondRun.cells[2].output).toEqual(firstRun.cells[2].output);
    expect(secondRun.cells[1].error).toBeNull();
    expect(secondRun.cells[2].error).toBeNull();
  });

  it("runCellsUpTo only (re)runs cells up to and including the target, leaving later cells untouched", async () => {
    const notebook = notebookWithCells();
    const partiallyRun = await runCellsUpTo(notebook, notebook.cells[1].id);

    expect(partiallyRun.cells[0].lastRunAt).not.toBeNull();
    expect(partiallyRun.cells[1].output?.rows).toEqual([
      ["Linus", 54],
      ["Grace", 85],
    ]);
    // Cell 3 was never touched by this call.
    expect(partiallyRun.cells[2].output).toBeNull();
    expect(partiallyRun.cells[2].lastRunAt).toBeNull();
  });

  it("records a cell's SQL error without throwing, and halts cells run afterward in the same pass", async () => {
    const notebook = notebookWithCells();
    notebook.cells[1].source = "SELECT * FROM this_table_does_not_exist";

    const executed = await runAllCells(notebook);

    expect(executed.cells[1].error).toMatch(/no such table/i);
    expect(executed.cells[1].output).toBeNull();
    // The cell after the failure was never reached this pass.
    expect(executed.cells[2].output).toBeNull();
    expect(executed.cells[2].lastRunAt).toBeNull();
  });

  it("throws for an unknown cell id", async () => {
    const notebook = notebookWithCells();
    await expect(runCellsUpTo(notebook, "does-not-exist")).rejects.toThrow(/Cell not found/);
  });
});

describe("exportNotebookAsMarkdownReport", () => {
  it("renders markdown cells, SQL source, and rendered result tables", async () => {
    const notebook = await runAllCells(notebookWithCells());
    const report = exportNotebookAsMarkdownReport(notebook);

    expect(report).toContain("# Analysis");
    expect(report).toContain("people.csv");
    expect(report).toContain("# People analysis");
    expect(report).toContain("```sql");
    expect(report).toContain("| name | age |");
    expect(report).toContain("| Grace | 85 |");
    expect(report).toContain("| total |");
  });

  it("surfaces a cell's error instead of a table", async () => {
    const notebook = notebookWithCells();
    notebook.cells[1].source = "SELECT * FROM missing";
    const executed = await runAllCells(notebook);
    const report = exportNotebookAsMarkdownReport(executed);
    expect(report).toContain("**Error:**");
  });
});
