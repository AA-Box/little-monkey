import { describe, expect, it } from "vitest";

import {
  applyWrites,
  boundingRange,
  cellRef,
  columnIndex,
  columnLetters,
  parseCellRef,
  parseCsv,
  parseRangeRef,
  parseSpreadsheetCopilotResponse,
  proposeOperation,
  rangeWithinTable,
  serializeCsv,
  type SpreadsheetTable,
} from "./spreadsheetCopilot";

const SAMPLE_CSV = [
  "name,quantity,price",
  "Widget,2,9.99",
  "Gadget,5,4.50",
  '"Gizmo, Deluxe",1,19.99',
].join("\n");

function sampleTable(): SpreadsheetTable {
  return parseCsv(SAMPLE_CSV);
}

describe("parseCsv / serializeCsv", () => {
  it("parses headers and rows, padding short rows", () => {
    const table = parseCsv("a,b,c\n1,2\n3,4,5,6\n");
    expect(table.headers).toEqual(["a", "b", "c"]);
    expect(table.rows).toEqual([
      ["1", "2", ""],
      ["3", "4", "5"],
    ]);
  });

  it("handles quoted fields with embedded commas and escaped quotes", () => {
    const table = parseCsv('name,note\n"Smith, John","Said ""hi"""\n');
    expect(table.rows).toEqual([["Smith, John", 'Said "hi"']]);
  });

  it("round-trips through serializeCsv", () => {
    const table = sampleTable();
    const serialized = serializeCsv(table);
    const reparsed = parseCsv(serialized);
    expect(reparsed).toEqual(table);
  });

  it("quotes fields containing commas/newlines/quotes on serialize", () => {
    const table: SpreadsheetTable = { headers: ["a"], rows: [["has,comma"], ['has"quote']] };
    const csv = serializeCsv(table);
    expect(csv).toContain('"has,comma"');
    expect(csv).toContain('"has""quote"');
  });

  it("treats an empty document as a trivial single-column table", () => {
    expect(parseCsv("")).toEqual({ headers: ["A"], rows: [] });
    expect(parseCsv("   \n  ")).toEqual({ headers: ["A"], rows: [] });
  });
});

describe("column/cell reference helpers", () => {
  it("converts column indexes to letters and back", () => {
    expect(columnLetters(0)).toBe("A");
    expect(columnLetters(25)).toBe("Z");
    expect(columnLetters(26)).toBe("AA");
    expect(columnLetters(27)).toBe("AB");
    expect(columnIndex("A")).toBe(0);
    expect(columnIndex("Z")).toBe(25);
    expect(columnIndex("AA")).toBe(26);
    expect(columnIndex("not-a-column")).toBeNull();
  });

  it("builds and parses cell refs", () => {
    expect(cellRef(2, 0)).toBe("A2");
    expect(cellRef(1, 3)).toBe("D1");
    expect(parseCellRef("D1")).toEqual({ sheetRow: 1, col: 3 });
    expect(parseCellRef("a2")).toEqual({ sheetRow: 2, col: 0 });
    expect(parseCellRef("2A")).toBeNull();
    expect(parseCellRef("A0")).toBeNull();
    expect(parseCellRef("A01")).toBeNull();
  });

  it("parses ranges and single cells", () => {
    expect(parseRangeRef("B2:B11")).toEqual({
      start: { sheetRow: 2, col: 1 },
      end: { sheetRow: 11, col: 1 },
    });
    expect(parseRangeRef("F1")).toEqual({
      start: { sheetRow: 1, col: 5 },
      end: { sheetRow: 1, col: 5 },
    });
    expect(parseRangeRef("bogus:B11")).toBeNull();
  });

  it("validates a range against a table's actual bounds", () => {
    const table = sampleTable(); // 3 columns (A-C), 3 data rows (rows 2-4)
    expect(rangeWithinTable(parseRangeRef("B2:B4")!, table)).toBe(true);
    expect(rangeWithinTable(parseRangeRef("B2:B99")!, table)).toBe(false);
    expect(rangeWithinTable(parseRangeRef("Z1")!, table)).toBe(false);
  });

  it("computes the smallest bounding range for a set of write refs", () => {
    const refs = ["D2", "D4", "D3"].map((ref) => parseCellRef(ref)!);
    expect(boundingRange(refs)).toBe("D2:D4");
    expect(boundingRange([parseCellRef("D1")!])).toBe("D1");
  });
});

describe("applyWrites", () => {
  it("edits an existing cell without mutating the input table", () => {
    const table = sampleTable();
    const { table: next, diff } = applyWrites(table, [{ ref: "A2", value: "Widget Pro" }]);
    expect(next.rows[0][0]).toBe("Widget Pro");
    expect(table.rows[0][0]).toBe("Widget"); // original untouched
    expect(diff).toEqual([{ ref: "A2", before: "Widget", after: "Widget Pro" }]);
  });

  it("appends a new derived column with a header write plus one write per row", () => {
    const table = sampleTable();
    const writes = [
      { ref: "D1", value: "Total" },
      { ref: "D2", value: "19.98" },
      { ref: "D3", value: "22.50" },
      { ref: "D4", value: "19.99" },
    ];
    const { table: next, diff } = applyWrites(table, writes);
    expect(next.headers).toEqual(["name", "quantity", "price", "Total"]);
    expect(next.rows).toEqual([
      ["Widget", "2", "9.99", "19.98"],
      ["Gadget", "5", "4.50", "22.50"],
      ["Gizmo, Deluxe", "1", "19.99", "19.99"],
    ]);
    expect(diff.every((entry) => entry.before === null)).toBe(true);
  });

  it("appends a new summary row past the last data row", () => {
    const table = sampleTable();
    const { table: next } = applyWrites(table, [
      { ref: "A5", value: "TOTAL" },
      { ref: "B5", value: "8" },
    ]);
    expect(next.rows).toHaveLength(4);
    expect(next.rows[3]).toEqual(["TOTAL", "8", ""]);
  });

  it("throws on a malformed ref", () => {
    const table = sampleTable();
    expect(() => applyWrites(table, [{ ref: "not-a-ref", value: "x" }])).toThrow();
  });
});

