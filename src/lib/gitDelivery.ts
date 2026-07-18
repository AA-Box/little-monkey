import { invoke } from "@tauri-apps/api/core";

export interface DeliveryPolicy {
  allowedRemotes: string[];
  branchPrefix: string;
  protectedBranches: string[];
  allowPush: boolean;
  allowCreatePullRequest: boolean;
  allowReviewComment: boolean;
  allowForkWrites: boolean;
}

export interface WorktreeCreateRequest extends DeliveryPolicy {
  repositoryRoot: string;
  repositorySlug: string;
  baseRef: string;
  label: string;
}

export interface OwnershipMarker {
  schemaVersion: number;
  worktreeId: string;
  leaseNonce: string;
  repositoryId: string;
  repositorySlug: string;
  repositoryRoot: string;
  commonGitDir: string;
  canonicalPath: string;
  branch: string;
  baseOid: string;
  policy: DeliveryPolicy;
  createdAtMs: number;
}

export interface OwnedWorktreeRecord {
  marker: OwnershipMarker;
  state: "active" | "recovered" | "archived" | "cleaned";
  locked: boolean;
  lockReason: string | null;
  archivePath: string | null;
  createdAtMs: number;
  updatedAtMs: number;
}

export interface ChangedFile {
  path: string;
  oldPath: string | null;
  indexStatus: string;
  worktreeStatus: string;
  untracked: boolean;
  ignored: boolean;
}

export interface DiffText {
  text: string;
  truncated: boolean;
}

export interface WorktreeInspection {
  worktree: OwnedWorktreeRecord;
  headOid: string;
  ahead: number;
  behind: number;
  dirty: boolean;
  cleanupBlocked: boolean;
  files: ChangedFile[];
  diffs: {
    staged: DiffText;
    unstaged: DiffText;
    head: DiffText;
  };
}

export interface ReviewFinding {
  findingId: string;
  severity: "blocking" | "warning" | "suggestion";
  path: string;
  line: number;
  title: string;
  body: string;
}

export interface ReviewReport {
  reportId: string;
  repositorySlug: string;
  prNumber: number;
  headOid: string;
  model: string;
  summary: string;
  findings: ReviewFinding[];
  reportDigest: string;
  publishedCommentId: number | null;
  createdAtMs: number;
  updatedAtMs: number;
}

export interface ConfirmationPreview {
  digest: string;
  action: string;
  summary: string;
  impact: string;
  repositorySlug: string;
  branch: string | null;
  external: boolean;
  expiresAtMs: number;
  confirmationPhrase: string;
}

export interface DeliveryAuditEntry {
  auditId: number;
  occurredAtMs: number;
  action: string;
  target: string | null;
  requestDigest: string;
  outcome: "success" | "failed" | string;
  detail: string | null;
}

export interface MutationExecutionRecord {
  requestDigest: string;
  action: string;
  target: string;
  external: boolean;
  state:
    | "executing"
    | "completed"
    | "failed"
    | "needs_reconciliation"
    | "reconciled_completed"
    | "reconciled_not_applied"
    | string;
  executorInstance: string;
  confirmedAtMs: number;
  startedAtMs: number;
  finishedAtMs: number | null;
  result: unknown | null;
  error: string | null;
  resolution: string | null;
  resolutionNote: string | null;
  updatedAtMs: number;
}

export interface GitHubAuthStatus {
  available: boolean;
  authenticated: boolean;
  account: string | null;
  hostname: string;
  detail: string;
}

type WithWorktree<T extends object = Record<string, never>> = { worktreeId: string } & T;

