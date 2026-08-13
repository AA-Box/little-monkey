import { invoke } from "@tauri-apps/api/core";

/**
 * What a producer is told about whether to send more work (K8).
 *
 * Three states rather than a boolean because the useful middle case exists:
 * `slow` means the queue has room but is deep enough that a producer *with a
 * choice* should wait. Branch on {@link Backpressure.state} or on `reason`,
 * both of which are stable tokens. **Never branch on `detail`** — it is a
 * sentence written for a human and its wording is free to change.
 */
export type BackpressureState = "accepting" | "slow" | "closed";

export type BackpressureReason = "kill_switch" | "queue_full" | "memory_saturated" | "queue_deep";

export interface Backpressure {
  state: BackpressureState;
  /** Mirror of `state !== "closed"`, so the common check is one field. */
  accepting: boolean;
  reason: BackpressureReason | null;
  /** Prose for a human. Displayed verbatim, never parsed. */
  detail: string | null;
  /**
   * Advisory wait before retrying. Derived from the poll interval and the
   * backlog — **not** a prediction of when anything will finish, because
   * nothing in the daemon knows that.
   */
  retryAfterMs: number | null;
  queueDepth: number;
  queueCapacity: number;
  queued: number;
  /** Queued jobs admission is refusing for resources. `held === queued` means
   * the machine is full rather than the queue. */
  held: number;
}

export interface DaemonStatus {
  installed: boolean;
  serviceRunning: boolean;
  heartbeatFresh: boolean;
  pid: number | null;
  killSwitch: boolean;
  queued: number;
  active: number;
  waitingApproval: number;
  paused: number;
  managedRunIds: string[];
  platform: unknown;
  /**
   * Optional because an older daemon does not send it. Absent is treated as
   * `accepting` by {@link backpressureOf} — a missing signal must never block
   * the app.
   *
   * `monkey daemon status --json` emits this block in snake_case
   * (`retry_after_ms`); `daemon_desktop_status` re-serializes the envelope in
   * camelCase. Which casing the nested block arrives in therefore depends on the
   * serde attributes on the Rust mirror struct, so `backpressureOf` accepts
   * either rather than betting on one and rendering an empty card if it loses.
   */
  backpressure?: Backpressure | RawBackpressure | null;
}

/** The CLI's own casing, tolerated on the way in. See `DaemonStatus.backpressure`. */
export interface RawBackpressure {
  state: BackpressureState;
  accepting: boolean;
  reason: BackpressureReason | null;
  detail: string | null;
  retry_after_ms: number | null;
  queue_depth: number;
  queue_capacity: number;
  queued: number;
  held: number;
}

export interface DaemonInstallRequest {
  concurrency: number;
  maxQueue: number;
  retentionDays: number;
  webhookPort: number | null;
  notifications: boolean;
}

export interface DaemonQueueRequest {
  recipe: string;
  runKey: string | null;
  priority: number;
  maxAttempts: number;
  maxRuntimeSeconds: number;
  maxMemoryMb: number | null;
  ownedWorktree: boolean;
  repository: string | null;
  branchPrefix: string;
  allowedRemotes: string[];
  allowCommit: boolean;
  allowPush: boolean;
  allowCreatePullRequest: boolean;
  allowReviewComment: boolean;
}

export interface DaemonTurnSubmitRequest {
  turnId: string;
  recipe: unknown;
  /** Which of the operator's own surfaces this turn came from. Both take the
   * same durable ingress path; the difference is what the ingress listing
   * shows. Omitted means the chat composer. */
  source?: "desktop" | "voice";
}

export interface DaemonTurnSubmitResponse {
  job_id: string;
  run_id: string;
  state: string;
}

export interface RemoteHostConfigureRequest {
  listen: string;
  advertiseUrl: string;
  tlsCertificate: string;
  tlsPrivateKey: string;
}

export interface RemotePairRequest {
  output: string;
  expiresMinutes: number;
  actions: string[];
  runIds: string[];
  workspaceIds: string[];
  maxArtifactBytes: number;
  /** First-party mobile-companion grants. Additive to `actions` and never
   * widening the run scope; omitted/empty means a runner-only controller. */
  mobileCapabilities?: string[];
  /** Grants over the device's own hardware. Omitted/empty means this runner
   * can ask the device for nothing physical, whatever it advertises. */
  deviceCapabilities?: string[];
}

