import { create } from "zustand";

import {
  generateCrossRepoPlan,
  type CrossRepoPlan,
  type CrossRepoPlanStep,
} from "../lib/crossRepoChangePlanner";
import * as gitDelivery from "../lib/gitDelivery";
import type {
  ConfirmationPreview,
  DeliveryMutation,
  OwnedWorktreeRecord,
} from "../lib/gitDelivery";
import { useWorkspaceStore } from "./workspaceStore";

function errorText(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

/** Per-step git delivery fields the user fills in before "create branch" can
 * fire — the plan itself only knows about workspace roots, not GitHub slugs
 * or branch-naming policy, so these live alongside the plan rather than
 * inside `CrossRepoPlanStep`. */
export interface StepGitConfig {
  repositorySlug: string;
  baseRef: string;
  branchPrefix: string;
  label: string;
}

export interface CreatedBranch {
  worktreeId: string;
  branch: string;
}

export type CrossRepoPlanStatus = "draft" | "approved";

function defaultGitConfig(planId: string, step: CrossRepoPlanStep): StepGitConfig {
  return {
    repositorySlug: "",
    baseRef: "main",
    branchPrefix: `cross-repo/${planId.slice(0, 8)}/`,
    label: step.rootLabel || `step-${step.order}`,
  };
}

interface CrossRepoChangePlannerState {
  description: string;
  plan: CrossRepoPlan | null;
  status: CrossRepoPlanStatus | null;
  approvedAtMs: number | null;
  gitConfigByStep: Record<string, StepGitConfig>;
  createdBranchByStep: Record<string, CreatedBranch>;
  preparingStepId: string | null;
  pendingMutation: DeliveryMutation | null;
  preview: ConfirmationPreview | null;
  busy: Record<string, boolean>;
  error: string | null;
  notice: string | null;

  setDescription: (description: string) => void;
  clearMessages: () => void;
  generate: () => Promise<void>;
  updateStepField: (
    stepId: string,
    field: "summary" | "changes" | "risks" | "rollback",
    value: string,
  ) => void;
  moveStep: (stepId: string, direction: "up" | "down") => void;
  updateGitConfig: (stepId: string, patch: Partial<StepGitConfig>) => void;
  approvePlan: () => void;
  startOver: () => void;
  prepareBranchForStep: (stepId: string) => Promise<ConfirmationPreview>;
  cancelPrepare: () => void;
  confirmBranch: (confirmation: string) => Promise<OwnedWorktreeRecord | null>;
}

function renumber(steps: CrossRepoPlanStep[]): CrossRepoPlanStep[] {
  return steps.map((step, index) => ({ ...step, order: index + 1 }));
}

export const useCrossRepoChangePlannerStore = create<CrossRepoChangePlannerState>((set, get) => {
  const perform = async <T>(key: string, task: () => Promise<T>): Promise<T> => {
    set((state) => ({ busy: { ...state.busy, [key]: true }, error: null, notice: null }));
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
    description: "",
    plan: null,
    status: null,
    approvedAtMs: null,
    gitConfigByStep: {},
    createdBranchByStep: {},
    preparingStepId: null,
    pendingMutation: null,
    preview: null,
    busy: {},
    error: null,
    notice: null,

    setDescription: (description) => set({ description }),

    clearMessages: () => set({ error: null, notice: null }),

    generate: () =>
      perform("generate", async () => {
        const roots = useWorkspaceStore.getState().roots;
        const plan = await generateCrossRepoPlan(get().description, roots);
        const gitConfigByStep: Record<string, StepGitConfig> = {};
        for (const step of plan.steps) {
          gitConfigByStep[step.stepId] = defaultGitConfig(plan.planId, step);
        }
        set({
          plan,
          status: "draft",
          approvedAtMs: null,
          gitConfigByStep,
          createdBranchByStep: {},
          preparingStepId: null,
          pendingMutation: null,
          preview: null,
          notice: `Plan generated with ${plan.steps.length} step(s). Review and approve before anything touches a repository.`,
        });
      }),

    updateStepField: (stepId, field, value) =>
      set((state) => {
        if (!state.plan) return state;
        return {
          plan: {
            ...state.plan,
            steps: state.plan.steps.map((step) =>
              step.stepId === stepId ? { ...step, [field]: value } : step,
            ),
          },
        };
      }),

    moveStep: (stepId, direction) =>
      set((state) => {
        if (!state.plan) return state;
        const steps = [...state.plan.steps];
        const index = steps.findIndex((step) => step.stepId === stepId);
        const swapWith = direction === "up" ? index - 1 : index + 1;
        if (index === -1 || swapWith < 0 || swapWith >= steps.length) return state;
        [steps[index], steps[swapWith]] = [steps[swapWith], steps[index]];
        return { plan: { ...state.plan, steps: renumber(steps) } };
      }),

    updateGitConfig: (stepId, patch) =>
      set((state) => ({
        gitConfigByStep: {
          ...state.gitConfigByStep,
          [stepId]: { ...state.gitConfigByStep[stepId], ...patch },
        },
      })),

    // The acceptance gate this whole feature exists for: nothing in this
    // store can reach `prepareBranchForStep` until `status` flips to
    // "approved" here, and this is the ONLY place that happens — no
    // generate/edit path sets it implicitly.
    approvePlan: () =>
      set((state) => {
        if (!state.plan) return state;
        return { status: "approved", approvedAtMs: Date.now(), notice: "Plan approved. You can now create a branch per step." };
      }),

    startOver: () =>
      set({
        description: "",
        plan: null,
        status: null,
        approvedAtMs: null,
        gitConfigByStep: {},
        createdBranchByStep: {},
        preparingStepId: null,
        pendingMutation: null,
        preview: null,
        error: null,
        notice: null,
      }),

    prepareBranchForStep: (stepId) =>
      perform("prepare", async () => {
        const { plan, status, gitConfigByStep } = get();
        if (!plan || status !== "approved") {
          throw new Error("Approve the plan before creating any branch.");
        }
        const step = plan.steps.find((candidate) => candidate.stepId === stepId);
        if (!step) throw new Error("Unknown plan step.");
        const config = gitConfigByStep[stepId];
        if (!config) throw new Error("Missing git configuration for this step.");

        const request: gitDelivery.WorktreeCreateRequest = {
          repositoryRoot: step.rootPath,
          repositorySlug: config.repositorySlug.trim(),
          baseRef: config.baseRef.trim() || "main",
          label: config.label.trim() || step.rootLabel,
          branchPrefix: config.branchPrefix.trim(),
          allowedRemotes: ["origin"],
          protectedBranches: ["main", "master"],
          // Branch creation only — this panel never pushes or opens a PR;
          // that stays a manual follow-up in the existing Git Delivery
          // settings panel, so every write permission here stays off.
          allowPush: false,
          allowCreatePullRequest: false,
          allowReviewComment: false,
          allowForkWrites: false,
        };
        const validationErrors = gitDelivery.validateCreateRequest(request);
        if (validationErrors.length > 0) throw new Error(validationErrors.join(" "));

        const mutation: DeliveryMutation = { kind: "create_worktree", payload: request };
        const preview = await gitDelivery.prepareDeliveryMutation(mutation);
        set({ preparingStepId: stepId, pendingMutation: mutation, preview });
        return preview;
      }),

    cancelPrepare: () => set({ preparingStepId: null, pendingMutation: null, preview: null }),

    confirmBranch: (confirmation) =>
      perform("confirm", async () => {
        const { pendingMutation, preview, preparingStepId } = get();
        if (!pendingMutation || !preview || !preparingStepId) {
          throw new Error("Open an exact branch-creation preview first.");
        }
        if (Date.now() > preview.expiresAtMs) {
          throw new Error("This confirmation preview expired. Prepare the branch again.");
        }
        const result = await gitDelivery.executeDeliveryMutation(pendingMutation, preview.digest, confirmation);
        const created =
          typeof result === "object" && result !== null && "marker" in result
            ? (result as OwnedWorktreeRecord)
            : null;
        set((state) => ({
          preparingStepId: null,
          pendingMutation: null,
          preview: null,
          notice: created ? `Branch "${created.marker.branch}" created.` : "Branch created.",
          createdBranchByStep: created
            ? {
                ...state.createdBranchByStep,
                [preparingStepId]: { worktreeId: created.marker.worktreeId, branch: created.marker.branch },
              }
            : state.createdBranchByStep,
        }));
        return created;
      }),
  };
});
