import { invoke } from "@tauri-apps/api/core";

/**
 * What a run got.
 *
 * `process_contained` is Windows: a job object bounded the process tree, its
 * committed memory and its window-station reach, and killed the tree at the end
 * of the run. It is deliberately not `os_sandboxed`, because the filesystem was
 * never confined — the real workspace stays reachable by absolute path.
 */
export type SandboxIsolation = "os_sandboxed" | "process_contained" | "process_only";

/**
 * What this machine can enforce, answerable before a run.
 *
 * Distinct from {@link SandboxIsolation}, which reports what a run *got* — true,
 * but only after the command has already executed. The panel offers the same Run
 * button everywhere, and generated MCP server code is probed through it, so the
 * answer is needed while the user is still deciding.
 *
 * `unavailable` is its own state: on macOS the sandbox binary is spawned
 * unconditionally, so if it is missing the run fails rather than silently
 * downgrading — a different problem from having no sandbox at all.
 *
 * `process_contained` sits between `os_enforced` and `process_only`, and closer to
 * `process_only` for any decision about running untrusted code: the kernel holds
 * the process tree, not the filesystem.
 */
export type SandboxEnforcement =
  | "os_enforced"
  | "process_contained"
  | "process_only"
  | "unavailable";

export const sandboxEnforcement = () => invoke<SandboxEnforcement>("sandbox_enforcement_probe");

export interface SandboxRunSummary {
  runId: string;
  isolation: SandboxIsolation;
  exitCode: number | null;
  timedOut: boolean;
  passed: boolean;
  durationMs: number;
  stdoutArtifactId: string;
  stderrArtifactId: string;
  stdoutExcerpt: string;
  stderrExcerpt: string;
  filesCopied: number;
}

export type SandboxRunStatus =
  | "queued"
  | "running"
  | "waiting_for_permission"
  | "paused"
  | "cancelling"
  | "succeeded"
  | "failed"
  | "cancelled"
  | "needs_reconciliation";

export interface SandboxRunListEntry {
  runId: string;
  status: SandboxRunStatus;
  task: string;
  createdAtMs: number;
  updatedAtMs: number;
}

export interface SandboxDiffEntry {
  path: string;
  status: "added" | "modified";
  sandboxSha256: string;
  workspaceSha256: string | null;
  sizeBytes: number;
}

export interface PromoteFileEntry {
  path: string;
  sha256: string;
  sizeBytes: number;
}

export interface SandboxPromotePreview {
  runId: string;
  digest: string;
  confirmationPhrase: string;
  files: PromoteFileEntry[];
  expiresAtMs: number;
}

export interface SandboxPromoteResult {
  runId: string;
  promotedFiles: string[];
}

export const runInSandbox = (
  command: string,
  options?: { timeoutMs?: number; allowNetwork?: boolean; approvedEnv?: string[] },
) =>
  invoke<SandboxRunSummary>("sandbox_run", {
    command,
    timeoutMs: options?.timeoutMs ?? null,
    allowNetwork: options?.allowNetwork ?? false,
    approvedEnv: options?.approvedEnv ?? [],
  });

export const listSandboxRuns = () => invoke<SandboxRunListEntry[]>("sandbox_list");

export const sandboxDiff = (runId: string) => invoke<SandboxDiffEntry[]>("sandbox_diff", { runId });

export const prepareSandboxPromote = (runId: string, files: string[]) =>
  invoke<SandboxPromotePreview>("sandbox_prepare_promote", { runId, files });

export const executeSandboxPromote = (runId: string, digest: string, confirmationPhrase: string) =>
  invoke<SandboxPromoteResult>("sandbox_execute_promote", { runId, digest, confirmationPhrase });

export const discardSandboxRun = (runId: string, reason?: string) =>
  invoke<void>("sandbox_discard", { runId, reason: reason ?? null });
