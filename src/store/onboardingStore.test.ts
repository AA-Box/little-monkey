import { beforeAll, beforeEach, describe, expect, it, vi } from "vitest";

// vitest's "node" test environment has no `localStorage` global (see
// `workflowDraftStore.test.ts`/`skillProposalStore.test.ts` for the same
// shim) — stub an in-memory one so the store's real persistence path is
// exercised rather than skipped.
beforeAll(() => {
  const values = new Map<string, string>();
  Object.defineProperty(globalThis, "localStorage", {
    configurable: true,
    value: {
      clear: () => values.clear(),
      getItem: (key: string) => values.get(key) ?? null,
      setItem: (key: string, value: string) => values.set(key, value),
      removeItem: (key: string) => values.delete(key),
    },
  });
});

import { ONBOARDING_STEPS, ONBOARDING_STORAGE_KEY, useOnboardingStore } from "./onboardingStore";

beforeEach(() => {
  localStorage.clear();
  useOnboardingStore.setState({
    hasCompletedOnboarding: false,
    currentStep: 0,
    selectedTemplateId: null,
  });
});

describe("onboardingStore defaults / hydration", () => {
  it("defaults to step 0, not completed, no template — exercising the real hydrate() path", async () => {
    // Same "exercise the real hydration path via a fresh module instance"
    // rationale as `settingsStore.test.ts`'s default-hydration tests —
    // `beforeEach` above forces these values by hand, so only a fresh
    // import actually covers `hydrate()`/`defaults()`.
    localStorage.clear();
    vi.resetModules();
    const fresh = await import("./onboardingStore");
    const state = fresh.useOnboardingStore.getState();
    expect(state.hasCompletedOnboarding).toBe(false);
    expect(state.currentStep).toBe(0);
    expect(state.selectedTemplateId).toBeNull();
  });

  it("falls back to defaults for a corrupt persisted blob", async () => {
    localStorage.setItem(ONBOARDING_STORAGE_KEY, "{not valid json");
    vi.resetModules();
    const fresh = await import("./onboardingStore");
    const state = fresh.useOnboardingStore.getState();
    expect(state.hasCompletedOnboarding).toBe(false);
    expect(state.currentStep).toBe(0);
  });

  it("falls back to defaults for an out-of-range persisted step", async () => {
    localStorage.setItem(
      ONBOARDING_STORAGE_KEY,
      JSON.stringify({ hasCompletedOnboarding: false, currentStep: 99, selectedTemplateId: null }),
    );
    vi.resetModules();
    const fresh = await import("./onboardingStore");
    expect(fresh.useOnboardingStore.getState().currentStep).toBe(0);
  });

  it("rehydrates a previously-completed, mid-selection state", async () => {
    localStorage.setItem(
      ONBOARDING_STORAGE_KEY,
      JSON.stringify({ hasCompletedOnboarding: true, currentStep: 3, selectedTemplateId: "research" }),
    );
    vi.resetModules();
    const fresh = await import("./onboardingStore");
    const state = fresh.useOnboardingStore.getState();
    expect(state.hasCompletedOnboarding).toBe(true);
    expect(state.currentStep).toBe(3);
    expect(state.selectedTemplateId).toBe("research");
  });
});

