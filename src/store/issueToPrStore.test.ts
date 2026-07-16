import { beforeEach, describe, expect, it, vi } from "vitest";

const api = vi.hoisted(() => ({
  startIssueToPr: vi.fn(),
  getIssueToPrStatus: vi.fn(),
  listIssueToPrRuns: vi.fn(),
  cancelIssueToPr: vi.fn(),
  advanceIssueToPr: vi.fn(),
  runIssueToPrChecks: vi.fn(),
  listenIssueToPrProgress: vi.fn(),
}));

const runner = vi.hoisted(() => ({
  runIssueToPrAgent: vi.fn(),
}));

vi.mock("../lib/issueToPr", async (importOriginal) => ({
  ...(await importOriginal<typeof import("../lib/issueToPr")>()),
  ...api,
}));

vi.mock("../lib/issueToPrRunner", () => runner);

import type { IssueToPrRun } from "../lib/issueToPr";
import { __resetIssueToPrControllersForTests, useIssueToPrStore } from "./issueToPrStore";

function fixtureRun(overrides: Partial<IssueToPrRun> = {}): IssueToPrRun {
  return {
    runId: "i2p-1",
    issueUrl: "https://github.com/owner/repo/issues/7",
    repositorySlug: "owner/repo",
    issueNumber: 7,
    issueTitle: "Fix the bug",
    issueBody: "Steps to reproduce…",
    worktreeId: "wt-1",
    branch: "issue-to-pr/issue-7-abcdefgh",
    workspaceLabel: "wt-1",
    status: "planning",
    prNumber: null,
    prUrl: null,
    checks: [],
    error: null,
    durableRunId: null,
    createdAtMs: 1,
    updatedAtMs: 1,
    ...overrides,
  };
}

/** Flushes the microtask queue enough times for the store's fire-and-forget
 * `driveRun` (kicked off from `start` without being awaited, exactly like a
 * real background run) to reach its own awaited steps. */
async function flush(times = 6): Promise<void> {
  for (let i = 0; i < times; i++) {
    await Promise.resolve();
  }
}

// `issueToPrStore.ts` keeps its listener registration in a module-level
// singleton (`unlisten`) that only ever calls `listenIssueToPrProgress` ONCE
// for the app's whole lifetime — calling `init()` again later is a no-op by
// design. That singleton persists across every test in this file exactly
// like it would across the real app's lifetime, so the captured `onProgress`
// callback below is likewise captured once (whichever test happens to run
// `init()` first) and reused by every later test that needs to deliver a
// progress event, rather than re-captured per test.
let capturedOnProgress: ((run: IssueToPrRun) => void) | null = null;

beforeEach(() => {
  for (const mock of Object.values(api)) mock.mockReset();
  runner.runIssueToPrAgent.mockReset();
  __resetIssueToPrControllersForTests();
  api.listenIssueToPrProgress.mockImplementation(async (onProgress: (run: IssueToPrRun) => void) => {
    capturedOnProgress = onProgress;
    return () => {};
  });
  api.listIssueToPrRuns.mockResolvedValue([]);
  // Safe fallbacks so an incidental resume-on-init drive (see the "resumes"
  // tests below, and `init()`'s own auto-resume loop firing for any
  // non-terminal run any test happens to list) never crashes on an
  // unconfigured mock returning `undefined` instead of a `Promise`.
  api.advanceIssueToPr.mockResolvedValue(fixtureRun());
  api.runIssueToPrChecks.mockResolvedValue(fixtureRun());
  api.getIssueToPrStatus.mockResolvedValue(fixtureRun());
  runner.runIssueToPrAgent.mockResolvedValue({
    outcome: "cancelled",
    summary: "Cancelled by the user.",
    durableRunId: null,
  });
  useIssueToPrStore.setState({
    runs: [],
    selectedRunId: null,
    activityByRun: {},
    busy: {},
    error: null,
    notice: null,
  });
});

