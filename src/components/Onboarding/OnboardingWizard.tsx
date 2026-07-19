import { useCallback, useEffect, useMemo, useState } from "react";
import { open } from "@tauri-apps/plugin-dialog";
import {
  Check,
  ChevronLeft,
  ChevronRight,
  FolderOpen,
  Lock,
  Pencil,
  Shield,
  ClipboardList,
  Sparkles as SparklesIcon,
  Zap,
} from "lucide-react";
import type { LucideIcon } from "lucide-react";

import { Button, StatusPill } from "../ui";
import { ModelManager } from "../Models";
import { OllamaPanel } from "../Ollama";
import { ProviderCard } from "../Settings/ProviderCard";
import { AddCustomProviderForm } from "../Settings/AddCustomProviderForm";
import { ONBOARDING_STEPS, useOnboardingStore } from "../../store/onboardingStore";
import { useModelStore } from "../../store/modelStore";
import { primaryRoot, useWorkspaceStore } from "../../store/workspaceStore";
import { usePermissionStore, type PermissionMode } from "../../store/permissionStore";
import {
  ONBOARDING_TEMPLATES,
  seedOnboardingTemplate,
  type OnboardingAssumptionKind,
  type OnboardingSeedResult,
} from "../../lib/onboardingTemplates";
import { useT } from "../../lib/i18n";

function formatError(err: unknown): string {
  if (err instanceof Error) return err.message;
  if (typeof err === "string") return err;
  try {
    return JSON.stringify(err);
  } catch {
    return String(err);
  }
}

/** Plain-language mode cards for the Safety defaults step — deliberately
 * excludes "bypass" (the one mode requiring its own confirm step in
 * `ModeSelector.tsx`) so a first-run choice is never a single click away
 * from disabling every permission prompt; bypass stays reachable later from
 * the normal mode selector in chat. Reuses the exact `ModeSelector.*`
 * translation keys already shipped for every locale rather than introducing
 * a second, slightly-different copy of the same descriptions. */
const SAFETY_MODE_OPTIONS: { mode: Exclude<PermissionMode, "bypass">; icon: LucideIcon; labelKey: string; descriptionKey: string }[] = [
  { mode: "manual", icon: Shield, labelKey: "ModeSelector.modeManualLabel", descriptionKey: "ModeSelector.modeManualDescription" },
  { mode: "acceptEdits", icon: Pencil, labelKey: "ModeSelector.modeAcceptEditsLabel", descriptionKey: "ModeSelector.modeAcceptEditsDescription" },
  { mode: "smart", icon: SparklesIcon, labelKey: "ModeSelector.modeSmartLabel", descriptionKey: "ModeSelector.modeSmartDescription" },
  { mode: "plan", icon: ClipboardList, labelKey: "ModeSelector.modePlanLabel", descriptionKey: "ModeSelector.modePlanDescription" },
  { mode: "auto", icon: Zap, labelKey: "ModeSelector.modeAutoLabel", descriptionKey: "ModeSelector.modeAutoDescription" },
];

const ASSUMPTION_META: Record<OnboardingAssumptionKind, { labelKey: string; descriptionKey: string }> = {
  model: { labelKey: "Onboarding.assumptionModelLabel", descriptionKey: "Onboarding.assumptionModelDescription" },
  tool: { labelKey: "Onboarding.assumptionToolLabel", descriptionKey: "Onboarding.assumptionToolDescription" },
  connector: { labelKey: "Onboarding.assumptionConnectorLabel", descriptionKey: "Onboarding.assumptionConnectorDescription" },
  permission: { labelKey: "Onboarding.assumptionPermissionLabel", descriptionKey: "Onboarding.assumptionPermissionDescription" },
  verification: { labelKey: "Onboarding.assumptionVerificationLabel", descriptionKey: "Onboarding.assumptionVerificationDescription" },
};

/** Step chrome shared by every step: title + body slot + Back/Skip/Next
 * footer. `nextLabel`/`onNext` let the last two steps rename/redirect the
 * primary action ("Finish" / "Enter Little Monkey") without a separate
 * layout. */
