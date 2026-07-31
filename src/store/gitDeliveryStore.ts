import { create } from "zustand";

import * as api from "../lib/gitDelivery";
import type {
  ConfirmationPreview,
  DeliveryAuditEntry,
  DeliveryMutation,
  GitHubAuthStatus,
  MutationExecutionRecord,
  OwnedWorktreeRecord,
  ReviewReport,
  WorktreeInspection,
} from "../lib/gitDelivery";
import { errorMessage } from "../lib/errors";

function errorText(error: unknown): string {
  return errorMessage(error);
}

interface GitDeliveryState {
  worktrees: OwnedWorktreeRecord[];
  selectedWorktreeId: string | null;
  inspection: WorktreeInspection | null;
  auth: GitHubAuthStatus | null;
  issue: Record<string, unknown> | null;
  pullRequest: Record<string, unknown> | null;
  reviewThreads: Record<string, unknown> | null;
  checks: Record<string, unknown> | null;
  reports: ReviewReport[];
  audit: DeliveryAuditEntry[];
  reconciliations: MutationExecutionRecord[];
  pendingMutation: DeliveryMutation | null;
  preview: ConfirmationPreview | null;
  busy: Record<string, boolean>;
  error: string | null;
  notice: string | null;

  clearMessages: () => void;
  refresh: () => Promise<void>;
  selectWorktree: (worktreeId: string | null) => Promise<void>;
  refreshInspection: () => Promise<void>;
  refreshAuth: () => Promise<void>;
  prepare: (mutation: DeliveryMutation) => Promise<ConfirmationPreview>;
  cancelPreview: () => void;
  executePrepared: (confirmation: string) => Promise<unknown>;
  loadGitHub: (number: number) => Promise<void>;
  runReview: (prNumber: number, model: string) => Promise<ReviewReport>;
  refreshReports: (prNumber: number) => Promise<void>;
  refreshAudit: () => Promise<void>;
}

