/**
 * Delivery half of the signal contract, for the kinds the desktop owns.
 *
 * `process_signal` records durable intent (`stopRequested` / `suspendRequested`)
 * and stops there, deliberately: that is what makes a signal survive a restart
 * and reach a process this app is not running. Something then has to read the
 * latch and turn it into the real primitive. The daemon does that once per tick
 * for its own jobs; this module does it for everything else.
 *
 * **It is a fan-out table, not a second mechanism.** Every kind here already had
 * a working cancellation path — `runCancellationRegistry` for chat turns and crew
 * members, `cancelSubagentRun`, `cancelSideTask`, `background_shell_kill`,
 * `m4_workflows_cancel`. None of them is replaced. All this does is map a latched
 * intent onto the one that belongs to that kind, which is the same shape
 * `run_request_cancellation` → `cancelRegisteredRun` already proved.
 *
 * Two triggers, because one is not enough:
 *
 * - `processes://changed` is the fast path, and covers a signal raised anywhere
 *   inside this app — Run Center, another window, a future process table UI.
 * - {@link sweepPendingProcessSignals} is the catch-up read, and covers
 *   `monkey processes signal` from a terminal. The CLI writes to SQLite from a
 *   different OS process and cannot emit a Tauri event, so no listener will ever
 *   hear it. Without the sweep, half of "signals work across a process boundary"
 *   would only be true in the daemon's direction.
 *
 * Neither trigger polls per round. The sweep is one indexed query
 * (`agent_processes_pending_signal_idx`) over the deliverable kinds, and the
 * listener does no IPC at all for the overwhelmingly common case of a record
 * with no intent set.
 */
import { invoke, isTauri } from "@tauri-apps/api/core";

import { cancelRegisteredRun } from "./runCancellationRegistry";
import { cancelSubagentRun } from "./subagent";
import { cancelSideTask, pauseSideTask, resumeSideTask } from "./sideTaskRunner";
import { type ProcessKind, type ProcessRecord, pendingProcessSignals } from "./processTable";

/**
 * What happened to one record's intent. Every non-delivery is named rather than
 * collapsed into "false", because the three reasons are diagnostically different
 * — and one of them (`no-live-target` for a workflow run) is a known gap this
 * module is the natural place to observe.
 */
export type ProcessSignalDelivery =
  | "stopped"
  | "suspended"
  | "resumed"
  /** No latch set, or the latch is already acknowledged by the record's state. */
  | "nothing-pending"
  /** Latched, but not deliverable from this state yet. The next read retries. */
  | "deferred"
  /** Another process delivers this kind: the daemon for its jobs and remote runs. */
  | "delivered-elsewhere"
  /** Ours by kind, but not running here — another window, or already wound down. */
  | "no-live-target"
  /** This kind has no cancellation primitive of its own at any granularity. */
  | "no-primitive";

/**
 * Kinds whose primitive lives in *this* WebView, so every window must attempt
 * delivery: only the window holding the `AbortController` can act, and a miss in
 * the others is a map lookup with no IPC behind it.
 */
const WINDOW_LOCAL_KINDS: readonly ProcessKind[] = [
  "chat_turn",
  "subagent",
  "crew_member",
  "side_task",
];

/**
 * Kinds owned by the Rust side, reachable identically from any window — so
 * exactly one window should deliver, or two invocations race over the same
 * child. The main window is that one (see `App.tsx`).
 */
const PROCESS_GLOBAL_KINDS: readonly ProcessKind[] = ["background_shell", "workflow_run"];

/** Every kind the desktop can deliver to, and therefore the sweep's scope. */
export const DESKTOP_DELIVERABLE_KINDS: readonly ProcessKind[] = [
  ...WINDOW_LOCAL_KINDS,
  ...PROCESS_GLOBAL_KINDS,
];

/**
 * How often the catch-up sweep runs.
 *
 * A signal from another OS process cannot be pushed to this app, so the only
 * question is how stale it may be. Two seconds is the same order as the per-run
 * polling `watchDaemonDesktopTurn` already does, and buys one indexed query
 * against a table whose live set is single digits — while making "stop that turn"
 * from a terminal land in about the time it takes to read the confirmation.
 */
export const PENDING_SIGNAL_SWEEP_INTERVAL_MS = 2000;

export interface DeliveryOptions {
  /**
   * Whether this caller is the one window responsible for
   * {@link PROCESS_GLOBAL_KINDS}. `App.tsx` passes `getCurrentWindow().label ===
   * "main"`.
   */
  ownsGlobalKinds: boolean;
}

/**
 * Which signal, if any, this record is still waiting on.
 *
 * `"too-early"` is distinct from `null` on purpose: a suspend latched on a
 * process that has not reached `running` yet is genuinely pending, and reporting
 * it as nothing-pending would claim the latch was already dealt with.
 */