function StepShell({
  title,
  children,
  onBack,
  onNext,
  nextLabel,
  nextDisabled,
  showSkip,
}: {
  title: string;
  children: React.ReactNode;
  onBack: (() => void) | null;
  onNext: () => void;
  nextLabel: string;
  nextDisabled?: boolean;
  showSkip: boolean;
}) {
  const { t } = useT();
  const skipOnboarding = useOnboardingStore((s) => s.skipOnboarding);
  const currentStep = useOnboardingStore((s) => s.currentStep);

  return (
    <div className="flex h-full w-full flex-col">
      <div className="flex shrink-0 items-center justify-between px-8 pt-6">
        <div className="flex items-center gap-1.5">
          {ONBOARDING_STEPS.map((step, index) => (
            <span
              key={step}
              className={`h-1.5 w-8 rounded-full transition-colors ${
                index <= currentStep ? "bg-accent" : "bg-surface-2"
              }`}
            />
          ))}
        </div>
        {showSkip && (
          <button
            type="button"
            onClick={skipOnboarding}
            className="cursor-pointer text-xs text-faint hover:text-foreground"
          >
            {t("Onboarding.skipButton")}
          </button>
        )}
      </div>

      <div className="mx-auto flex min-h-0 w-full max-w-2xl flex-1 flex-col overflow-y-auto px-8 py-8 [overscroll-behavior:contain]">
        <h1 className="text-2xl font-semibold text-foreground">{title}</h1>
        <div className="mt-5 flex flex-1 flex-col gap-4">{children}</div>
      </div>

      <div className="flex shrink-0 items-center justify-between border-t border-border px-8 py-4">
        {onBack ? (
          <Button variant="ghost" size="sm" onClick={onBack}>
            <ChevronLeft size={14} />
            {t("Onboarding.backButton")}
          </Button>
        ) : (
          <span />
        )}
        <Button variant="primary" size="md" onClick={onNext} disabled={nextDisabled}>
          {nextLabel}
          <ChevronRight size={14} />
        </Button>
      </div>
    </div>
  );
}

function WelcomeStep() {
  const { t } = useT();
  return (
    <>
      <p className="text-sm leading-relaxed text-muted">{t("Onboarding.welcomeBody1")}</p>
      <p className="text-sm leading-relaxed text-muted">{t("Onboarding.welcomeBody2")}</p>
      <div className="mt-2 flex items-start gap-2 rounded-lg border border-border bg-background p-3">
        <Lock size={16} className="mt-0.5 shrink-0 text-accent" />
        <p className="text-xs text-muted">{t("Onboarding.welcomePrivacyNote")}</p>
      </div>
    </>
  );
}

function ModelStep() {
  const { t } = useT();
  const providers = useModelStore((s) => s.providers);
  const refreshProviders = useModelStore((s) => s.refreshProviders);

  useEffect(() => {
    void refreshProviders();
  }, [refreshProviders]);

  return (
    <>
      <p className="text-sm text-muted">{t("Onboarding.modelIntro")}</p>

      <section>
        <h3 className="mb-1 text-xs font-semibold uppercase tracking-wide text-faint">{t("Onboarding.modelLocalHeading")}</h3>
        <ModelManager />
      </section>

      <section>
        <h3 className="mb-1 text-xs font-semibold uppercase tracking-wide text-faint">{t("Onboarding.modelOllamaHeading")}</h3>
        <OllamaPanel />
      </section>

      <details className="group rounded-lg border border-border">
        <summary className="flex cursor-pointer list-none items-center gap-1.5 px-3 py-2 text-sm text-muted [&::-webkit-details-marker]:hidden">
          {t("Onboarding.modelCloudHeading")}
          <ChevronRight size={14} className="ml-auto transition-transform group-open:rotate-90" />
        </summary>
        <div className="flex flex-col gap-2 border-t border-border p-3">
          <p className="text-xs text-muted">{t("Onboarding.modelCloudIntro")}</p>
          {providers.map((provider) => (
            <ProviderCard key={provider.id} provider={provider} />
          ))}
          <AddCustomProviderForm />
        </div>
      </details>

      <p className="text-xs text-faint">{t("Onboarding.modelSkipHint")}</p>
    </>
  );
}

