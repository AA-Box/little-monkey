/**
 * The wall-budget decision table, and the three properties that make it safe to
 * ship a kill switch on a 2-second timer:
 *
 * 1. It fires for nobody until a budget is set. Every assertion that the bound
 *    *works* is paired with one that an unset or unexpired row is untouched —
 *    without which a budget of zero, or one that fired on every sweep, would pass
 *    just as happily.
 * 2. `workflow_node` is not in the allow-list. Asserted directly, because that
 *    kind has no primitive to deliver a stop to: a latch on a node row would be
 *    committed durably and never delivered.
 * 3. A budget kill is distinguishable from a human's Stop after the fact, in both
 *    directions — that is the counter-test that stops "everything is a limit
 *    kill" from passing.
 *
 * The clock is injected as an argument rather than faked, matching
 * `processSignalDelivery.test.ts`: the verdict is pure, so the instant under test
 * is just a number.
 */
import { beforeEach, describe, expect, it, vi } from "vitest";

const invokeMock = vi.fn();
vi.mock("@tauri-apps/api/core", () => ({
  invoke: (...args: unknown[]) => invokeMock(...args),
  isTauri: () => true,
}));
vi.mock("@tauri-apps/api/event", () => ({ listen: vi.fn(async () => () => {}) }));

import {
  WALL_BUDGET_KINDS,
  WALL_BUDGET_STOP_REASON_PREFIX,
  enforceWallBudgets,
  isWallBudgetKill,
  wallBudgetStopReason,
  wallBudgetVerdict,
} from "./processWallBudget";
import { admitProcess } from "./processTable";
import type { ProcessKind, ProcessRecord } from "./processTable";
import {
  DEFAULT_PROCESS_WALL_BUDGET_HOURS,
  useSettingsStore,
} from "../store/settingsStore";

const MAIN = { ownsGlobalKinds: true };
const SECONDARY = { ownsGlobalKinds: false };

/** A row that started at t=1000 with a one-minute budget, unless overridden. */
function record(overrides: Partial<ProcessRecord> = {}): ProcessRecord {
  return {
    processId: "proc-1",
    parentProcessId: null,
    kind: "chat_turn",
    externalId: "ext-1",
    state: "running",
    runId: null,
    workspace: null,
    profile: null,
    nativePid: null,
    limits: { maxWallMs: 60_000 },
    signalIntent: { stopRequested: false, suspendRequested: false, killRequested: false },
    signalReason: null,
    signalRequestedAtMs: null,
    exit: null,
    createdAtMs: 500,
    updatedAtMs: 1_000,
    startedAtMs: 1_000,
    exitedAtMs: null,
    ...overrides,
  };
}

const STARTED = 1_000;
const WITHIN = STARTED + 59_999;
const PAST = STARTED + 60_001;

beforeEach(() => {
  invokeMock.mockReset();
  invokeMock.mockResolvedValue(true);
});