export const MAX_REMOTE_ARTIFACT_BYTES = 32 * 1024 * 1024;
export const MAX_REMOTE_RUN_SCOPES = 1_024;
export const MAX_REMOTE_WORKSPACE_SCOPES = 128;

const REMOTE_ACTIONS = new Set([
  "view-runs",
  "view-events",
  "read-artifacts",
  "approve",
  "cancel",
  "kill",
  "control-desktop",
]);

/** Mobile-only grants — see `protocol::DeviceCapability` on the node. */
const MOBILE_CAPABILITIES = new Set([
  "view-sessions",
  "chat",
  "view-tasks",
  "run-workflows",
  "capture",
]);

export const daemonStatus = () => invoke<DaemonStatus>("daemon_desktop_status");
export const daemonInstall = (request: DaemonInstallRequest) => invoke<string>("daemon_desktop_install", { request });
export const daemonStart = () => invoke<string>("daemon_desktop_start");
export const daemonStop = () => invoke<string>("daemon_desktop_stop");
export const daemonUninstall = (purgeState = false) => invoke<string>("daemon_desktop_uninstall", { purgeState });
export const daemonQueue = (request: DaemonQueueRequest) => invoke<Record<string, unknown>>("daemon_desktop_queue", { request });
export const daemonPause = (runId: string) => invoke<string>("daemon_desktop_pause", { runId });
export const daemonResume = (runId: string) => invoke<string>("daemon_desktop_resume", { runId });
export const daemonCancel = (runId: string, reason?: string) => invoke<string>("daemon_desktop_cancel", { runId, reason: reason ?? null });
export const daemonRetry = (runId: string, acknowledgeSideEffects = false) => invoke<string>("daemon_desktop_retry", { runId, acknowledgeSideEffects });
export const daemonKillSwitch = (engaged: boolean) => invoke<string>("daemon_desktop_kill_switch", { engaged });
export const daemonTriggers = () => invoke<unknown>("daemon_desktop_triggers");
export const daemonDesktopTurnSubmit = (request: DaemonTurnSubmitRequest) =>
  invoke<DaemonTurnSubmitResponse>("m6a_desktop_turn_submit", { request });

export const remoteHostStatus = () => invoke<Record<string, unknown> | null>("remote_host_status");
export const remoteHostConfigure = (request: RemoteHostConfigureRequest) => invoke<string>("remote_host_configure", { request });
export const remoteHostDisable = () => invoke<string>("remote_host_disable");
export const remotePairCreate = (request: RemotePairRequest) => invoke<string>("remote_pair_create", { request });
export const remotePairList = () => invoke<string>("remote_pair_list");
export const remotePairRevoke = (deviceId: string, reason: string) => invoke<string>("remote_pair_revoke", { deviceId, reason });
export const remotePairRotate = (deviceId: string, output: string) => invoke<string>("remote_pair_rotate", { deviceId, output });
export const remoteAudit = (limit = 100) => invoke<unknown>("remote_audit", { limit });

/** Physical capabilities an operator may grant a paired device over its own
 * hardware — see `protocol::PHYSICAL_DEVICE_CAPABILITIES` on the node. Ordered
 * weakest-first, matching the CLI's own picker order. */
export const DEVICE_CAPABILITIES = [
  "device_info",
  "notification_post",
  "location_read",
  "audio_playback",
  "camera_capture",
  "microphone_capture",
  "screen_capture",
  "voice_stream",
] as const;

/** One command queued for a device, as `device-list`/`device-commands` report it. */
export interface RemoteDeviceCommandRow {
  command_id: string;
  device_id: string;
  capability: string;
  /** `queued` | `leased` | `running` | `succeeded` | `failed` | `cancelled` | `expired`. */
  state: string;
  attempt: number;
  cancel_requested: boolean;
  created_at_ms: number;
  updated_at_ms: number;
  expires_at_ms: number;
  source_run_id: string | null;
  artifact: { sha256: string; bytes: number; media_type: string } | null;
  error: string | null;
}

/** One paired physical device.
 *
 * The four capability fields are deliberately separate rather than merged:
 * "why can this phone not take a photo" has four different answers — not
 * granted, not supported by that build, not permitted by its OS, or all three
 * fine — and an operator has to be able to see which one applies. */