describe("issueToPrStore", () => {
  it("init wires the progress listener exactly once and loads the run list", async () => {
    api.listIssueToPrRuns.mockResolvedValue([fixtureRun()]);
    await useIssueToPrStore.getState().init();
    await useIssueToPrStore.getState().init();
    expect(api.listenIssueToPrProgress).toHaveBeenCalledOnce();
    expect(useIssueToPrStore.getState().runs).toHaveLength(1);
    expect(useIssueToPrStore.getState().selectedRunId).toBe("i2p-1");
  });

  it("starting a run selects it immediately and drives it through implementing to checks", async () => {
    const run = fixtureRun();
    api.startIssueToPr.mockResolvedValue(run);
    api.advanceIssueToPr.mockResolvedValue({ ...run, status: "implementing" });
    runner.runIssueToPrAgent.mockResolvedValue({
      outcome: "completed",
      summary: "Implemented the fix and ran checks.",
      durableRunId: "run-abc",
    });
    api.runIssueToPrChecks.mockResolvedValue({ ...run, status: "opening_pr" });

    await useIssueToPrStore.getState().start(run.issueUrl);
    expect(useIssueToPrStore.getState().selectedRunId).toBe(run.runId);
    expect(useIssueToPrStore.getState().runs[0]?.status).toBe("planning");

    await flush();

    expect(api.advanceIssueToPr).toHaveBeenCalledWith(run.runId, "implementing");
    expect(runner.runIssueToPrAgent).toHaveBeenCalledWith(
      expect.objectContaining({ runId: run.runId, repositorySlug: run.repositorySlug }),
    );
    expect(api.runIssueToPrChecks).toHaveBeenCalledWith(run.runId);
  });

  it("reports an agent error onto the run as a failed transition instead of running checks", async () => {
    const run = fixtureRun();
    api.startIssueToPr.mockResolvedValue(run);
    api.advanceIssueToPr.mockResolvedValue({ ...run, status: "implementing" });
    runner.runIssueToPrAgent.mockResolvedValue({
      outcome: "error",
      summary: "The model stream failed.",
      durableRunId: null,
    });

    await useIssueToPrStore.getState().start(run.issueUrl);
    await flush();

    expect(api.advanceIssueToPr).toHaveBeenCalledWith(
      run.runId,
      "failed",
      expect.objectContaining({ error: "The model stream failed." }),
    );
    expect(api.runIssueToPrChecks).not.toHaveBeenCalled();
  });

  it("a cancelled agent outcome does not re-report the run — cancel() already owns that transition", async () => {
    const run = fixtureRun();
    api.startIssueToPr.mockResolvedValue(run);
    api.advanceIssueToPr.mockResolvedValue({ ...run, status: "implementing" });
    runner.runIssueToPrAgent.mockResolvedValue({
      outcome: "cancelled",
      summary: "Cancelled by the user.",
      durableRunId: null,
    });

    await useIssueToPrStore.getState().start(run.issueUrl);
    await flush();

    expect(api.advanceIssueToPr).toHaveBeenCalledTimes(1); // only the initial "implementing" move
    expect(api.runIssueToPrChecks).not.toHaveBeenCalled();
  });

  it("cancel aborts the in-flight run's signal and persists the cancellation", async () => {
    const run = fixtureRun({ status: "implementing" });
    api.startIssueToPr.mockResolvedValue(run);
    api.advanceIssueToPr.mockResolvedValue({ ...run, status: "implementing" });
    let capturedSignal: AbortSignal | undefined;
    runner.runIssueToPrAgent.mockImplementation(async (params: { signal: AbortSignal }) => {
      capturedSignal = params.signal;
      // Never resolves within this test — cancel() must still work while
      // the headless agent loop is genuinely in flight.
      return new Promise(() => {});
    });

    await useIssueToPrStore.getState().start(run.issueUrl);
    await flush();
    expect(capturedSignal?.aborted).toBe(false);

    api.cancelIssueToPr.mockResolvedValue({ ...run, status: "cancelled" });
    await useIssueToPrStore.getState().cancel(run.runId);

    expect(capturedSignal?.aborted).toBe(true);
    expect(api.cancelIssueToPr).toHaveBeenCalledWith(run.runId);
    expect(useIssueToPrStore.getState().runs.find((r) => r.runId === run.runId)?.status).toBe("cancelled");
  });

  it("cancel surfaces an error onto the store without throwing to the caller's caller", async () => {
    api.cancelIssueToPr.mockRejectedValue(new Error("run not found"));
    await expect(useIssueToPrStore.getState().cancel("missing")).rejects.toThrow("run not found");
    expect(useIssueToPrStore.getState().error).toBe("run not found");
  });

  it("progress events upsert the run and clear its tracked activity once terminal", async () => {
    const run = fixtureRun();
    await useIssueToPrStore.getState().init();
    expect(capturedOnProgress).not.toBeNull();
    useIssueToPrStore.setState({ activityByRun: { [run.runId]: "run_shell(pnpm test)" } });

    capturedOnProgress!({ ...run, status: "checking" });
    expect(useIssueToPrStore.getState().runs.find((r) => r.runId === run.runId)?.status).toBe("checking");
    expect(useIssueToPrStore.getState().activityByRun[run.runId]).toBe("run_shell(pnpm test)");

    capturedOnProgress!({ ...run, status: "done" });
    expect(useIssueToPrStore.getState().activityByRun[run.runId]).toBeUndefined();
  });

  it("markPrOpened and markDone advance the run through the recorded terminal statuses", async () => {
    const run = fixtureRun({ status: "opening_pr" });
    useIssueToPrStore.setState({ runs: [run] });
    api.advanceIssueToPr.mockResolvedValueOnce({ ...run, status: "awaiting_review", prNumber: 42, prUrl: "https://github.com/owner/repo/pull/42" });
    await useIssueToPrStore.getState().markPrOpened(run.runId, 42, "https://github.com/owner/repo/pull/42");
    expect(api.advanceIssueToPr).toHaveBeenCalledWith(run.runId, "awaiting_review", { prNumber: 42, prUrl: "https://github.com/owner/repo/pull/42" });

    api.advanceIssueToPr.mockResolvedValueOnce({ ...run, status: "done" });
    await useIssueToPrStore.getState().markDone(run.runId);
    expect(useIssueToPrStore.getState().runs.find((r) => r.runId === run.runId)?.status).toBe("done");
  });

  it("a completed agent turn self-reports its durableRunId onto the run's CURRENT status, not the stale one captured at start()", async () => {
    const run = fixtureRun();
    api.startIssueToPr.mockResolvedValue(run);
    api.advanceIssueToPr.mockResolvedValue({ ...run, status: "implementing" });
    runner.runIssueToPrAgent.mockResolvedValue({
      outcome: "completed",
      summary: "Implemented the fix.",
      durableRunId: "run-abc",
    });
    api.runIssueToPrChecks.mockResolvedValue({ ...run, status: "opening_pr" });

    await useIssueToPrStore.getState().start(run.issueUrl);
    await flush();

    expect(api.advanceIssueToPr).toHaveBeenCalledWith(run.runId, "implementing", { durableRunId: "run-abc" });
  });

  it("init re-drives a run left in implementing from a previous process (no controller tracked for it yet)", async () => {
    const run = fixtureRun({ status: "implementing" });
    api.listIssueToPrRuns.mockResolvedValue([run]);
    runner.runIssueToPrAgent.mockResolvedValue({
      outcome: "completed",
      summary: "Implemented the fix.",
      durableRunId: null,
    });
    api.runIssueToPrChecks.mockResolvedValue({ ...run, status: "opening_pr" });

    await useIssueToPrStore.getState().init();
    await flush();

    expect(api.advanceIssueToPr).toHaveBeenCalledWith(run.runId, "implementing");
    expect(runner.runIssueToPrAgent).toHaveBeenCalledWith(expect.objectContaining({ runId: run.runId }));
    expect(api.runIssueToPrChecks).toHaveBeenCalledWith(run.runId);
  });

  it("init re-runs checks (without re-invoking the agent) for a run left in checking, after reconfirming the point-in-time status", async () => {
    const run = fixtureRun({ status: "checking" });
    api.listIssueToPrRuns.mockResolvedValue([run]);
    api.getIssueToPrStatus.mockResolvedValue(run);

    await useIssueToPrStore.getState().init();
    await flush();

    expect(api.getIssueToPrStatus).toHaveBeenCalledWith(run.runId);
    expect(api.runIssueToPrChecks).toHaveBeenCalledWith(run.runId);
    expect(runner.runIssueToPrAgent).not.toHaveBeenCalled();
  });

  it("init does not re-run checks for a run in checking if a point-in-time reconfirm shows it already moved on", async () => {
    const run = fixtureRun({ status: "checking" });
    api.listIssueToPrRuns.mockResolvedValue([run]);
    api.getIssueToPrStatus.mockResolvedValue({ ...run, status: "opening_pr" });

    await useIssueToPrStore.getState().init();
    await flush();

    expect(api.runIssueToPrChecks).not.toHaveBeenCalled();
  });

  it("init never re-drives a run waiting on a human action (opening_pr or awaiting_review)", async () => {
    const openingPr = fixtureRun({ runId: "i2p-open", status: "opening_pr" });
    const awaitingReview = fixtureRun({ runId: "i2p-review", status: "awaiting_review" });
    api.listIssueToPrRuns.mockResolvedValue([openingPr, awaitingReview]);

    await useIssueToPrStore.getState().init();
    await flush();

    expect(api.getIssueToPrStatus).not.toHaveBeenCalled();
    expect(api.runIssueToPrChecks).not.toHaveBeenCalled();
    expect(runner.runIssueToPrAgent).not.toHaveBeenCalled();
  });

  it("init never re-drives a run that already has an in-flight controller tracked for it", async () => {
    const run = fixtureRun({ status: "implementing" });
    api.startIssueToPr.mockResolvedValue(run);
    api.advanceIssueToPr.mockResolvedValue({ ...run, status: "implementing" });
    runner.runIssueToPrAgent.mockImplementation(() => new Promise(() => {})); // never resolves — still "in flight"
    await useIssueToPrStore.getState().start(run.issueUrl);
    await flush();
    const callsBeforeInit = runner.runIssueToPrAgent.mock.calls.length;

    api.listIssueToPrRuns.mockResolvedValue([run]);
    await useIssueToPrStore.getState().init();
    await flush();

    expect(runner.runIssueToPrAgent.mock.calls.length).toBe(callsBeforeInit);
  });
});