describe("the verdict", () => {
  it("fires once the declared budget has elapsed", () => {
    expect(wallBudgetVerdict(record(), PAST)).toBe("exceeded");
  });

  it("leaves a process inside its budget alone", () => {
    // The counter-test for the assertion above: a bound of zero, or one measured
    // from the wrong stamp, would fire here too.
    expect(wallBudgetVerdict(record(), WITHIN)).toBe("within-budget");
  });

  it("counts from the moment it started running, not from admission", () => {
    // Admitted long before it ran. Measuring from `createdAtMs` would make the
    // queue wait spend the budget.
    const queued = record({ createdAtMs: 0, startedAtMs: 100_000 });
    expect(wallBudgetVerdict(queued, 130_000)).toBe("within-budget");
    expect(wallBudgetVerdict(queued, 161_000)).toBe("exceeded");
  });

  it("treats the budget boundary as over", () => {
    expect(wallBudgetVerdict(record(), STARTED + 60_000)).toBe("exceeded");
    expect(wallBudgetVerdict(record(), STARTED + 59_999)).toBe("within-budget");
  });

  it("does nothing at all when no budget is declared", () => {
    // The shipped state of every kind in the allow-list: the mechanism is
    // enforced and the number is unset, so this is the verdict that matters most.
    expect(wallBudgetVerdict(record({ limits: {} }), Number.MAX_SAFE_INTEGER)).toBe("unset");
    expect(wallBudgetVerdict(record({ limits: { maxWallMs: null } }), PAST)).toBe("unset");
  });

  it("reads a non-positive budget as unset rather than as kill-on-sight", () => {
    // The ledger's CHECK forbids one, so a zero here came from a caller inventing
    // it — and reading it as a budget would kill every process at admission.
    expect(wallBudgetVerdict(record({ limits: { maxWallMs: 0 } }), STARTED)).toBe("unset");
    expect(wallBudgetVerdict(record({ limits: { maxWallMs: -1 } }), PAST)).toBe("unset");
  });

  it("does not trip a suspended row however long it has been parked", () => {
    const parked = record({ state: "suspended" });
    expect(wallBudgetVerdict(parked, PAST)).toBe("parked");
    expect(wallBudgetVerdict(parked, STARTED + 86_400_000)).toBe("parked");
  });

  it("does trip a row still running with a pause merely requested", () => {
    // `pause_pending` is not parked: the loop has not reached its safe point, so
    // it is still doing real work and still spending the budget.
    const pausePending = record({
      state: "running",
      signalIntent: { stopRequested: false, suspendRequested: true, killRequested: false },
    });
    expect(wallBudgetVerdict(pausePending, PAST)).toBe("exceeded");
  });

  it("does not re-latch a row that is already stopping", () => {
    const stopping = record({
      signalIntent: { stopRequested: true, suspendRequested: false, killRequested: false },
    });
    expect(wallBudgetVerdict(stopping, PAST)).toBe("already-stopping");
  });

  it("ignores a row that never started running", () => {
    // Nothing to measure, and a stop latched here could not be delivered: the
    // loop has not registered its cancellation entry yet.
    expect(wallBudgetVerdict(record({ state: "admitted", startedAtMs: null }), PAST)).toBe(
      "not-started",
    );
  });

  it("ignores an exited row", () => {
    expect(wallBudgetVerdict(record({ state: "exited" }), PAST)).toBe("exited");
  });

  it("does not kill anyone because the clock went backwards", () => {
    expect(wallBudgetVerdict(record({ startedAtMs: 10_000_000 }), STARTED)).toBe("within-budget");
  });
});

describe("which kinds a budget applies to", () => {
  /**
   * `true` where a wall budget is enforced by this module. `satisfies
   * Record<ProcessKind, boolean>` is the point: a tenth kind cannot join the
   * union without a decision recorded here.
   */
  const BOUNDED_HERE = {
    chat_turn: true,
    subagent: true,
    crew_member: true,
    side_task: true,
    // Already bounded elsewhere: the executor's 24h run budget, the per-node
    // timeout, the daemon's own watchdog.
    workflow_run: false,
    workflow_node: false,
    daemon_job: false,
    // Records that a controller asked for work, not the work itself.
    remote_run: false,
    // Spawned with no timeout on purpose, so it can outlive its turn.
    background_shell: false,
    // Bounded by their own owners, not by this sweep: a foreground shell by the
    // resource controller its spawn site holds, a browser session by its
    // watchdog. A second wall sweep over either would race the first.
    foreground_shell: false,
    browser_session: false,
    // Bounded by the resource controller each one's runner holds, with its own
    // deadline as the wall limit. A second sweep here would race that one.
    verify_command: false,
    hook_command: false,
    sandbox_run: false,
  } satisfies Record<ProcessKind, boolean>;

  it("bounds exactly the kinds this WebView hosts", () => {
    const expected = Object.entries(BOUNDED_HERE)
      .filter(([, bounded]) => bounded)
      .map(([kind]) => kind)
      .sort();
    expect([...WALL_BUDGET_KINDS].sort()).toEqual(expected);
  });

  it("excludes a workflow node, which has no primitive to deliver a stop to", () => {
    // The failure this prevents: `deliverProcessSignal` answers "no-primitive"
    // for a node, so a latched stop would be committed and never delivered,
    // leaving the row reading `stopping` forever.
    expect(WALL_BUDGET_KINDS).not.toContain("workflow_node");
    const node = record({ kind: "workflow_node", limits: { maxWallMs: 1 } });
    expect(wallBudgetVerdict(node, PAST)).toBe("not-applicable");
  });

  it("refuses every kind it does not own, however far past a budget", () => {
    for (const [kind, bounded] of Object.entries(BOUNDED_HERE)) {
      const verdict = wallBudgetVerdict(
        record({ kind: kind as ProcessKind, limits: { maxWallMs: 1 } }),
        PAST,
      );
      if (bounded) expect(verdict, kind).toBe("exceeded");
      else expect(verdict, kind).toBe("not-applicable");
    }
  });
});

