/**
 * Frontend client for the unified agent process table (`process_table.rs`).
 *
 * Every call here is **fail-soft**: the process table is an observability and
 * arbitration surface, and a turn must never fail because its projection could
 * not be written. A missing row is a worse listing; a turn that refuses to run
 * to protect a listing is a worse product. Failures warn and return a falsy
 * value, exactly as the daemon's own projection swallows and logs.
 *
 * This is also why every function tolerates a non-Tauri host: the dev/browser
 * profile has no backend, and the loop it wraps still has to work there.
 */
import { invoke, isTauri } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";

/** Mirrors `process_commands.rs`'s `PROCESSES_CHANGED_EVENT`. */
export const PROCESSES_CHANGED_EVENT = "processes://changed";

export type ProcessKind =
  | "chat_turn"
  | "daemon_job"
  | "subagent"
  | "crew_member"
  | "workflow_run"
  | "workflow_node"
  | "remote_run"
  | "background_shell"
  | "side_task";

export type ProcessState = "admitted" | "running" | "suspended" | "exited";

export type ProcessExitStatus =
  | "succeeded"
  | "failed"
  | "cancelled"
  | "limit_exceeded"
  | "lost"
  | "needs_reconciliation";

export interface ProcessLimits {
  maxWallMs?: number | null;
  maxMemoryBytes?: number | null;
  maxOutputBytes?: number | null;
  maxChildProcesses?: number | null;
}

export interface ProcessExit {
  status: ProcessExitStatus;
  code?: number | null;
  signal?: string | null;
  reason?: string | null;
}

/**
 * Durable signal intent, as two independent latches.
 *
 * Independent on purpose: asking a suspended process to stop must not erase that
 * it was suspended, and `resume` clears only `suspendRequested` — never a
 * pending stop, which would turn "stop this" into "keep going" on a race.
 */
export interface SignalIntent {
  stopRequested: boolean;
  suspendRequested: boolean;
  /**
   * Whether the stop must be delivered as an immediate termination rather than
   * a cooperative wind-down. Never set without `stopRequested` — a kill IS a
   * stop with a stronger promise about how — so any check that only asks "is
   * this winding down?" stays correct without reading this at all.
   */
  killRequested: boolean;
}

export interface ProcessRecord {
  processId: string;
  parentProcessId: string | null;
  kind: ProcessKind;
  externalId: string;
  state: ProcessState;
  runId: string | null;
  workspace: string | null;
  profile: string | null;
  nativePid: number | null;
  limits: ProcessLimits;
  /** What has been asked of this process. Delivery is `processSignalDelivery.ts`. */
  signalIntent: SignalIntent;
  signalReason: string | null;
  signalRequestedAtMs: number | null;
  exit: ProcessExit | null;
  createdAtMs: number;
  updatedAtMs: number;
  startedAtMs: number | null;
  exitedAtMs: number | null;
}

export interface AdmitProcessArgs {
  kind: ProcessKind;
  /** The surface's own id — a turn id, a subagent cancel id, a workflow run id. */
  externalId: string;
  parentProcessId?: string | null;
  /** The parent's surface id, when that is all the caller has. */
  parentExternalId?: string | null;
  parentKind?: ProcessKind | null;
  runId?: string | null;
  workspace?: string | null;
  profile?: string | null;
  maxWallMs?: number | null;
  maxMemoryBytes?: number | null;
  maxOutputBytes?: number | null;
  maxChildProcesses?: number | null;
}

function warn(operation: string, error: unknown): void {
  console.warn(
    `[processTable] ${operation} failed; the process listing will be incomplete:`,
    error,
  );
}

/**
 * Admit a process and return its id, or `null` if it could not be recorded.
 *
 * A `null` return is not an error the caller should propagate — it means "carry
 * on without a projection".
 */
export async function admitProcess(args: AdmitProcessArgs): Promise<string | null> {
  if (!isTauri()) return null;
  try {
    const record = await invoke<ProcessRecord>("process_admit", { args });
    return record.processId;
  } catch (error) {
    warn(`admit ${args.kind} ${args.externalId}`, error);
    return null;
  }
}

/**
 * Point an already-admitted process at its ledger run.
 *
 * Separate from {@link AdmitProcessArgs.runId} because `agent_processes.run_id`
 * is a foreign key into `runs`: a surface whose process row is minted *before*
 * its run row exists cannot carry the link at admission time. The link is what
 * lets the per-process resource ledger attribute a run's measured usage — CPU
 * time, peak RSS, disk I/O, egress bytes — to this row instead of an
 * unattributed bucket.
 *
 * Call it at most **once** per process. The ledger charges a run only when
 * exactly one row claims it, and re-pointing a row at a second run does not
 * merely move the charge: it leaves the first run claimed by nobody, which the
 * ledger treats exactly like several rows claiming it — unattributed.
 */
export async function linkProcessRun(processId: string, runId: string): Promise<void> {
  if (!isTauri()) return;
  try {
    await invoke("process_link_run", { processId, runId });
  } catch (error) {
    warn(`link ${processId} to run ${runId}`, error);
  }
}

async function transition(
  processId: string,
  state: ProcessState,
  exit?: ProcessExit,
): Promise<void> {
  if (!isTauri()) return;
  try {
    await invoke("process_transition", {
      args: {
        processId,
        state,
        exitStatus: exit?.status ?? null,
        exitCode: exit?.code ?? null,
        exitSignal: exit?.signal ?? null,
        exitReason: exit?.reason ?? null,
      },
    });
  } catch (error) {
    warn(`transition ${processId} -> ${state}`, error);
  }
}