describe("parseSpreadsheetCopilotResponse", () => {
  const table = sampleTable();

  it("parses a well-formed derived_column response and always cites a range", () => {
    const content = JSON.stringify({
      kind: "derived_column",
      title: "Add line total",
      explanation: "quantity * price per row",
      citedReadRanges: ["B2:B4", "C2:C4"],
      writes: [
        { ref: "D1", value: "Line Total" },
        { ref: "D2", value: "19.98" },
        { ref: "D3", value: "22.50" },
        { ref: "D4", value: "19.99" },
      ],
    });
    const proposal = parseSpreadsheetCopilotResponse(content, table);
    expect(proposal).not.toBeNull();
    expect(proposal!.kind).toBe("derived_column");
    expect(proposal!.citedRanges).toEqual(expect.arrayContaining(["B2:B4", "C2:C4", "D1:D4"]));
    expect(proposal!.proposedTable.headers).toContain("Line Total");
  });

  it("recovers JSON embedded in surrounding prose", () => {
    const content = `Sure, here you go:\n${JSON.stringify({
      kind: "cleanup",
      title: "Fix casing",
      explanation: "normalize name casing",
      citedReadRanges: ["A2:A4"],
      writes: [{ ref: "A2", value: "Widget" }],
    })}\nLet me know if you need anything else.`;
    const proposal = parseSpreadsheetCopilotResponse(content, table);
    expect(proposal).not.toBeNull();
    expect(proposal!.kind).toBe("cleanup");
  });

  it("still cites the write range even when the model omits/fabricates read ranges", () => {
    const content = JSON.stringify({
      kind: "aggregate_summary",
      title: "Total quantity",
      explanation: "sum of quantity column",
      citedReadRanges: ["Z1:Z999"], // out of bounds -> dropped
      writes: [{ ref: "B5", value: "8" }],
    });
    const proposal = parseSpreadsheetCopilotResponse(content, table);
    expect(proposal).not.toBeNull();
    expect(proposal!.citedRanges).toEqual(["B5"]);
  });

  it("returns null when writes is empty", () => {
    const content = JSON.stringify({ kind: "cleanup", title: "x", explanation: "y", citedReadRanges: ["A2"], writes: [] });
    expect(parseSpreadsheetCopilotResponse(content, table)).toBeNull();
  });

  it("returns null when a write ref is malformed", () => {
    const content = JSON.stringify({
      kind: "cleanup",
      title: "x",
      explanation: "y",
      citedReadRanges: ["A2"],
      writes: [{ ref: "!!", value: "x" }],
    });
    expect(parseSpreadsheetCopilotResponse(content, table)).toBeNull();
  });

  it("returns null for unparseable content", () => {
    expect(parseSpreadsheetCopilotResponse("not json at all", table)).toBeNull();
  });

  it("returns null for an unrecognized operation kind", () => {
    const content = JSON.stringify({
      kind: "delete_everything",
      title: "x",
      explanation: "y",
      citedReadRanges: ["A2"],
      writes: [{ ref: "A2", value: "x" }],
    });
    expect(parseSpreadsheetCopilotResponse(content, table)).toBeNull();
  });
});

describe("proposeOperation", () => {
  const table = sampleTable();

  it("throws for a blank instruction without calling the model", async () => {
    const callModel = async () => ({ content: "", streamError: null });
    await expect(proposeOperation(table, "   ", callModel)).rejects.toThrow(/describe the operation/i);
  });

  it("surfaces a stream error as a thrown error", async () => {
    const callModel = async () => ({ content: "", streamError: "model unavailable" });
    await expect(proposeOperation(table, "add a total column", callModel)).rejects.toThrow("model unavailable");
  });

  it("returns a validated proposal on a well-formed reply", async () => {
    const callModel = async () => ({
      content: JSON.stringify({
        kind: "derived_column",
        title: "Add line total",
        explanation: "quantity * price",
        citedReadRanges: ["B2:B4", "C2:C4"],
        writes: [
          { ref: "D1", value: "Line Total" },
          { ref: "D2", value: "19.98" },
          { ref: "D3", value: "22.50" },
          { ref: "D4", value: "19.99" },
        ],
      }),
      streamError: null,
    });
    const proposal = await proposeOperation(table, "add a line total column", callModel);
    expect(proposal.kind).toBe("derived_column");
    expect(proposal.citedRanges.length).toBeGreaterThan(0);
  });

  it("throws when the model reply doesn't parse into a usable proposal", async () => {
    const callModel = async () => ({ content: "nonsense", streamError: null });
    await expect(proposeOperation(table, "add a total column", callModel)).rejects.toThrow(/did not return a usable/i);
  });
});
