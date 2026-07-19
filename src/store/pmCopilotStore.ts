import { create } from "zustand";

import {
  cancelPmPlanGeneration,
  generatePmPlan,
  pmCopilotGenerationKey,
  pmPlanToMarkdown,
  savePmPlanToWorkspace,
  slugifyGoal,
  type PmMilestone,
  type PmPlan,
  type PmRisk,
  type PmRiskSeverity,
  type PmUserStory,
} from "../lib/pmCopilot";

/**
 * Draft state for Product Manager Copilot (ROADMAP.md Phase 7): one goal,
 * one generated-then-editable `PmPlan`, and the save-to-workspace flow. A
 * single-draft store (no list of past drafts) — mirrors the scope of the
 * MVP acceptance criterion ("a product idea can become a scoped, testable
 * work plan"), not a plan library. Starting a new goal via `reset()`
 * discards the previous draft entirely; nothing here is persisted across an
 * app restart (unlike the file it writes on Save, which is the actual
 * durable artifact).
 */

export type PmCopilotStatus = "idle" | "generating" | "ready" | "error";
export type PmCopilotSaveStatus = "idle" | "saving" | "saved" | "error";

function makeDraftId(): string {
  return typeof crypto !== "undefined" && "randomUUID" in crypto ? crypto.randomUUID() : `draft-${Date.now()}`;
}

interface PmCopilotStoreState {
  draftId: string;
  goal: string;
  status: PmCopilotStatus;
  error: string | null;
  plan: PmPlan | null;
  modelLabel: string | null;
  generatedAtMs: number | null;
  slug: string;
  slugTouched: boolean;
  saveStatus: PmCopilotSaveStatus;
  saveError: string | null;
  savedPath: string | null;

  setGoal: (goal: string) => void;
  setSlug: (slug: string) => void;
  generate: () => Promise<void>;
  cancelGenerate: () => void;

  updatePrdSummary: (text: string) => void;

  addUserStory: () => void;
  updateUserStory: (index: number, field: keyof PmUserStory, value: string) => void;
  removeUserStory: (index: number) => void;

  addAcceptanceCriterion: () => void;
  updateAcceptanceCriterion: (index: number, value: string) => void;
  removeAcceptanceCriterion: (index: number) => void;

  addRisk: () => void;
  updateRisk: (index: number, field: keyof PmRisk, value: string) => void;
  removeRisk: (index: number) => void;

  addMilestone: () => void;
  updateMilestone: (index: number, field: keyof PmMilestone, value: string) => void;
  removeMilestone: (index: number) => void;

  markdownPreview: () => string | null;
  save: () => Promise<void>;
  reset: () => void;
}

function emptyPlan(goal: string): PmPlan {
  return { goal, prdSummary: "", userStories: [], acceptanceCriteria: [], risks: [], milestones: [] };
}

function mutatePlan(state: PmCopilotStoreState, mutator: (plan: PmPlan) => PmPlan): Partial<PmCopilotStoreState> {
  return { plan: mutator(state.plan ?? emptyPlan(state.goal)) };
}