export interface RemoteDeviceRow {
  device_id: string;
  device_name: string;
  revoked: boolean;
  secret_generation: number;
  granted: string[];
  /** `null` until the device has reported its surface at least once. */
  advertised: string[] | null;
  os_permissions: Record<string, string> | null;
  effective: string[];
  platform: string | null;
  platform_version: string | null;
  app_version: string | null;
  device_model: string | null;
  last_seen_at_ms: number | null;
  /** The daemon's clock when it answered, so "last seen" is measured against
   * the machine that recorded it rather than this one. */
  now_ms: number;
  recent_commands: RemoteDeviceCommandRow[];
}

export const remoteDeviceList = () => invoke<{ devices: RemoteDeviceRow[] }>("remote_device_list");
export const remoteDeviceGrant = (deviceId: string, capabilities: string[]) =>
  invoke<string>("remote_device_grant", { deviceId, capabilities });
export const remoteDeviceCommands = (deviceId: string, limit = 20) =>
  invoke<{ commands: RemoteDeviceCommandRow[] }>("remote_device_commands", { deviceId, limit });
export const remoteDeviceCancel = (commandId: string) =>
  invoke<string>("remote_device_cancel", { commandId });

/** One node this machine may place work on, as `monkey daemon remote node-list --json` reports it (roadmap K17 S1). */
export interface RemoteNodeRow {
  alias: string;
  runner_id: string;
  node_name: string;
  residency: string;
  accepting: boolean;
  queue_depth: number;
  queue_capacity: number;
  last_seen_at_ms: number | null;
  /** `alive` | `stale` | `vanished` — computed by the daemon, never here, so
   * one implementation of the silence thresholds exists. */
  liveness: string;
}

/** One run this machine has placed on a node (roadmap K17 S2/S4). */
export interface RemotePlacementRow {
  submitted_run_id: string;
  alias: string;
  node_run_id: string;
  job_id: string;
  state: string;
  attempt: number;
  residency: string;
  /** Which of `select_node`'s keys chose that node — the record says why, not only where. */
  deciding_key: string;
  last_error: string | null;
  updated_at_ms: number;
}

export const remoteNodeList = () => invoke<{ nodes: RemoteNodeRow[] }>("remote_node_list");
export const remotePlacements = () => invoke<{ placements: RemotePlacementRow[] }>("remote_placements");
export const remoteNodeRefresh = (alias?: string) => invoke<string>("remote_node_refresh", { alias: alias ?? null });
export const remotePlacementSync = () => invoke<string>("remote_placement_sync");
export const remoteNodeLabel = (name: string | null, residency: string | null) =>
  invoke<string>("remote_node_label", { name, residency });

/**
 * What an older daemon — or one whose status could not be read — means.
 *
 * Wide open. A signal the app cannot see must not be treated as a refusal:
 * failing closed on a missing field would make an upgrade break every enqueue.
 */
export const OPEN_BACKPRESSURE: Backpressure = {
  state: "accepting",
  accepting: true,
  reason: null,
  detail: null,
  retryAfterMs: null,
  queueDepth: 0,
  queueCapacity: 0,
  queued: 0,
  held: 0,
};

function numberOr(value: unknown, fallback: number): number {
  return typeof value === "number" && Number.isFinite(value) ? value : fallback;
}

/**
 * The backpressure signal from a status payload, in one casing, always present.
 *
 * Absent field → {@link OPEN_BACKPRESSURE}. An unrecognised `state` is also
 * treated as accepting: a token this build has never heard of is a signal it
 * cannot honour, and guessing "closed" would block work over a vocabulary
 * mismatch.
 */