export const useGitDeliveryStore = create<GitDeliveryState>((set, get) => {
  const perform = async <T>(key: string, task: () => Promise<T>): Promise<T> => {
    set((state) => ({
      busy: { ...state.busy, [key]: true },
      error: null,
      notice: null,
    }));
    try {
      return await task();
    } catch (error) {
      set({ error: errorText(error) });
      throw error;
    } finally {
      set((state) => ({ busy: { ...state.busy, [key]: false } }));
    }
  };

  return {
    worktrees: [],
    selectedWorktreeId: null,
    inspection: null,
    auth: null,
    issue: null,
    pullRequest: null,
    reviewThreads: null,
    checks: null,
    reports: [],
    audit: [],
    reconciliations: [],
    pendingMutation: null,
    preview: null,
    busy: {},
    error: null,
    notice: null,

    clearMessages: () => set({ error: null, notice: null }),

    refresh: () => perform("refresh", async () => {
      const worktrees = await api.listOwnedWorktrees();
      let selectedWorktreeId = get().selectedWorktreeId;
      if (!selectedWorktreeId || !worktrees.some((item) => item.marker.worktreeId === selectedWorktreeId)) {
        selectedWorktreeId = worktrees.find((item) => item.state !== "cleaned")?.marker.worktreeId ?? null;
      }
      set({ worktrees, selectedWorktreeId });
      if (selectedWorktreeId) {
        try {
          const inspection = await api.inspectOwnedWorktree(selectedWorktreeId);
          set({ inspection });
        } catch (error) {
          set({ inspection: null, error: errorText(error) });
        }
      } else {
        set({ inspection: null });
      }
    }),

    selectWorktree: (worktreeId) => perform("inspect", async () => {
      set({
        selectedWorktreeId: worktreeId,
        issue: null,
        pullRequest: null,
        reviewThreads: null,
        checks: null,
        reports: [],
      });
      if (!worktreeId) {
        set({ inspection: null });
        return;
      }
      set({ inspection: await api.inspectOwnedWorktree(worktreeId) });
    }),

    refreshInspection: () => perform("inspect", async () => {
      const worktreeId = get().selectedWorktreeId;
      if (!worktreeId) {
        set({ inspection: null });
        return;
      }
      set({ inspection: await api.inspectOwnedWorktree(worktreeId) });
    }),

    refreshAuth: () => perform("auth", async () => {
      set({ auth: await api.githubAuthStatus() });
    }),

    prepare: (mutation) => perform("prepare", async () => {
      const preview = await api.prepareDeliveryMutation(mutation);
      set({ pendingMutation: mutation, preview });
      return preview;
    }),

    cancelPreview: () => set({ pendingMutation: null, preview: null }),

    executePrepared: (confirmation) => perform("execute", async () => {
      const { pendingMutation, preview } = get();
      if (!pendingMutation || !preview) throw new Error("Open an exact mutation preview first.");
      if (Date.now() > preview.expiresAtMs) throw new Error("This confirmation preview expired. Open a new preview.");
      const result = await api.executeDeliveryMutation(
        pendingMutation,
        preview.digest,
        confirmation,
      );
      const created = pendingMutation.kind === "create_worktree"
        && typeof result === "object"
        && result !== null
        && "marker" in result
        ? result as OwnedWorktreeRecord
        : null;
      set({
        pendingMutation: null,
        preview: null,
        notice: `${preview.summary} completed.`,
        selectedWorktreeId: created?.marker.worktreeId ?? get().selectedWorktreeId,
      });
      await Promise.all([get().refresh(), get().refreshAudit()]);
      return result;
    }),

    loadGitHub: (number) => perform("github", async () => {
      const worktreeId = get().selectedWorktreeId;
      if (!worktreeId) throw new Error("Select an owned worktree first.");
      if (!Number.isInteger(number) || number < 1) throw new Error("Enter a positive issue or PR number.");
      const [issue, pullRequest, reviewThreads, checks] = await Promise.allSettled([
        api.githubIssue(worktreeId, number),
        api.githubPullRequest(worktreeId, number),
        api.githubReviewThreads(worktreeId, number),
        api.githubChecks(worktreeId, number),
      ]);
      set({
        issue: issue.status === "fulfilled" ? issue.value : null,
        pullRequest: pullRequest.status === "fulfilled" ? pullRequest.value : null,
        reviewThreads: reviewThreads.status === "fulfilled" ? reviewThreads.value : null,
        checks: checks.status === "fulfilled" ? checks.value : null,
      });
      if ([issue, pullRequest].every((result) => result.status === "rejected")) {
        const reason = issue.status === "rejected" ? issue.reason : pullRequest.status === "rejected" ? pullRequest.reason : "Not found";
        throw new Error(errorText(reason));
      }
    }),

    runReview: (prNumber, model) => perform("review", async () => {
      const worktreeId = get().selectedWorktreeId;
      if (!worktreeId) throw new Error("Select an owned worktree first.");
      if (!model.trim()) throw new Error("Enter a local Ollama model.");
      const report = await api.reviewPullRequest(worktreeId, prNumber, model.trim());
      set((state) => ({
        reports: [report, ...state.reports.filter((item) => item.reportId !== report.reportId)],
        notice: `Local review completed with ${report.findings.length} line-mapped finding(s).`,
      }));
      return report;
    }),

    refreshReports: (prNumber) => perform("reports", async () => {
      const worktreeId = get().selectedWorktreeId;
      if (!worktreeId) throw new Error("Select an owned worktree first.");
      set({ reports: await api.reviewReports(worktreeId, prNumber) });
    }),

    refreshAudit: () => perform("audit", async () => {
      const [audit, reconciliations] = await Promise.all([
        api.deliveryAudit(100),
        api.deliveryReconciliations(),
      ]);
      set({ audit, reconciliations });
    }),
  };
});
