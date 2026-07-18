import { invoke } from "@tauri-apps/api/core";

export type SandboxIsolation = "os_sandboxed" | "process_only";

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