export const usePmCopilotStore = create<PmCopilotStoreState>((set, get) => ({
  draftId: makeDraftId(),
  goal: "",
  status: "idle",
  error: null,
  plan: null,
  modelLabel: null,
  generatedAtMs: null,
  slug: "",
  slugTouched: false,
  saveStatus: "idle",
  saveError: null,
  savedPath: null,

  setGoal: (goal) =>
    set((state) => ({
      goal,
      // Keep the auto-derived slug in sync with the goal until the user
      // types into the filename field directly (`setSlug` below flips
      // `slugTouched`, same "don't clobber an explicit user edit" rule
      // every other derived-field pattern in this codebase follows).
      slug: state.slugTouched ? state.slug : slugifyGoal(goal),
    })),

  setSlug: (slug) => set({ slug, slugTouched: true }),

  generate: async () => {
    const { draftId, goal } = get();
    set({ status: "generating", error: null, saveStatus: "idle", saveError: null, savedPath: null });
    try {
      const { plan, target } = await generatePmPlan(draftId, goal);
      set((state) => ({
        status: "ready",
        plan,
        modelLabel: `${target.label} · ${target.displayName}`,
        generatedAtMs: Date.now(),
        slug: state.slugTouched ? state.slug : slugifyGoal(plan.goal || goal),
      }));
    } catch (error) {
      set({
        status: "error",
        error: error instanceof Error ? error.message : String(error),
      });
    }
  },

  cancelGenerate: () => {
    const { draftId } = get();
    cancelPmPlanGeneration(pmCopilotGenerationKey(draftId));
  },

  updatePrdSummary: (text) => set((state) => mutatePlan(state, (plan) => ({ ...plan, prdSummary: text }))),

  addUserStory: () =>
    set((state) =>
      mutatePlan(state, (plan) => ({
        ...plan,
        userStories: [...plan.userStories, { asA: "", iWant: "", soThat: "" }],
      })),
    ),
  updateUserStory: (index, field, value) =>
    set((state) =>
      mutatePlan(state, (plan) => ({
        ...plan,
        userStories: plan.userStories.map((story, i) => (i === index ? { ...story, [field]: value } : story)),
      })),
    ),
  removeUserStory: (index) =>
    set((state) =>
      mutatePlan(state, (plan) => ({
        ...plan,
        userStories: plan.userStories.filter((_, i) => i !== index),
      })),
    ),

  addAcceptanceCriterion: () =>
    set((state) => mutatePlan(state, (plan) => ({ ...plan, acceptanceCriteria: [...plan.acceptanceCriteria, ""] }))),
  updateAcceptanceCriterion: (index, value) =>
    set((state) =>
      mutatePlan(state, (plan) => ({
        ...plan,
        acceptanceCriteria: plan.acceptanceCriteria.map((entry, i) => (i === index ? value : entry)),
      })),
    ),
  removeAcceptanceCriterion: (index) =>
    set((state) =>
      mutatePlan(state, (plan) => ({
        ...plan,
        acceptanceCriteria: plan.acceptanceCriteria.filter((_, i) => i !== index),
      })),
    ),

  addRisk: () =>
    set((state) =>
      mutatePlan(state, (plan) => ({
        ...plan,
        risks: [...plan.risks, { description: "", severity: "medium" as PmRiskSeverity, mitigation: "" }],
      })),
    ),
  updateRisk: (index, field, value) =>
    set((state) =>
      mutatePlan(state, (plan) => ({
        ...plan,
        risks: plan.risks.map((risk, i) =>
          i === index
            ? { ...risk, [field]: field === "severity" ? (value as PmRiskSeverity) : value }
            : risk,
        ),
      })),
    ),
  removeRisk: (index) =>
    set((state) => mutatePlan(state, (plan) => ({ ...plan, risks: plan.risks.filter((_, i) => i !== index) }))),

  addMilestone: () =>
    set((state) => mutatePlan(state, (plan) => ({ ...plan, milestones: [...plan.milestones, { name: "", summary: "" }] }))),
  updateMilestone: (index, field, value) =>
    set((state) =>
      mutatePlan(state, (plan) => ({
        ...plan,
        milestones: plan.milestones.map((milestone, i) => (i === index ? { ...milestone, [field]: value } : milestone)),
      })),
    ),
  removeMilestone: (index) =>
    set((state) =>
      mutatePlan(state, (plan) => ({ ...plan, milestones: plan.milestones.filter((_, i) => i !== index) })),
    ),

  markdownPreview: () => {
    const { plan, generatedAtMs, modelLabel } = get();
    if (!plan) return null;
    return pmPlanToMarkdown(plan, generatedAtMs ?? Date.now(), modelLabel ?? "an unspecified model");
  },

  save: async () => {
    const { plan, generatedAtMs, modelLabel, slug } = get();
    if (!plan) {
      set({ saveStatus: "error", saveError: "Generate a plan before saving." });
      return;
    }
    set({ saveStatus: "saving", saveError: null });
    try {
      const markdown = pmPlanToMarkdown(plan, generatedAtMs ?? Date.now(), modelLabel ?? "an unspecified model");
      const path = await savePmPlanToWorkspace(markdown, slug);
      set({ saveStatus: "saved", savedPath: path });
    } catch (error) {
      set({ saveStatus: "error", saveError: error instanceof Error ? error.message : String(error) });
    }
  },

  reset: () =>
    set({
      draftId: makeDraftId(),
      goal: "",
      status: "idle",
      error: null,
      plan: null,
      modelLabel: null,
      generatedAtMs: null,
      slug: "",
      slugTouched: false,
      saveStatus: "idle",
      saveError: null,
      savedPath: null,
    }),
}));