function pendingSignal(
  record: ProcessRecord,
): "stop" | "suspend" | "resume" | "too-early" | null {
  if (record.state === "exited") return null;
  // Stop is checked first and wins, for the same reason the daemon applies it
  // first: a suspended loop never reaches its own cancellation branch, so
  // honouring the suspend of a process that has also been asked to stop would
  // park it there instead of winding it down.
  if (record.signalIntent.stopRequested) return "stop";
  // State is the acknowledgement — a suspend already reflected in `suspended` is
  // delivered, and so is a cleared suspend on a record already back to running.
  if (record.signalIntent.suspendRequested) {
    if (record.state === "suspended") return null;
    // `admitted` has no legal transition to `suspended`, so a suspend that
    // arrives in the gap between admit and running waits for the next read
    // rather than failing a transition forever.
    return record.state === "running" ? "suspend" : "too-early";
  }
  return record.state === "suspended" ? "resume" : null;
}

async function deliverStop(record: ProcessRecord): Promise<ProcessSignalDelivery> {
  switch (record.kind) {
    // A chat turn registers its own turn id (`registerDurableController`) and a
    // crew member its actor run id, both in the shared registry that Run Center
    // already stops through.
    case "chat_turn":
    case "crew_member":
      return cancelRegisteredRun(record.externalId) ? "stopped" : "no-live-target";
    case "subagent":
      return cancelSubagentRun(record.externalId) ? "stopped" : "no-live-target";
    case "side_task":
      // Aborts the in-flight attempt and releases a paused one, so a stop
      // reaches a task parked in `waitUntilResumed` too.
      cancelSideTask(record.externalId);
      return "stopped";
    case "background_shell":
      // Rust owns the child; killing is the only stop it has (which is also why
      // `suspend` is refused for this kind).
      if (!isTauri()) return "no-live-target";
      try {
        await invoke("background_shell_kill", { id: record.externalId });
        return "stopped";
      } catch {
        // Already gone, or never ours. Not worth a warning: the sweep re-reads
        // until the row leaves the live set, and this is the ordinary shape of
        // the race it loses.
        return "no-live-target";
      }
    case "workflow_run":
      if (!isTauri()) return "no-live-target";
      try {
        // `false` is meaningful rather than a failure: the run is absent from
        // `WorkflowService`'s in-memory registry, which is exactly the
        // out-of-process hole K2 still lists as open. Naming it `no-live-target`
        // keeps that visible instead of reporting a stop that did not happen.
        const cancelled = await invoke<boolean>("m4_workflows_cancel", {
          runId: record.externalId,
        });
        return cancelled ? "stopped" : "no-live-target";
      } catch {
        return "no-live-target";
      }
    default:
      return "delivered-elsewhere";
  }
}

/**
 * Delivers whatever `record` is still waiting on, and reports which.
 *
 * Safe to call repeatedly with the same record, and it will be: a
 * `stopRequested` row keeps reading as pending until it exits, which is
 * deliberate — an ignored stop should keep being pressed. Every delivery path
 * here is idempotent (aborting an aborted controller, pausing a paused task).
 */
export async function deliverProcessSignal(
  record: ProcessRecord,
  options: DeliveryOptions,
): Promise<ProcessSignalDelivery> {
  const signal = pendingSignal(record);
  if (signal === null) return "nothing-pending";
  if (signal === "too-early") return "deferred";

  const windowLocal = WINDOW_LOCAL_KINDS.includes(record.kind);
  const processGlobal = PROCESS_GLOBAL_KINDS.includes(record.kind);
  if (!windowLocal && !processGlobal) {
    // A workflow *node* has no primitive of its own at any granularity —
    // cancelling one means cancelling its run, which is a different request than
    // the caller made. Everything else here belongs to the daemon.
    return record.kind === "workflow_node" ? "no-primitive" : "delivered-elsewhere";
  }
  if (processGlobal && !options.ownsGlobalKinds) return "delivered-elsewhere";

  if (signal === "stop") return deliverStop(record);

  // Suspend and resume exist for exactly one desktop kind. `signal_support`
  // refuses them for the rest, so `process_signal` rejects the request before it
  // ever reaches a latch — this branch is unreachable for other kinds, and says
  // so rather than guessing at a pause point that does not exist.
  if (record.kind !== "side_task") return "no-primitive";
  if (signal === "suspend") {
    pauseSideTask(record.externalId);
    return "suspended";
  }
  resumeSideTask(record.externalId);
  return "resumed";
}

/**
 * Reads undelivered intent for the deliverable kinds and delivers each.
 *
 * Called at startup and on a slow interval — see `App.tsx` for the cadence and
 * why the interval exists at all.
 */
export async function sweepPendingProcessSignals(
  options: DeliveryOptions,
): Promise<ProcessSignalDelivery[]> {
  const pending = await pendingProcessSignals([...DESKTOP_DELIVERABLE_KINDS]);
  const outcomes: ProcessSignalDelivery[] = [];
  for (const record of pending) {
    outcomes.push(await deliverProcessSignal(record, options));
  }
  return outcomes;
}
