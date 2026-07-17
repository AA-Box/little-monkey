import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";

export const ISSUE_TO_PR_PROGRESS_EVENT = "issue-to-pr://progress";

export type IssueToPrStatus =
  | "planning"
  | "implementing"
  | "checking"
  | "opening_pr"
  | "awaiting_review"
  | "done"
  | "failed"
  | "cancelled";

export interface IssueToPrCheckOutcome {
  label: string;
  command: string;
  passed: boolean;
  code: number | null;
  outputExcerpt: string;
}

export interface IssueToPrRun {
  runId: string;
  issueUrl: string;
  repositorySlug: string;
  issueNumber: number;
  issueTitle: string;
  issueBody: string;
  worktreeId: string;
  branch: string;
  workspaceLabel: string;
  status: IssueToPrStatus;
  prNumber: number | null;
  prUrl: string | null;
  checks: IssueToPrCheckOutcome[];
  error: string | null;
  durableRunId: string | null;
  createdAtMs: number;
  updatedAtMs: number;
}

export const TERMINAL_ISSUE_TO_PR_STATUSES: ReadonlySet<IssueToPrStatus> = new Set([
  "done",
  "failed",
  "cancelled",
]);

export function isTerminalIssueToPrStatus(status: IssueToPrStatus): boolean {
  return TERMINAL_ISSUE_TO_PR_STATUSES.has(status);
}

export const startIssueToPr = (issueUrl: string) =>
  invoke<IssueToPrRun>("issue_to_pr_start", { issueUrl });

export const getIssueToPrStatus = (runId: string) =>
  invoke<IssueToPrRun>("issue_to_pr_status", { runId });

export const listIssueToPrRuns = () => invoke<IssueToPrRun[]>("issue_to_pr_list");

export const cancelIssueToPr = (runId: string) =>
  invoke<IssueToPrRun>("issue_to_pr_cancel", { runId });

export interface AdvanceIssueToPrOptions {
  error?: string | null;
  prNumber?: number | null;
  prUrl?: string | null;
  durableRunId?: string | null;
}

export const advanceIssueToPr = (
  runId: string,
  status: IssueToPrStatus,
  options: AdvanceIssueToPrOptions = {},
) =>
  invoke<IssueToPrRun>("issue_to_pr_advance", {
    runId,
    status,
    error: options.error ?? null,
    prNumber: options.prNumber ?? null,
    prUrl: options.prUrl ?? null,
    durableRunId: options.durableRunId ?? null,
  });

export const runIssueToPrChecks = (runId: string) =>
  invoke<IssueToPrRun>("issue_to_pr_run_checks", { runId });

export function listenIssueToPrProgress(
  onProgress: (run: IssueToPrRun) => void,
): Promise<UnlistenFn> {
  return listen<IssueToPrRun>(ISSUE_TO_PR_PROGRESS_EVENT, (event) => onProgress(event.payload));
}