function WorkspaceStep() {
  const { t } = useT();
  const roots = useWorkspaceStore((s) => s.roots);
  const recent = useWorkspaceStore((s) => s.recent);
  const openPrimary = useWorkspaceStore((s) => s.openPrimary);
  const primary = primaryRoot(roots);
  const [error, setError] = useState<string | null>(null);
  const [opening, setOpening] = useState(false);

  const handleOpenFolder = useCallback(async () => {
    setError(null);
    try {
      const selected = await open({ directory: true, multiple: false });
      if (!selected || Array.isArray(selected)) return;
      setOpening(true);
      await openPrimary(selected);
    } catch (err) {
      setError(formatError(err));
    } finally {
      setOpening(false);
    }
  }, [openPrimary]);

  const handleSelectRecent = useCallback(
    async (path: string) => {
      setError(null);
      setOpening(true);
      try {
        await openPrimary(path);
      } catch (err) {
        setError(formatError(err));
      } finally {
        setOpening(false);
      }
    },
    [openPrimary],
  );

  return (
    <>
      <p className="text-sm text-muted">{t("Onboarding.workspaceIntro")}</p>

      {primary ? (
        <StatusPill tone="success">{t("Onboarding.workspaceCurrentLabel", { path: primary.path })}</StatusPill>
      ) : (
        <StatusPill tone="neutral">{t("Onboarding.workspaceNoneLabel")}</StatusPill>
      )}

      <Button variant="primary" size="md" onClick={() => void handleOpenFolder()} disabled={opening} className="self-start">
        <FolderOpen size={14} />
        {opening ? t("Onboarding.workspaceOpeningButton") : t("Onboarding.workspaceOpenFolderButton")}
      </Button>

      {error && <p className="text-xs text-danger">{error}</p>}

      {recent.length > 0 && (
        <div className="flex flex-col gap-1">
          <span className="text-xs font-semibold uppercase tracking-wide text-faint">{t("Onboarding.workspaceRecentHeading")}</span>
          {recent.slice(0, 5).map((entry) => (
            <button
              key={entry.path}
              type="button"
              onClick={() => void handleSelectRecent(entry.path)}
              disabled={opening}
              className="cursor-pointer truncate rounded-md border border-border bg-background px-3 py-2 text-left text-xs text-muted hover:bg-surface-2 hover:text-foreground disabled:cursor-not-allowed"
            >
              {entry.label}
              <span className="ml-2 text-faint">{entry.path}</span>
            </button>
          ))}
        </div>
      )}

      <p className="text-xs text-faint">{t("Onboarding.workspaceSkipHint")}</p>
    </>
  );
}

function SafetyStep() {
  const { t } = useT();
  const mode = usePermissionStore((s) => s.mode);
  const setMode = usePermissionStore((s) => s.setMode);
  const setLastActMode = usePermissionStore((s) => s.setLastActMode);
  const [error, setError] = useState<string | null>(null);

  const handleSelect = useCallback(
    async (nextMode: Exclude<PermissionMode, "bypass">) => {
      setError(null);
      try {
        if (nextMode !== "plan") setLastActMode(nextMode);
        await setMode(nextMode);
      } catch (err) {
        setError(formatError(err));
      }
    },
    [setMode, setLastActMode],
  );

  return (
    <>
      <p className="text-sm text-muted">{t("Onboarding.safetyIntro")}</p>
      <div className="flex flex-col gap-2">
        {SAFETY_MODE_OPTIONS.map((option) => {
          const Icon = option.icon;
          const isActive = mode === option.mode;
          return (
            <button
              key={option.mode}
              type="button"
              onClick={() => void handleSelect(option.mode)}
              className={`flex cursor-pointer items-start gap-3 rounded-lg border p-3 text-left transition-colors ${
                isActive ? "border-accent bg-accent-soft" : "border-border bg-background hover:bg-surface-2"
              }`}
            >
              <Icon size={16} className="mt-0.5 shrink-0 text-faint" />
              <span className="min-w-0 flex-1">
                <span className="flex items-center gap-2">
                  <span className="text-sm font-medium text-foreground">{t(option.labelKey)}</span>
                  {isActive && <Check size={13} className="shrink-0 text-accent" />}
                </span>
                <span className="mt-0.5 block text-xs text-muted">{t(option.descriptionKey)}</span>
              </span>
            </button>
          );
        })}
      </div>
      {error && <p className="text-xs text-danger">{error}</p>}
      <p className="text-xs text-faint">{t("Onboarding.safetyAdvancedNote")}</p>
    </>
  );
}

function TemplateStep() {
  const { t } = useT();
  const selectedTemplateId = useOnboardingStore((s) => s.selectedTemplateId);
  const selectTemplate = useOnboardingStore((s) => s.selectTemplate);
  const [seeding, setSeeding] = useState<string | null>(null);
  const [results, setResults] = useState<Record<string, OnboardingSeedResult>>({});

  const handlePick = useCallback(
    async (id: string) => {
      selectTemplate(id);
      setSeeding(id);
      try {
        const result = await seedOnboardingTemplate(id);
        setResults((prev) => ({ ...prev, [id]: result }));
      } finally {
        setSeeding(null);
      }
    },
    [selectTemplate],
  );

  return (
    <>
      <p className="text-sm text-muted">{t("Onboarding.templateIntro")}</p>
      <div className="grid grid-cols-1 gap-2.5 sm:grid-cols-2">
        {ONBOARDING_TEMPLATES.map((template) => {
          const isSelected = selectedTemplateId === template.id;
          const result = results[template.id];
          return (
            <button
              key={template.id}
              type="button"
              onClick={() => void handlePick(template.id)}
              disabled={seeding !== null}
              className={`flex flex-col gap-1.5 rounded-lg border p-3 text-left transition-colors disabled:cursor-not-allowed ${
                isSelected ? "border-accent bg-accent-soft" : "border-border bg-background hover:bg-surface-2"
              }`}
            >
              <span className="flex items-center gap-2">
                <span className="text-sm font-medium text-foreground">{t(template.nameKey)}</span>
                {isSelected && <Check size={13} className="shrink-0 text-accent" />}
              </span>
              <span className="text-xs text-muted">{t(template.descriptionKey)}</span>
              <span className="flex flex-wrap gap-1 pt-0.5">
                {template.assumptions.map((kind) => (
                  <span
                    key={kind}
                    title={t(ASSUMPTION_META[kind].descriptionKey)}
                    className="rounded-full bg-surface-2 px-1.5 py-0.5 text-[10px] font-medium uppercase tracking-wide text-faint"
                  >
                    {t(ASSUMPTION_META[kind].labelKey)}
                  </span>
                ))}
              </span>
              {template.caveatKey && <span className="text-[11px] italic text-faint">{t(template.caveatKey)}</span>}
              {seeding === template.id && <span className="text-[11px] text-faint">{t("Onboarding.templateSeeding")}</span>}
              {result && (
                <span className="text-[11px] text-success">
                  {result.personaCreated && t("Onboarding.templateSeededPersona")}
                  {result.recipeCreated && ` ${t("Onboarding.templateSeededRecipe")}`}
                  {result.automationCreated && ` ${t("Onboarding.templateSeededAutomation")}`}
                </span>
              )}
              {result?.error && <span className="text-[11px] text-danger">{result.error}</span>}
            </button>
          );
        })}
      </div>
    </>
  );
}