export function markProcessRunning(processId: string): Promise<void> {
  return transition(processId, "running");
}

export function markProcessSuspended(processId: string): Promise<void> {
  return transition(processId, "suspended");
}

/** Exit a process. `reason` is required for anything but a plain success. */
export function exitProcess(
  processId: string,
  status: ProcessExitStatus,
  reason?: string | null,
): Promise<void> {
  return transition(processId, "exited", { status, reason: reason ?? null });
}

/**
 * Classifies how a turn or task ended into the one exit vocabulary.
 *
 * `aborted` wins over a thrown error on purpose: aborting a turn usually
 * surfaces as an exception, and recording a user's Stop as a failure would make
 * the listing lie about what happened.
 */
export function exitStatusFor(options: {
  aborted: boolean;
  error?: unknown;
}): { status: ProcessExitStatus; reason: string | null } {
  if (options.aborted) return { status: "cancelled", reason: "stopped by the user" };
  if (options.error !== undefined && options.error !== null) {
    return {
      status: "failed",
      reason: options.error instanceof Error ? options.error.message : String(options.error),
    };
  }
  return { status: "succeeded", reason: null };
}

export interface ReconcileProcessArgs {
  kind: ProcessKind;
  externalId: string;
  state: ProcessState;
  parentKind?: ProcessKind | null;
  parentExternalId?: string | null;
  runId?: string | null;
  workspace?: string | null;
  profile?: string | null;
  exitStatus?: ProcessExitStatus | null;
  exitReason?: string | null;
}

/**
 * Idempotent projection. Unlike {@link admitProcess}, reconciling twice is a
 * no-op rather than an error, so a caller that may not be first can still
 * establish a record.
 *
 * Used where the frontend knows something the eventual owner does not — a turn
 * routed to the resident runner creates the daemon job's record with the turn as
 * its parent, and the daemon's own reconcile then finds that record and only
 * moves its state. That is how the parent edge survives crossing the process
 * boundary.
 */
export async function reconcileProcess(args: ReconcileProcessArgs): Promise<string | null> {
  if (!isTauri()) return null;
  try {
    const record = await invoke<ProcessRecord>("process_reconcile", { args });
    return record.processId;
  } catch (error) {
    warn(`reconcile ${args.kind} ${args.externalId}`, error);
    return null;
  }
}

export interface ProcessListFilter {
  kinds?: ProcessKind[];
  liveOnly?: boolean;
  parentProcessId?: string;
  workspace?: string;
  limit?: number;
}

export async function listProcesses(filter?: ProcessListFilter): Promise<ProcessRecord[]> {
  if (!isTauri()) return [];
  try {
    return await invoke<ProcessRecord[]>("process_list", { args: filter ?? {} });
  } catch (error) {
    warn("list", error);
    return [];
  }
}

export async function processDescendants(processId: string): Promise<ProcessRecord[]> {
  if (!isTauri()) return [];
  try {
    return await invoke<ProcessRecord[]>("process_descendants", { processId });
  } catch (error) {
    warn(`descendants ${processId}`, error);
    return [];
  }
}

export interface ProcessLiveCount {
  kind: ProcessKind;
  count: number;
}

export async function processLiveCounts(): Promise<ProcessLiveCount[]> {
  if (!isTauri()) return [];
  try {
    return await invoke<ProcessLiveCount[]>("process_live_counts");
  } catch (error) {
    warn("live counts", error);
    return [];
  }
}

/**
 * Every process of `kinds` whose signal intent has not been delivered yet.
 *
 * The catch-up read behind {@link onProcessesChanged}: that event only reaches
 * windows of this app, so a `monkey processes signal` from a terminal — another
 * OS process, no Tauri bus — is invisible to a listener and lands here instead.
 */
export async function pendingProcessSignals(kinds?: ProcessKind[]): Promise<ProcessRecord[]> {
  if (!isTauri()) return [];
  try {
    return await invoke<ProcessRecord[]>("process_pending_signals", {
      kinds: kinds ?? null,
    });
  } catch (error) {
    warn("pending signals", error);
    return [];
  }
}

/**
 * Fires for every process-table change, in every window — the same fan-out
 * convention as `runs://changed`.
 *
 * Resolves to a no-op unlisten outside Tauri so a caller's cleanup path does not
 * have to special-case the dev/browser profile.
 */
export async function onProcessesChanged(
  handler: (record: ProcessRecord) => void,
): Promise<UnlistenFn> {
  if (!isTauri()) return () => {};
  try {
    return await listen<ProcessRecord>(PROCESSES_CHANGED_EVENT, (event) => handler(event.payload));
  } catch (error) {
    warn("subscribe to changes", error);
    return () => {};
  }
}

/**
 * Reap processes this app instance can no longer account for.
 *
 * Called once at startup with the processes still known to be live — which,
 * after a restart, is none of the frontend's. A turn whose WebView died
 * mid-run previously stayed `running` in the ledger forever because nothing
 * swept it.
 */
export async function reapMissingProcesses(
  liveProcessIds: string[],
  reason?: string,
): Promise<ProcessRecord[]> {
  if (!isTauri()) return [];
  try {
    return await invoke<ProcessRecord[]>("process_reap_missing", {
      liveProcessIds,
      reason: reason ?? null,
    });
  } catch (error) {
    warn("reap", error);
    return [];
  }
}
