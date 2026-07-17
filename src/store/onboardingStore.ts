import { create } from "zustand";

/** localStorage key the onboarding blob is persisted under — dedicated (not
 * folded into `settingsStore`'s own key) so wiping app settings never
 * silently re-triggers the first-run wizard, and vice versa. Mirrors
 * `settingsStore.ts`'s `STORAGE_KEY` persistence approach exactly: hydrate
 * once at module load, best-effort `persist()` after every mutation. */
export const ONBOARDING_STORAGE_KEY = "little-monkey-onboarding";

/** The wizard's fixed step sequence (ROADMAP.md's "First-Run Onboarding and
 * Use-Case Templates" item) — indices into this array are what `currentStep`
 * actually stores, so reordering steps only ever means editing this one
 * list. */
export const ONBOARDING_STEPS = [
  "welcome",
  "model",
  "workspace",
  "safety",
  "template",
  "finish",
] as const;

export type OnboardingStepId = (typeof ONBOARDING_STEPS)[number];

const LAST_STEP_INDEX = ONBOARDING_STEPS.length - 1;

interface PersistedShape {
  /** Whether the wizard has been finished or explicitly skipped — the single
   * flag `App.tsx` checks to decide whether to render the wizard instead of
   * the normal shell. Once true, the wizard never auto-shows again; the only
   * way back in is `restartOnboarding()` (see `AppMenu`'s "Restart
   * onboarding" row). */
  hasCompletedOnboarding: boolean;
  /** Index into `ONBOARDING_STEPS` the wizard is currently showing. Persisted
   * so quitting mid-wizard (e.g. to go download a model) resumes on the same
   * step next launch, rather than restarting from Welcome. */
  currentStep: number;
  /** The use-case template id chosen on the "template" step, or `null` if
   * none has been picked yet. Kept even after `hasCompletedOnboarding`
   * becomes true so `restartOnboarding()`'s "start over" flow and any later
   * "what did I pick" affordance both have something to read. */
  selectedTemplateId: string | null;
}

function defaults(): PersistedShape {
  return {
    hasCompletedOnboarding: false,
    currentStep: 0,
    selectedTemplateId: null,
  };
}

/** Loads the persisted onboarding blob, falling back to defaults for
 * anything absent, corrupt, or malformed — same defensive shape as
 * `settingsStore.ts`'s `hydrate()`. */
function hydrate(): PersistedShape {
  const fallback = defaults();
  try {
    const raw = localStorage.getItem(ONBOARDING_STORAGE_KEY);
    if (!raw) return fallback;
    const parsed = JSON.parse(raw) as Partial<PersistedShape> | null;
    if (!parsed || typeof parsed !== "object") return fallback;
    return {
      hasCompletedOnboarding:
        typeof parsed.hasCompletedOnboarding === "boolean" ? parsed.hasCompletedOnboarding : fallback.hasCompletedOnboarding,
      currentStep:
        typeof parsed.currentStep === "number" && Number.isFinite(parsed.currentStep) && parsed.currentStep >= 0 && parsed.currentStep <= LAST_STEP_INDEX
          ? Math.round(parsed.currentStep)
          : fallback.currentStep,
      selectedTemplateId: typeof parsed.selectedTemplateId === "string" ? parsed.selectedTemplateId : fallback.selectedTemplateId,
    };
  } catch {
    return fallback;
  }
}

/** Best-effort persist — a quota error or serialization issue must never
 * throw into the caller, mirroring `settingsStore.ts`'s `persist()`. */
function persist(state: PersistedShape): void {
  try {
    localStorage.setItem(ONBOARDING_STORAGE_KEY, JSON.stringify(state));
  } catch {
    // Ignore — persistence is best-effort.
  }
}

function clampStep(step: number): number {
  return Math.min(LAST_STEP_INDEX, Math.max(0, Math.round(step)));
}

export interface OnboardingState extends PersistedShape {
  /** Jump straight to a step (e.g. a "back to Safety" link) — clamped into
   * range rather than throwing on an out-of-bounds index. */
  goToStep: (step: number) => void;
  /** Advance one step; a no-op past the last step. */
  nextStep: () => void;
  /** Go back one step; a no-op before the first step. */
  previousStep: () => void;
  /** Records (or clears, with `null`) the chosen use-case template. Does not
   * itself seed anything — seeding is the caller's job (see
   * `lib/onboardingTemplates.ts`'s `seedOnboardingTemplate`), so selecting a
   * template is always cheap/instant and re-selecting never re-seeds twice
   * from inside the store. */
  selectTemplate: (id: string | null) => void;
  /** Marks the wizard finished via its own "Finish" step. */
  completeOnboarding: () => void;
  /** Marks the wizard finished via an explicit "Skip" action, at any step.
   * Behaviorally identical to `completeOnboarding` (both just set the one
   * flag `App.tsx` checks) — kept as a separate action so call sites read as
   * "the user explicitly opted out" versus "the user finished it", and so a
   * future analytics/telemetry hook (there isn't one today) has a
   * distinguishable place to attach without touching every call site. */
  skipOnboarding: () => void;
  /** Resets every persisted field back to defaults, re-showing the wizard
   * from Welcome — the "Restart onboarding" AppMenu row's action. */
  restartOnboarding: () => void;
}

const initial = hydrate();

export const useOnboardingStore = create<OnboardingState>((set, get) => ({
  ...initial,

  goToStep: (step) => {
    set({ currentStep: clampStep(step) });
    persist({ ...get() });
  },

  nextStep: () => {
    set((state) => ({ currentStep: clampStep(state.currentStep + 1) }));
    persist({ ...get() });
  },

  previousStep: () => {
    set((state) => ({ currentStep: clampStep(state.currentStep - 1) }));
    persist({ ...get() });
  },

  selectTemplate: (id) => {
    set({ selectedTemplateId: id });
    persist({ ...get() });
  },

  completeOnboarding: () => {
    set({ hasCompletedOnboarding: true });
    persist({ ...get() });
  },

  skipOnboarding: () => {
    set({ hasCompletedOnboarding: true });
    persist({ ...get() });
  },

  restartOnboarding: () => {
    const reset = defaults();
    set(reset);
    persist(reset);
  },
}));

export default useOnboardingStore;