describe("onboardingStore step navigation", () => {
  it("advances one step at a time with nextStep()", () => {
    useOnboardingStore.getState().nextStep();
    expect(useOnboardingStore.getState().currentStep).toBe(1);
    useOnboardingStore.getState().nextStep();
    expect(useOnboardingStore.getState().currentStep).toBe(2);
  });

  it("never advances past the last step", () => {
    useOnboardingStore.getState().goToStep(ONBOARDING_STEPS.length - 1);
    useOnboardingStore.getState().nextStep();
    expect(useOnboardingStore.getState().currentStep).toBe(ONBOARDING_STEPS.length - 1);
  });

  it("never goes back before the first step", () => {
    useOnboardingStore.getState().previousStep();
    expect(useOnboardingStore.getState().currentStep).toBe(0);
  });

  it("goToStep clamps an out-of-range index into range", () => {
    useOnboardingStore.getState().goToStep(-5);
    expect(useOnboardingStore.getState().currentStep).toBe(0);
    useOnboardingStore.getState().goToStep(999);
    expect(useOnboardingStore.getState().currentStep).toBe(ONBOARDING_STEPS.length - 1);
  });

  it("goToStep rounds a fractional index", () => {
    useOnboardingStore.getState().goToStep(2.6);
    expect(useOnboardingStore.getState().currentStep).toBe(3);
  });

  it("previousStep/nextStep round-trip back to the same step", () => {
    useOnboardingStore.getState().goToStep(2);
    useOnboardingStore.getState().nextStep();
    useOnboardingStore.getState().previousStep();
    expect(useOnboardingStore.getState().currentStep).toBe(2);
  });
});

describe("onboardingStore template selection", () => {
  it("records a selected template id", () => {
    useOnboardingStore.getState().selectTemplate("code-review");
    expect(useOnboardingStore.getState().selectedTemplateId).toBe("code-review");
  });

  it("clears the selection when passed null", () => {
    useOnboardingStore.getState().selectTemplate("qa");
    useOnboardingStore.getState().selectTemplate(null);
    expect(useOnboardingStore.getState().selectedTemplateId).toBeNull();
  });

  it("switching templates replaces, not accumulates, the selection", () => {
    useOnboardingStore.getState().selectTemplate("research");
    useOnboardingStore.getState().selectTemplate("docs");
    expect(useOnboardingStore.getState().selectedTemplateId).toBe("docs");
  });
});

describe("onboardingStore completion flags", () => {
  it("completeOnboarding sets hasCompletedOnboarding", () => {
    useOnboardingStore.getState().completeOnboarding();
    expect(useOnboardingStore.getState().hasCompletedOnboarding).toBe(true);
  });

  it("skipOnboarding also sets hasCompletedOnboarding, from any step", () => {
    useOnboardingStore.getState().goToStep(1);
    useOnboardingStore.getState().skipOnboarding();
    expect(useOnboardingStore.getState().hasCompletedOnboarding).toBe(true);
  });

  it("restartOnboarding resets every field back to defaults", () => {
    useOnboardingStore.getState().goToStep(4);
    useOnboardingStore.getState().selectTemplate("release");
    useOnboardingStore.getState().completeOnboarding();

    useOnboardingStore.getState().restartOnboarding();

    const state = useOnboardingStore.getState();
    expect(state.hasCompletedOnboarding).toBe(false);
    expect(state.currentStep).toBe(0);
    expect(state.selectedTemplateId).toBeNull();
  });
});

describe("onboardingStore persistence", () => {
  it("persists step/template/completion changes across a hydrate() reload", async () => {
    useOnboardingStore.getState().goToStep(2);
    useOnboardingStore.getState().selectTemplate("homelab-admin");
    useOnboardingStore.getState().completeOnboarding();

    vi.resetModules();
    const fresh = await import("./onboardingStore");
    const state = fresh.useOnboardingStore.getState();
    expect(state.currentStep).toBe(2);
    expect(state.selectedTemplateId).toBe("homelab-admin");
    expect(state.hasCompletedOnboarding).toBe(true);
  });

  it("persists a restart back to defaults across a hydrate() reload", async () => {
    useOnboardingStore.getState().goToStep(5);
    useOnboardingStore.getState().completeOnboarding();
    useOnboardingStore.getState().restartOnboarding();

    vi.resetModules();
    const fresh = await import("./onboardingStore");
    const state = fresh.useOnboardingStore.getState();
    expect(state.currentStep).toBe(0);
    expect(state.hasCompletedOnboarding).toBe(false);
    expect(state.selectedTemplateId).toBeNull();
  });
});
