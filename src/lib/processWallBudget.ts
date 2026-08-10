/**
 * Wall-clock budget enforcement for the four process kinds this WebView hosts.
 *
 * `chat_turn`, `subagent`, `crew_member` and `side_task` had no wall bound of any
 * kind: each runs an unbounded number of *bounded* tool calls, so the tools are
 * capped (`SHELL_TIMEOUT`, `DEFAULT_VERIFY_TIMEOUT_SECS`) and the process issuing
 * them is not. `workflow_run` already has its enforced 24h budget in the Rust
 * executor and `workflow_node` its validated per-node timeout; `daemon_job` has
 * its watchdog. This closes the remaining hole, and closes it with the parts that
 * already exist rather than a second mechanism:
 *
 * - `max_wall_ms` / `started_at_ms` are already columns on the row and already
 *   reach the frontend (`processTable.ts`).
 * - The 2-second catch-up sweep already runs (`PENDING_SIGNAL_SWEEP_INTERVAL_MS`),
 *   so this rides it instead of adding a timer — see
 *   {@link enforceWallBudgets} for why the read is cheap enough to ride along.
 * - The stop latch is already durable (`process_signal` → `stopRequested`) and
 *   already fanned out to each kind's own primitive by
 *   `processSignalDelivery.ts`. A budget kill is therefore *exactly* a stop that
 *   nobody had to press, and it reaches a turn running in another window for
 *   free, because that fan-out already crosses windows.
 *
 * # It ships enforced and UNSET, deliberately
 *
 * Nothing here picks a number, and neither does any admit call site: no kind in
 * this allow-list passes `maxWallMs`, and `ProcessKind::default_limits` returns
 * `None` for all four. So the mechanism is live and fires for nobody until a
 * budget is configured, which is the only honest place to ship it from.
 *
 * The reason is not timidity, it is that `ProcessState` has no state for "parked
 * waiting on a human". A turn blocked on an unanswered permission dialog reads
 * as `Running` and its `started_at_ms` keeps ageing, so any default would kill a
 * turn for the user's own slowness — the failure mode is "the app cancelled my
 * work while I was reading the prompt", which is worse than an unbounded turn.
 * Choosing the number is a judgement about what a turn is *for* and belongs to
 * settings, the same reasoning `ProcessKind::default_limits` already records for
 * why it invents no per-kind wall bound.
 *
 * # A budget is a floor, not a ceiling
 *
 * The latch is only *observed* at a safe point. A turn 10 seconds into a
 * 120-second `run_shell` cannot see it until that tool returns, so the real bound
 * is `max_wall_ms` + the longest tool timeout in flight (120s for a shell, 300s
 * for a verify). This bounds how long a runaway keeps *starting new work*; it is
 * not a hard kill and must not be documented as one. A hard kill would need an OS
 * process to signal, which is precisely what these kinds do not have — see
 * `signal_support`'s refusal of `Kill` for them.
 *
 * # Suspended time counts against the budget
 *
 * `started_at_ms` is stamped on the *first* entry into `running` and deliberately
 * survives a resume (`ProcessTable::transition`), and there is no
 * accumulated-suspended-ms column. So an hour parked is an hour of budget, and a
 * long-suspended row can trip the moment it resumes. That is a known limit, not a
 * design: fixing it means a new column, which is a schema change and a different
 * piece of work. Note the asymmetry with `SHELL_TIMEOUT`, which *does* discount
 * suspended time — a tool has one live wait to subtract from, a process row has
 * one stamp and no history.
 */
import { invoke } from "@tauri-apps/api/core";

import { listProcesses, WALL_BUDGET_KINDS, type ProcessRecord } from "./processTable";

export { WALL_BUDGET_KINDS } from "./processTable";

/**
 * Why a row was or was not killed for its budget.
 *
 * Every non-fire is named rather than collapsed into `false`, the same way
 * `ProcessSignalDelivery` names every non-delivery: on a mechanism that ships
 * inert, "did not fire" is the answer nearly every time, and the six reasons it
 * did not are the only way to tell inert-because-unconfigured from
 * inert-because-broken.
 */