export function backpressureOf(status: Pick<DaemonStatus, "backpressure"> | null | undefined): Backpressure {
  const raw = status?.backpressure as Record<string, unknown> | null | undefined;
  if (!raw || typeof raw !== "object") return OPEN_BACKPRESSURE;
  const state = raw.state;
  if (state !== "accepting" && state !== "slow" && state !== "closed") return OPEN_BACKPRESSURE;
  return {
    state,
    accepting: state !== "closed",
    reason: (raw.reason as BackpressureReason | null) ?? null,
    detail: typeof raw.detail === "string" ? raw.detail : null,
    retryAfterMs: typeof raw.retryAfterMs === "number"
      ? raw.retryAfterMs
      : typeof raw.retry_after_ms === "number" ? raw.retry_after_ms : null,
    queueDepth: numberOr(raw.queueDepth ?? raw.queue_depth, 0),
    queueCapacity: numberOr(raw.queueCapacity ?? raw.queue_capacity, 0),
    queued: numberOr(raw.queued, 0),
    held: numberOr(raw.held, 0),
  };
}

/**
 * What kind of work a producer is about to send.
 *
 * The distinction only matters at `slow`, and it is the whole reason the state
 * is not a boolean:
 *
 * - `interactive` — a user is sitting there waiting on this turn. Deferring it
 *   is a refusal they did not ask for and cannot act on, so a `slow` signal is
 *   *reported* (the caller may show the sentence) and the work goes through.
 * - `batch` — a queued job nobody is watching. It can wait, which is exactly
 *   what `slow` is asking for, so this defers and the caller offers an override.
 *
 * `closed` blocks both: the daemon's own `enqueue` refuses there, so attempting
 * it only trades the signal for an error.
 */
export type ProducerWork = "interactive" | "batch";

export interface BackpressureGate {
  /** Whether to attempt the enqueue at all. */
  proceed: boolean;
  /** True when `proceed` is false only because a batch producer can wait — the
   * caller may offer "queue anyway". A blocked `closed` signal is not deferrable. */
  deferrable: boolean;
  signal: Backpressure;
}

export function backpressureGate(signal: Backpressure, work: ProducerWork): BackpressureGate {
  if (signal.state === "closed") return { proceed: false, deferrable: false, signal };
  if (signal.state === "slow" && work === "batch") return { proceed: false, deferrable: true, signal };
  return { proceed: true, deferrable: false, signal };
}

/**
 * The daemon's own sentence, plus its advisory retry hint.
 *
 * Mirrors `Backpressure::refusal()` on the Rust side so the text a user reads
 * here and the error the daemon would have returned cannot disagree. `fallback`
 * and `retryHint` are translated strings from the caller; `detail` is the
 * daemon's prose and is never translated — paraphrasing it would mean inventing
 * a claim about the queue.
 */
export function backpressureMessage(
  signal: Backpressure,
  fallback: string,
  retryHint: (retryAfterMs: number) => string,
): string {
  const detail = signal.detail ?? fallback;
  return signal.retryAfterMs === null ? detail : `${detail} ${retryHint(signal.retryAfterMs)}`;
}

/**
 * One arbitration decision, as `monkey daemon decisions --json` prints it
 * (`SchedulerDecision` in the CLI's `daemon/store.rs`, serialized camelCase).
 *
 * The point of the row is the last three fields: `measurement` names *which*
 * number decided it, and `measuredAtMs` is **that reading's own observation
 * time**, not when this row was written. A re-derived guess carrying a fresh
 * timestamp is exactly what that column exists to rule out, so it must never be
 * relabelled as the decision time — `decidedAtMs` is that.
 */
export interface SchedulerDecision {
  decidedAtMs: number;
  jobId: string;
  outcome: string;
  /** The class the run's frozen kind declares. */
  processClass: string;
  /** That class after aging promotion — what the ranking actually used. */
  effectiveClass: string;
  workspace: string | null;
  /** What this job was chosen over, most-nearly-chosen first. Bounded when written. */
  passedOver: string[];
  detail: string;
  measurement: string;
  measuredValue: number | null;
  measuredAtMs: number | null;
}

export type SchedulerOutcome = "admitted" | "held" | "preempted" | "resumed" | "rejected";

/** The log is bounded to this many rows on the daemon side. */
export const MAX_SCHEDULER_DECISIONS = 512;

/**
 * Recent scheduling decisions, newest first.
 *
 * `daemon_desktop_decisions` is a fixed-argument shell-out to
 * `monkey daemon decisions --limit N --json`, like every other daemon call here.
 * The CLI emits camelCase for every field of `SchedulerDecision`, and both ends
 * assert that spelling — the nested `backpressure` block on `daemon_desktop_status`
 * does not, so the two are not interchangeable.
 *
 * A read failure surfaces as the panel's read error rather than sample rows: a
 * decision log that shows invented decisions is worse than one that shows none.
 */