export type DeliveryMutation =
  | { kind: "create_worktree"; payload: WorktreeCreateRequest }
  | { kind: "set_lock"; payload: WithWorktree<{ locked: boolean; reason: string | null }> }
  | { kind: "stage"; payload: WithWorktree<{ paths: string[] }> }
  | { kind: "commit"; payload: WithWorktree<{ paths: string[]; message: string }> }
  | { kind: "push"; payload: WithWorktree<{ remote: string }> }
  | { kind: "archive_worktree"; payload: WithWorktree }
  | { kind: "cleanup_worktree"; payload: WithWorktree }
  | { kind: "create_draft_pr"; payload: WithWorktree<{ base: string; title: string; body: string }> }
  | { kind: "update_draft_pr"; payload: WithWorktree<{ prNumber: number; title: string; body: string }> }
  | { kind: "publish_review"; payload: WithWorktree<{ reportId: string }> }
  | { kind: "queue_patch_task"; payload: WithWorktree<{ prNumber: number; commentId: number; model: string }> }
  | {
    kind: "resolve_reconciliation";
    payload: {
      requestDigest: string;
      resolution: "completed" | "not_applied";
      note: string;
    };
  };

export const listOwnedWorktrees = () =>
  invoke<OwnedWorktreeRecord[]>("m5_delivery_list_worktrees");

export const inspectOwnedWorktree = (worktreeId: string) =>
  invoke<WorktreeInspection>("m5_delivery_inspect_worktree", { worktreeId });

export const prepareDeliveryMutation = (mutation: DeliveryMutation) =>
  invoke<ConfirmationPreview>("m5_delivery_prepare_mutation", { mutation });

export const executeDeliveryMutation = (
  mutation: DeliveryMutation,
  digest: string,
  confirmation: string,
) => invoke<unknown>("m5_delivery_execute_mutation", { mutation, digest, confirmation });

export const deliveryAudit = (limit = 100) =>
  invoke<DeliveryAuditEntry[]>("m5_delivery_audit", { limit });

export const deliveryReconciliations = () =>
  invoke<MutationExecutionRecord[]>("m5_delivery_reconciliations");

export const githubAuthStatus = () =>
  invoke<GitHubAuthStatus>("m5_github_auth_status");

export const githubIssue = (worktreeId: string, number: number) =>
  invoke<Record<string, unknown>>("m5_github_issue", { worktreeId, number });

export const githubPullRequest = (worktreeId: string, number: number) =>
  invoke<Record<string, unknown>>("m5_github_pull_request", { worktreeId, number });

export const githubReviewThreads = (worktreeId: string, number: number) =>
  invoke<Record<string, unknown>>("m5_github_review_threads", { worktreeId, number });

export const githubChecks = (worktreeId: string, number: number) =>
  invoke<Record<string, unknown>>("m5_github_checks", { worktreeId, number });

export const reviewPullRequest = (worktreeId: string, prNumber: number, model: string) =>
  invoke<ReviewReport>("m5_review_pull_request", { request: { worktreeId, prNumber, model } });

export const reviewReports = (worktreeId: string, prNumber: number) =>
  invoke<ReviewReport[]>("m5_review_reports", { worktreeId, prNumber });

export function validateCreateRequest(request: WorktreeCreateRequest): string[] {
  const errors: string[] = [];
  if (!/^(?:\/|[A-Za-z]:[\\/])/.test(request.repositoryRoot)) errors.push("Open a primary workspace first.");
  if (!/^[A-Za-z0-9_.-]+\/[A-Za-z0-9_.-]+$/.test(request.repositorySlug)) {
    errors.push("Repository must be exactly owner/name.");
  }
  if (!request.branchPrefix.endsWith("/") || request.branchPrefix.includes("..")) {
    errors.push("Branch prefix must be safe and end in /.");
  }
  if (request.allowedRemotes.length === 0) errors.push("At least one declared remote is required.");
  if ((request.allowCreatePullRequest || request.allowReviewComment) && !request.allowPush) {
    errors.push("PR and review writes require owned-branch push permission.");
  }
  return errors;
}

export function isExternalMutation(mutation: DeliveryMutation): boolean {
  return ["push", "create_draft_pr", "update_draft_pr", "publish_review"].includes(mutation.kind);
}