function FinishStep() {
  const { t } = useT();
  const selectedTemplateId = useOnboardingStore((s) => s.selectedTemplateId);
  const template = ONBOARDING_TEMPLATES.find((candidate) => candidate.id === selectedTemplateId);
  return (
    <>
      <p className="text-sm leading-relaxed text-muted">{t("Onboarding.finishBody")}</p>
      {template && (
        <p className="text-sm text-muted">
          {t("Onboarding.finishTemplateSummary", { template: t(template.nameKey) })}
        </p>
      )}
      <p className="text-xs text-faint">{t("Onboarding.finishReopenHint")}</p>
    </>
  );
}

/**
 * Full-screen first-run wizard (ROADMAP.md Phase 6, "First-Run Onboarding
 * and Use-Case Templates"). Renders instead of the normal app shell — see
 * `App.tsx` — while `onboardingStore.hasCompletedOnboarding` is false and a
 * Tauri environment is detected. Every step reuses an existing store/UI
 * surface rather than re-implementing one: model detection reuses
 * `modelStore` + `ModelManager`/`OllamaPanel`/`ProviderCard`, the workspace
 * step reuses `workspaceStore.openPrimary` via the same folder-picker flow
 * `WorkspaceBar.tsx` uses, the safety step reuses `permissionStore`'s mode
 * switch, and the template picker seeds only through
 * `promptStore`/`recipeStore`/`automationsStore` (see
 * `lib/onboardingTemplates.ts`).
 */
export function OnboardingWizard() {
  const { t } = useT();
  const currentStep = useOnboardingStore((s) => s.currentStep);
  const nextStep = useOnboardingStore((s) => s.nextStep);
  const previousStep = useOnboardingStore((s) => s.previousStep);
  const completeOnboarding = useOnboardingStore((s) => s.completeOnboarding);

  const stepId = ONBOARDING_STEPS[currentStep];
  const isFirst = currentStep === 0;
  const isLast = currentStep === ONBOARDING_STEPS.length - 1;

  const title = useMemo(() => {
    switch (stepId) {
      case "welcome":
        return t("Onboarding.welcomeTitle");
      case "model":
        return t("Onboarding.modelTitle");
      case "workspace":
        return t("Onboarding.workspaceTitle");
      case "safety":
        return t("Onboarding.safetyTitle");
      case "template":
        return t("Onboarding.templateTitle");
      case "finish":
      default:
        return t("Onboarding.finishTitle");
    }
  }, [stepId, t]);

  const handleNext = useCallback(() => {
    if (isLast) {
      completeOnboarding();
      return;
    }
    nextStep();
  }, [isLast, completeOnboarding, nextStep]);

  return (
    <div className="fixed inset-0 z-50 flex bg-background text-foreground">
      <StepShell
        title={title}
        onBack={isFirst ? null : previousStep}
        onNext={handleNext}
        nextLabel={isLast ? t("Onboarding.finishButton") : stepId === "welcome" ? t("Onboarding.getStartedButton") : t("Onboarding.nextButton")}
        showSkip={!isLast}
      >
        {stepId === "welcome" && <WelcomeStep />}
        {stepId === "model" && <ModelStep />}
        {stepId === "workspace" && <WorkspaceStep />}
        {stepId === "safety" && <SafetyStep />}
        {stepId === "template" && <TemplateStep />}
        {stepId === "finish" && <FinishStep />}
      </StepShell>
    </div>
  );
}

export default OnboardingWizard;
