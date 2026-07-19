import { beforeEach, describe, expect, it, vi } from "vitest";

// `dbAdminGuardrailsStore.ts` drives its natural-language -> SQL proposal
// call through `agentLoop.ts`'s `resolveTarget` + `turnEngine.ts`'s
// `attemptStream` — exactly the same pair `sopCompilerStore.ts`'s `compile`
// uses — mocked here so these tests pin the STORE's own gating behavior
// (dry-run-before-apply, backup-before-write, approval) without needing a
// real streaming provider or a real file on disk.
const resolveTargetMock = vi.fn();
vi.mock("../lib/agentLoop", () => ({
  resolveTarget: (...args: unknown[]) => resolveTargetMock(...args),
}));

const attemptStreamMock = vi.fn();
vi.mock("../lib/turnEngine", () => ({
  attemptStream: (...args: unknown[]) => attemptStreamMock(...args),
}));

const dialogOpenMock = vi.fn();
vi.mock("@tauri-apps/plugin-dialog", () => ({
  open: (...args: unknown[]) => dialogOpenMock(...args),
}));

const readFileMock = vi.fn();
const writeFileMock = vi.fn();
const copyFileMock = vi.fn();
const statMock = vi.fn();
vi.mock("@tauri-apps/plugin-fs", () => ({
  readFile: (...args: unknown[]) => readFileMock(...args),
  writeFile: (...args: unknown[]) => writeFileMock(...args),
  copyFile: (...args: unknown[]) => copyFileMock(...args),
  stat: (...args: unknown[]) => statMock(...args),
}));

import { useDbAdminGuardrailsStore } from "./dbAdminGuardrailsStore";
import { openDatabaseFromBytes } from "../lib/dbAdminGuardrails";

/** Builds a real in-memory sql.js database's raw bytes (one `customers`
 * table with a PII-shaped `email` column and two rows) — used as the
 * `readFile` mock's return value so the store's own `openDatabaseFromBytes`
 * + `introspectSchema` calls run against a real (small) SQLite file. */
async function buildFixtureDbBytes(): Promise<Uint8Array> {
  const db = await openDatabaseFromBytes(new Uint8Array());
  db.run("CREATE TABLE customers (id INTEGER PRIMARY KEY, full_name TEXT, email TEXT)");
  db.run("INSERT INTO customers (full_name, email) VALUES ('Ada Lovelace', 'ada@example.com')");
  db.run("INSERT INTO customers (full_name, email) VALUES ('Alan Turing', 'alan@example.com')");
  const bytes = db.export();
  db.close();
  return bytes;
}

async function openFixtureFile() {
  dialogOpenMock.mockResolvedValueOnce("/Users/me/app.sqlite");
  statMock.mockResolvedValueOnce({ size: 4096 });
  readFileMock.mockResolvedValueOnce(await buildFixtureDbBytes());
  await useDbAdminGuardrailsStore.getState().openFile();
}