export type WallBudgetVerdict =
  /** Past budget, and a stop has just been latched for it. */
  | "exceeded"
  /** Inside its budget. The overwhelmingly common outcome once one is set. */
  | "within-budget"
  /** No `maxWallMs` on the row — the shipped default for every kind here. */
  | "unset"
  /** Not a kind this applies to. See {@link WALL_BUDGET_KINDS}. */
  | "not-applicable"
  /** Suspended: not consuming anything, so it must not trip. */
  | "parked"
  /** A stop is already latched; re-latching would be noise, not urgency. */
  | "already-stopping"
  /** Admitted but never `running`, so no elapsed time exists to measure. */
  | "not-started"
  | "exited";

/**
 * Prefix of the `signalReason` written by a budget kill, and the marker that
 * lets one be told apart from a human pressing Stop after the fact.
 *
 * It has to survive a round trip through SQLite: the row is latched here and the
 * exit is recorded later, by another function and possibly in another window,
 * with nothing of this decision left in memory. `signal_reason` is the column
 * that carries it — the same trick the daemon plays with `last_error` for exactly
 * the same reason, and the same reason `ProcessExit::reason` is documented to
 * name the limit that fired.
 */
export const WALL_BUDGET_STOP_REASON_PREFIX = "wall budget exceeded: max_wall_ms";

/** How long this row has been running, or `null` if it never started. */
function elapsedMs(record: ProcessRecord, nowMs: number): number | null {
  return record.startedAtMs === null ? null : nowMs - record.startedAtMs;
}

/**
 * Whether `record` has outlived its declared wall budget, and if not, why not.
 *
 * Pure and clock-injected so the whole decision table can be asserted without
 * fake timers: the sweep passes `Date.now()`, tests pass the instant they care
 * about.
 *
 * The order of the checks is the point of the function. Kind is first so that
 * exclusion from {@link WALL_BUDGET_KINDS} cannot be reached around by any later
 * branch. Then the three "this row is not eligible" facts — exited, already
 * stopping, parked — before anything is measured, because for each of them the
 * elapsed time is a true number with no action attached to it.
 */
export function wallBudgetVerdict(record: ProcessRecord, nowMs: number): WallBudgetVerdict {
  if (!WALL_BUDGET_KINDS.includes(record.kind)) return "not-applicable";
  if (record.state === "exited") return "exited";
  // Idempotence. A latched stop keeps reading as pending until the row exits (by
  // design — an ignored stop should keep being pressed), and delivery may be
  // minutes away behind a tool timeout, so this row will be seen again on every
  // sweep in between. Latching again would rewrite `signalReason` and emit a
  // `processes://changed` per tick for a decision already made.
  if (record.signalIntent.stopRequested) return "already-stopping";
  const budgetMs = record.limits.maxWallMs;
  // `<= 0` is treated as unset rather than as "kill immediately". The ledger's
  // own CHECK already forbids a non-positive `max_wall_ms`, so a zero here means
  // a caller invented one, and reading it as a budget would make every process
  // die at admission — the loudest possible way to be wrong about a number this
  // module deliberately does not choose.
  if (budgetMs === null || budgetMs === undefined || !(budgetMs > 0)) return "unset";
  // A suspended row is consuming nothing, so its budget must not run. It also
  // could not act on the latch: a parked cooperative loop never reaches its own
  // cancellation branch, which is why stop-wins-over-suspend exists at all.
  //
  // A `pause_pending` row — still `running`, suspend latched, waiting for its
  // safe point — is NOT parked and does trip. It is running real work, and its
  // stop is honoured ahead of the pending suspend rather than parking it.
  if (record.state === "suspended") return "parked";
  const elapsed = elapsedMs(record, nowMs);
  // Admitted but never running. Not measurable, and not this mechanism's problem
  // either: a row stuck at `admitted` is a projection leak, and stopping it would
  // latch an intent nothing can deliver, because the loop has not registered its
  // cancellation entry yet. `reapMissingProcesses` is what clears those.
  if (elapsed === null) return "not-started";
  // `>=` and not `>`: a budget of 0 is refused above, so the two differ only by
  // one millisecond at the boundary, and "at the budget" is more naturally over
  // than under. A negative elapsed (the clock went backwards between the stamp
  // and now) falls through to `within-budget`, which is the right answer: a
  // clock correction must not kill anybody's turn.
  return elapsed >= budgetMs ? "exceeded" : "within-budget";
}