export const daemonDecisions = (limit = 50) =>
  invoke<SchedulerDecision[]>("daemon_desktop_decisions", { limit });

/** Exact daemon ownership comes from daemon state rather than a run-key
 * heuristic: task run keys are intentionally hashed before they reach the
 * shared ledger. */
export function isDaemonManagedRun(runId: string, managedRunIds: readonly string[]): boolean {
  return managedRunIds.includes(runId);
}

export function validateDaemonQueuePolicy(request: DaemonQueueRequest): string[] {
  const warnings: string[] = [];
  if ((request.allowPush || request.allowCreatePullRequest || request.allowReviewComment) && !request.ownedWorktree) {
    warnings.push("GitHub writes require an owned worktree.");
  }
  if (request.allowCreatePullRequest && !request.allowPush) {
    warnings.push("Creating a pull request also requires push permission.");
  }
  if (!request.branchPrefix.startsWith("codex/")) {
    warnings.push("Use a codex/ branch prefix for protected-branch isolation.");
  }
  return warnings;
}

export function validateRemotePairRequest(request: RemotePairRequest): string[] {
  const warnings: string[] = [];
  if (!Number.isInteger(request.expiresMinutes) || request.expiresMinutes < 1 || request.expiresMinutes > 1_440) {
    warnings.push("Pairing expiry must be between 1 and 1,440 minutes.");
  }
  if (request.actions.length === 0 || request.actions.some((action) => !REMOTE_ACTIONS.has(action))) {
    warnings.push("Select at least one valid remote action.");
  }
  if (new Set(request.actions).size !== request.actions.length) {
    warnings.push("Remote actions must not contain duplicates.");
  }
  if (request.runIds.length === 0 && request.workspaceIds.length === 0) {
    warnings.push("Declare at least one exact run ID or workspace ID.");
  }
  if (request.runIds.length > MAX_REMOTE_RUN_SCOPES || request.workspaceIds.length > MAX_REMOTE_WORKSPACE_SCOPES) {
    warnings.push("Remote pairing scope is too large.");
  }
  const invalidId = [...request.runIds, ...request.workspaceIds].some((value) =>
    value.length === 0 || value.length > 256 || value.includes("..") || !/^[A-Za-z0-9_.-]+$/.test(value),
  );
  if (invalidId) {
    warnings.push("Run and workspace IDs may contain only letters, numbers, '.', '-', and '_' and cannot contain '..'.");
  }
  if (!Number.isSafeInteger(request.maxArtifactBytes) || request.maxArtifactBytes < 1 || request.maxArtifactBytes > MAX_REMOTE_ARTIFACT_BYTES) {
    warnings.push("Artifact access must be limited to between 1 byte and 32 MiB.");
  }
  if ((request.actions.includes("approve") || request.actions.includes("read-artifacts")) && !request.actions.includes("view-runs")) {
    warnings.push("Approve and artifact access also require view-runs.");
  }
  const mobile = request.mobileCapabilities ?? [];
  if (mobile.some((capability) => !MOBILE_CAPABILITIES.has(capability))) {
    warnings.push("Unknown mobile companion capability.");
  }
  // Mirrors `protocol::validate_capabilities` on the node, so an invalid
  // combination is caught before the CLI is ever invoked.
  if (mobile.includes("chat") && !mobile.includes("view-sessions")) {
    warnings.push("Mobile chat also requires view-sessions.");
  }
  if (mobile.includes("run-workflows") && !mobile.includes("view-tasks")) {
    warnings.push("Mobile workflow launch also requires view-tasks.");
  }
  const device = request.deviceCapabilities ?? [];
  if (device.some((capability) => !(DEVICE_CAPABILITIES as readonly string[]).includes(capability))) {
    warnings.push("Unknown device hardware capability.");
  }
  // Same rule the node enforces: a continuous stream cannot be the only
  // microphone grant, or withdrawing microphone capture would leave the
  // microphone reachable.
  if (device.includes("voice_stream") && !device.includes("microphone_capture")) {
    warnings.push("Streaming voice also requires microphone_capture.");
  }
  return warnings;
}
