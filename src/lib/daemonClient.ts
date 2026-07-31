import { invoke } from "@tauri-apps/api/core";

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
  return warnings;
}