/**
 * The `signalReason` a budget kill records, naming the limit and what it caught.
 *
 * The elapsed value is included because the budget alone does not tell an
 * operator whether the kill was tight or wildly overdue — and given the floor
 * behaviour above, overdue by a tool timeout is the expected shape rather than a
 * bug.
 */
export function wallBudgetStopReason(record: ProcessRecord, nowMs: number): string {
  const elapsed = elapsedMs(record, nowMs) ?? 0;
  return `${WALL_BUDGET_STOP_REASON_PREFIX}=${record.limits.maxWallMs}ms, ran ${elapsed}ms`;
}

/** Whether this row's pending stop came from a budget rather than from a human. */
export function isWallBudgetKill(record: ProcessRecord): boolean {
  return record.signalReason?.startsWith(WALL_BUDGET_STOP_REASON_PREFIX) === true;
}

/*
 * There is deliberately no `wallBudgetExitStatus` here.
 *
 * An earlier draft classified the exit on this side, which would have been a
 * second mechanism: Rust's `ProcessTable::transition` already reads the row's
 * `signal_reason` on its way to writing the exit, so it can do the upgrade for
 * every host — the four WebView loops, the daemon, and `monkey processes` alike —
 * where a TypeScript classifier would have covered only the loops, and only after
 * four separate adoptions each needing a record read in a `finally` that holds
 * nothing but a process id.
 *
 * So the contract is one-way and narrow: this module writes
 * {@link WALL_BUDGET_STOP_REASON_PREFIX} into the stop reason, and
 * `process_table.rs`'s `WALL_BUDGET_REASON_PREFIX` reads it. That constant is the
 * authority; the copy here mirrors it, and a test on each side pins the literal.
 */

/**
 * Reads the live rows for {@link WALL_BUDGET_KINDS} and latches a stop on any
 * that has outlived its budget.
 *
 * Runs from `sweepPendingProcessSignals`, on the sweep's existing 2-second
 * cadence, and adds one indexed `liveOnly` read to it. That is affordable for the
 * same reason the sweep itself is: the live set is single digits, and the
 * alternative — a per-process timer armed at admission — would have to be
 * cancelled on every exit path in four modules and re-armed after a restart,
 * where this needs neither, because the row's own `started_at_ms` is the state.
 *
 * Main-window-only (`ownsGlobalKinds`), even though all four kinds are delivered
 * window-locally. The *decision* is a durable write against timestamps every
 * window can read, so two windows would latch the same row twice; the delivery
 * that follows is still done by whichever window holds the controller, through
 * the fan-out that already crosses windows. One decider, any deliverer.
 *
 * Fail-soft as a whole: the read is (`listProcesses` warns and yields nothing),
 * and each latch is caught individually so one refused `process_signal` cannot
 * cost the rest of the sweep — including the pending-signal delivery that runs
 * after it.
 */
export async function enforceWallBudgets(
  options: { ownsGlobalKinds: boolean },
  nowMs: number = Date.now(),
): Promise<WallBudgetVerdict[]> {
  if (!options.ownsGlobalKinds) return [];
  const live = await listProcesses({ kinds: [...WALL_BUDGET_KINDS], liveOnly: true });
  const verdicts: WallBudgetVerdict[] = [];
  for (const record of live) {
    const verdict = wallBudgetVerdict(record, nowMs);
    verdicts.push(verdict);
    if (verdict !== "exceeded") continue;
    try {
      // The same durable latch the Processes panel's Stop button writes, so
      // everything downstream — cross-window delivery, the CLI's view of the
      // row, surviving a restart — is inherited rather than rebuilt. `stop`
      // rather than `kill` because `kill` is refused for every kind here: none
      // of them owns an OS process to terminate.
      await invoke("process_signal", {
        processId: record.processId,
        signal: "stop",
        reason: wallBudgetStopReason(record, nowMs),
      });
    } catch (error) {
      // Worth a warning, unlike the sweep's delivery misses: the latch is the
      // one step with no retry cheaper than the next sweep, and a budget that
      // silently fails to latch looks exactly like a budget nobody set.
      console.warn(
        `[processWallBudget] could not latch a stop for ${record.kind} ${record.externalId}:`,
        error,
      );
    }
  }
  return verdicts;
}
