/**
 * Signalling and display derivation for the Processes panel.
 *
 * Split out of `processTable.ts` on purpose, and the reason is the bundle
 * budget rather than taste. `processTable.ts` is imported by `agentLoop.ts`,
 * `subagent.ts`, `crewRunner.ts` and `sideTaskRunner.ts` — all eager — so
 * anything added there lands in the main chunk for every user whether or not
 * they ever open a panel. Everything here is used only by the lazily-loaded
 * Processes panel and its store, so it belongs in that chunk.
 */
import { invoke } from "@tauri-apps/api/core";

import type { ProcessRecord, ProcessState } from "./processTable";

export type ProcessSignal = "stop" | "suspend" | "resume" | "kill";

/**
 * Asks a process for a signal, recording durable intent that the owning kind
 * delivers at its own safe point.
 *
 * The one call here that is deliberately **not** fail-soft. Everything else in
 * this module is bookkeeping the user never asked for; this is a direct user
 * action, and a refusal carries the reason the kind gave (see
 * `ProcessKind::signal_support`) — swallowing it would leave a button that
 * looks like it worked and did nothing.
 */
export async function signalProcess(
  processId: string,
  signal: ProcessSignal,
  reason?: string,
): Promise<ProcessRecord> {
  return invoke<ProcessRecord>("process_signal", {
    processId,
    signal,
    reason: reason ?? null,
  });
}

/**
 * What a process record should be *displayed* as, which is not always its
 * stored `state`.
 *
 * `pause_pending` is the honest gap between asking and arriving: a suspend is
 * latched durably the moment it is requested, but a cooperative kind only
 * reaches its safe point at the end of the current round — which for a long
 * `run_shell` can be minutes away. Showing `running` would hide that a pause
 * was asked for; showing `suspended` would claim a park that has not happened.
 * Derived here rather than stored, so `ProcessState` keeps its four variants
 * and its SQL transition trigger stays the single authority on what is legal.
 */
export type ProcessDisplayState = ProcessState | "pause_pending" | "stopping";

export function processDisplayState(record: ProcessRecord): ProcessDisplayState {
  if (record.state === "exited") return "exited";
  // A pending stop outranks a pending pause: the two latches are independent,
  // and a process on its way out is the more urgent fact about it.
  if (record.signalIntent.stopRequested) return "stopping";
  if (record.state === "running" && record.signalIntent.suspendRequested) {
    return "pause_pending";
  }
  return record.state;
}

/** Whether this record is in a state a resume would apply to.
 *
 * Both the latch and the state count, and it has to be both. Reading only the
 * latch was right for the cooperative kinds — where being parked *is* the latch,
 * since the row stays `running` until the loop reaches its safe point — and
 * wrong for a kind the OS actually stopped. A background shell that a resume has
 * already un-latched but whose SIGCONT never landed sits at `suspended` with no
 * intent: latch-only said "nothing to resume" and the panel offered Pause on a
 * stopped process, leaving no way back short of `kill -CONT` from outside.
 */
export function canResume(record: ProcessRecord): boolean {
  if (record.state === "exited") return false;
  return record.state === "suspended" || record.signalIntent.suspendRequested;
}

/** Whether asking for a suspend would say anything new. Mirror of
 * [`canResume`]: a row that is already parked has nothing to add either. */
export function canSuspend(record: ProcessRecord): boolean {
  if (record.state === "exited" || record.state === "suspended") return false;
  return !record.signalIntent.suspendRequested;
}