describe("the exit a budget kill records", () => {
  // The classification itself lives in Rust (`process_table.rs`'s
  // `upgrade_a_budget_kill`), because `transition` already reads `signal_reason`
  // on its way to writing the exit and so covers every host rather than only the
  // four WebView loops. What this side owns is the marker, so what is asserted
  // here is that the marker survives a round trip and that the literal still
  // matches the one Rust reads.

  it("writes the exact prefix the Rust side reads", () => {
    // Cross-language contract with no compiler to enforce it: renaming either
    // literal would silently stop budget kills being recorded as
    // `limit_exceeded`. `process_table.rs` pins the same string.
    expect(WALL_BUDGET_STOP_REASON_PREFIX).toBe("wall budget exceeded: max_wall_ms");
  });

  it("recognises its own reason through a round trip", () => {
    const reason = wallBudgetStopReason(record(), PAST);
    expect(reason).toContain("max_wall_ms=60000ms");
    expect(reason).toContain("ran 60001ms");
    expect(isWallBudgetKill(record({ signalReason: reason }))).toBe(true);
  });
});

describe("the sweep", () => {
  function withLiveRows(rows: ProcessRecord[]): void {
    invokeMock.mockImplementation((command: string) =>
      command === "process_list" ? Promise.resolve(rows) : Promise.resolve(true),
    );
  }

  it("latches a durable stop on the row that blew its budget", async () => {
    withLiveRows([record({ processId: "proc-turn", externalId: "turn-1" })]);

    const verdicts = await enforceWallBudgets(MAIN, PAST);

    expect(verdicts).toEqual(["exceeded"]);
    expect(invokeMock).toHaveBeenCalledWith("process_signal", {
      processId: "proc-turn",
      signal: "stop",
      // `stop`, not `kill`: none of these kinds owns an OS process, and
      // `signal_support` refuses `kill` for all four.
      reason: expect.stringContaining(WALL_BUDGET_STOP_REASON_PREFIX),
    });
  });

  it("asks only for the live rows of the kinds it bounds", async () => {
    withLiveRows([]);
    await enforceWallBudgets(MAIN, PAST);
    expect(invokeMock).toHaveBeenCalledWith("process_list", {
      args: { kinds: [...WALL_BUDGET_KINDS], liveOnly: true },
    });
  });

  it("signals nothing for rows inside their budget or with none set", async () => {
    withLiveRows([
      record({ processId: "proc-young" }),
      record({ processId: "proc-unbounded", limits: {} }),
    ]);

    const verdicts = await enforceWallBudgets(MAIN, WITHIN);

    expect(verdicts).toEqual(["within-budget", "unset"]);
    expect(invokeMock).not.toHaveBeenCalledWith("process_signal", expect.anything());
  });

  it("latches once, not once per sweep, while the stop is undelivered", async () => {
    // The row keeps reading as live and past budget for as long as delivery
    // takes — which behind a 120-second shell timeout is a minute of sweeps.
    const stopping = record({
      signalIntent: { stopRequested: true, suspendRequested: false, killRequested: false },
      signalReason: wallBudgetStopReason(record(), PAST),
    });
    withLiveRows([stopping]);

    await enforceWallBudgets(MAIN, PAST + 2_000);
    await enforceWallBudgets(MAIN, PAST + 4_000);

    expect(
      invokeMock.mock.calls.filter(([command]) => command === "process_signal"),
    ).toHaveLength(0);
  });

  it("leaves the decision to the one window that owns it", async () => {
    // Two windows reading the same timestamps would latch the same row twice.
    // Delivery is still window-local — whoever holds the controller — so a
    // secondary window skips the read entirely rather than reading and abstaining.
    withLiveRows([record({ processId: "proc-overdue" })]);
    const verdicts = await enforceWallBudgets(SECONDARY, PAST);
    expect(verdicts).toEqual([]);
    expect(invokeMock).not.toHaveBeenCalled();
  });

  it("survives a refused latch without abandoning the rest of the sweep", async () => {
    const warn = vi.spyOn(console, "warn").mockImplementation(() => {});
    invokeMock.mockImplementation((command: string) =>
      command === "process_list"
        ? Promise.resolve([record({ processId: "a" }), record({ processId: "b" })])
        : Promise.reject(new Error("signal refused")),
    );

    const verdicts = await enforceWallBudgets(MAIN, PAST);

    expect(verdicts).toEqual(["exceeded", "exceeded"]);
    expect(invokeMock.mock.calls.filter(([command]) => command === "process_signal")).toHaveLength(
      2,
    );
    expect(warn).toHaveBeenCalled();
    warn.mockRestore();
  });

  it("survives an unavailable ledger", async () => {
    const warn = vi.spyOn(console, "warn").mockImplementation(() => {});
    invokeMock.mockRejectedValue(new Error("no ledger"));
    await expect(enforceWallBudgets(MAIN, PAST)).resolves.toEqual([]);
    warn.mockRestore();
  });
});