describe("dbAdminGuardrailsStore", () => {
  beforeEach(() => {
    resolveTargetMock.mockReset();
    attemptStreamMock.mockReset();
    dialogOpenMock.mockReset();
    readFileMock.mockReset();
    writeFileMock.mockReset();
    copyFileMock.mockReset();
    statMock.mockReset();
    resolveTargetMock.mockResolvedValue({ kind: "local", baseUrl: "http://localhost:8090" });
    useDbAdminGuardrailsStore.getState().closeFile();
    useDbAdminGuardrailsStore.setState({ nlRequest: "" });
  });

  it("opens a file, introspects its schema, and flags PII columns", async () => {
    await openFixtureFile();
    const state = useDbAdminGuardrailsStore.getState();
    expect(state.fileName).toBe("app.sqlite");
    expect(state.filePath).toBe("/Users/me/app.sqlite");
    expect(state.tables).toHaveLength(1);
    expect(state.tables[0].name).toBe("customers");
    const email = state.tables[0].columns.find((c) => c.name === "email");
    expect(email?.pii).toBe(true);
  });

  it("does nothing when the user cancels the file picker", async () => {
    dialogOpenMock.mockResolvedValueOnce(null);
    await useDbAdminGuardrailsStore.getState().openFile();
    expect(statMock).not.toHaveBeenCalled();
    expect(useDbAdminGuardrailsStore.getState().filePath).toBeNull();
  });

  it("rejects a file above the size limit without reading it", async () => {
    dialogOpenMock.mockResolvedValueOnce("/Users/me/huge.sqlite");
    statMock.mockResolvedValueOnce({ size: 500 * 1024 * 1024 });
    await useDbAdminGuardrailsStore.getState().openFile();
    expect(readFileMock).not.toHaveBeenCalled();
    expect(useDbAdminGuardrailsStore.getState().proposalError).toMatch(/larger than/i);
  });

  it("refuses to propose without an open file", async () => {
    useDbAdminGuardrailsStore.getState().setNlRequest("list customers");
    await useDbAdminGuardrailsStore.getState().propose();
    expect(attemptStreamMock).not.toHaveBeenCalled();
    expect(useDbAdminGuardrailsStore.getState().proposalError).toMatch(/open a sqlite database file/i);
  });

  it("a proposed SELECT runs immediately with results, no gate", async () => {
    await openFixtureFile();
    attemptStreamMock.mockResolvedValue({
      content: JSON.stringify({ sql: "SELECT id, full_name FROM customers ORDER BY id", explanation: "Lists customers." }),
      streamError: null,
      toolCalls: [],
      contentStarted: true,
    });
    useDbAdminGuardrailsStore.getState().setNlRequest("list customers");

    await useDbAdminGuardrailsStore.getState().propose();

    const state = useDbAdminGuardrailsStore.getState();
    expect(state.statementKind).toBe("select");
    expect(state.selectResult?.rows).toEqual([
      [1, "Ada Lovelace"],
      [2, "Alan Turing"],
    ]);
    expect(state.dryRun).toBeNull();
    // `recordUsage` (8th positional arg) must be threaded through as `false`.
    const call = attemptStreamMock.mock.calls[0];
    expect(call[7]).toBe(false);
  });

  it("a proposed write requires a dry run before it can be approved", async () => {
    await openFixtureFile();
    attemptStreamMock.mockResolvedValue({
      content: JSON.stringify({
        sql: "UPDATE customers SET full_name = 'Changed' WHERE id = 1",
        explanation: "Renames customer 1.",
      }),
      streamError: null,
      toolCalls: [],
      contentStarted: true,
    });
    useDbAdminGuardrailsStore.getState().setNlRequest("rename customer 1");
    await useDbAdminGuardrailsStore.getState().propose();

    expect(useDbAdminGuardrailsStore.getState().statementKind).toBe("write");

    // Approving before a dry run must be rejected, and must never write.
    await useDbAdminGuardrailsStore.getState().approveApply();
    expect(copyFileMock).not.toHaveBeenCalled();
    expect(writeFileMock).not.toHaveBeenCalled();
    expect(useDbAdminGuardrailsStore.getState().applyError).toMatch(/dry-run/i);

    await useDbAdminGuardrailsStore.getState().runDryRun();
    const afterDryRun = useDbAdminGuardrailsStore.getState();
    expect(afterDryRun.dryRun?.rowsAffected).toBe(1);
    expect(afterDryRun.dryRun?.piiColumns).toEqual(expect.arrayContaining(["full_name", "email"]));

    // Now approval backs up the original file, then writes the real change.
    copyFileMock.mockResolvedValue(undefined);
    writeFileMock.mockResolvedValue(undefined);
    await useDbAdminGuardrailsStore.getState().approveApply();

    const finalState = useDbAdminGuardrailsStore.getState();
    expect(copyFileMock).toHaveBeenCalledTimes(1);
    expect(copyFileMock.mock.calls[0][0]).toBe("/Users/me/app.sqlite");
    expect(String(copyFileMock.mock.calls[0][1])).toMatch(/app\.sqlite\.bak-/);
    expect(writeFileMock).toHaveBeenCalledTimes(1);
    expect(finalState.history).toHaveLength(1);
    expect(finalState.history[0].rowsAffected).toBe(1);
    expect(finalState.lastBackupPath).toMatch(/app\.sqlite\.bak-/);
    // The gate resets after a successful apply — no stale proposal/dry-run
    // lingers to be re-approved.
    expect(finalState.proposedSql).toBeNull();
    expect(finalState.dryRun).toBeNull();
  });

  it("cancelProposal clears the proposal/dry-run without ever writing", async () => {
    await openFixtureFile();
    attemptStreamMock.mockResolvedValue({
      content: JSON.stringify({ sql: "DELETE FROM customers WHERE id = 1", explanation: "Removes customer 1." }),
      streamError: null,
      toolCalls: [],
      contentStarted: true,
    });
    useDbAdminGuardrailsStore.getState().setNlRequest("remove customer 1");
    await useDbAdminGuardrailsStore.getState().propose();
    await useDbAdminGuardrailsStore.getState().runDryRun();
    expect(useDbAdminGuardrailsStore.getState().dryRun).not.toBeNull();

    useDbAdminGuardrailsStore.getState().cancelProposal();

    const state = useDbAdminGuardrailsStore.getState();
    expect(state.proposedSql).toBeNull();
    expect(state.dryRun).toBeNull();
    expect(copyFileMock).not.toHaveBeenCalled();
    expect(writeFileMock).not.toHaveBeenCalled();
  });

  it("closing the file clears schema, proposal, and history", async () => {
    await openFixtureFile();
    useDbAdminGuardrailsStore.getState().closeFile();
    const state = useDbAdminGuardrailsStore.getState();
    expect(state.filePath).toBeNull();
    expect(state.tables).toEqual([]);
    expect(state.history).toEqual([]);
  });
});