/**
 * The mechanism shipped enforced and *unset*: `ProcessKind::default_limits`
 * returned `None` for all four kinds and `process_admit` built its limit set
 * from the arguments alone, so every WebView row was declared unbounded and the
 * sweep above had nothing to fire on. These assert the two halves that changed —
 * that a row now arrives carrying a budget, and that the budget it arrives with
 * is the one the user's setting names.
 */
describe("the budget a WebView process is admitted with", () => {
  beforeEach(() => {
    useSettingsStore.setState({
      processWallBudgetEnabled: true,
      processWallBudgetHours: DEFAULT_PROCESS_WALL_BUDGET_HOURS,
    });
  });

  function admittedArgs(): Record<string, unknown> {
    const call = invokeMock.mock.calls.find(([command]) => command === "process_admit");
    expect(call, "process_admit was never invoked").toBeDefined();
    return (call![1] as { args: Record<string, unknown> }).args;
  }

  it("carries the default budget for every kind the sweep enforces", async () => {
    for (const kind of WALL_BUDGET_KINDS) {
      invokeMock.mockReset();
      invokeMock.mockResolvedValue({ processId: "p-1" });
      await admitProcess({ kind, externalId: `ext-${kind}` });
      expect(admittedArgs().maxWallMs).toBe(
        DEFAULT_PROCESS_WALL_BUDGET_HOURS * 60 * 60 * 1000,
      );
      expect(admittedArgs().unboundedWall).toBeUndefined();
    }
  });

  it("leaves a kind bounded by somebody else alone", async () => {
    invokeMock.mockReset();
    invokeMock.mockResolvedValue({ processId: "p-1" });
    // A background shell is spawned with no timeout on purpose so it can
    // outlive its turn; a global slider must not quietly bound it.
    await admitProcess({ kind: "background_shell", externalId: "sh-1" });
    expect(admittedArgs().maxWallMs).toBeUndefined();
    expect(admittedArgs().unboundedWall).toBeUndefined();
  });

  it("asks for no budget at all when the setting is off, rather than one nothing enforces", async () => {
    useSettingsStore.setState({ processWallBudgetEnabled: false });
    invokeMock.mockReset();
    invokeMock.mockResolvedValue({ processId: "p-1" });
    await admitProcess({ kind: "chat_turn", externalId: "ext-off" });
    expect(admittedArgs().unboundedWall).toBe(true);
    expect(admittedArgs().maxWallMs).toBeUndefined();
  });

  it("uses the configured hours rather than the default once one is set", async () => {
    useSettingsStore.setState({ processWallBudgetHours: 2 });
    invokeMock.mockReset();
    invokeMock.mockResolvedValue({ processId: "p-1" });
    await admitProcess({ kind: "subagent", externalId: "ext-2h" });
    expect(admittedArgs().maxWallMs).toBe(2 * 60 * 60 * 1000);
  });

  it("never overrides a budget the caller stated for itself", async () => {
    invokeMock.mockReset();
    invokeMock.mockResolvedValue({ processId: "p-1" });
    await admitProcess({ kind: "chat_turn", externalId: "ext-explicit", maxWallMs: 5_000 });
    expect(admittedArgs().maxWallMs).toBe(5_000);
  });

  /**
   * End to end over the two halves: a row admitted with the shipped default is
   * inside its budget for hours and then latches a stop — which is the claim
   * "the budget actually fires", rather than "a hand-written row would fire".
   */
  it("fires on a row admitted with the shipped default, and not before", async () => {
    const budgetMs = DEFAULT_PROCESS_WALL_BUDGET_HOURS * 60 * 60 * 1000;
    const admitted = record({ limits: { maxWallMs: budgetMs } });

    expect(wallBudgetVerdict(admitted, STARTED + budgetMs - 1)).toBe("within-budget");
    expect(wallBudgetVerdict(admitted, STARTED + budgetMs)).toBe("exceeded");

    invokeMock.mockReset();
    invokeMock.mockImplementation((command: string) =>
      command === "process_list" ? Promise.resolve([admitted]) : Promise.resolve(true),
    );
    await expect(enforceWallBudgets(MAIN, STARTED + budgetMs)).resolves.toEqual(["exceeded"]);

    const signal = invokeMock.mock.calls.find(([command]) => command === "process_signal");
    expect(signal).toBeDefined();
    const payload = signal![1] as { signal: string; reason: string };
    expect(payload.signal).toBe("stop");
    expect(payload.reason.startsWith(WALL_BUDGET_STOP_REASON_PREFIX)).toBe(true);
    expect(isWallBudgetKill({ ...admitted, signalReason: payload.reason })).toBe(true);
  });
});
