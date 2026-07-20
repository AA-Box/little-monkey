import {
  useEffect,
  useMemo,
  useRef,
  useState,
  type ChangeEvent,
  type ReactNode,
} from "react";
import {
  Accessibility,
  Contrast,
  Download,
  Eye,
  Gauge,
  Keyboard,
  LayoutPanelLeft,
  Moon,
  Monitor,
  Palette,
  Sun,
  Type,
  Upload,
  ZapOff,
  type LucideIcon,
} from "lucide-react";
import { useT } from "../../lib/i18n";
import {
  applyAppearance,
  subscribeToSystemTheme,
  useResolvedTheme,
  type AccentColor,
  type AppearanceSettings,
  type ChatBubbleStyle,
  type CodeFontSize,
  type FocusVisibility,
  type MotionPreference,
  type SidebarLayout,
  type TextScale,
  type ThemePreference,
  type UiDensity,
} from "../../lib/theme";
import {
  appearanceSettingsEqual,
  applyAccessibleAppearancePreset,
  createAppearanceWorkspaceOverride,
  exportAppearanceProfile,
  importAppearanceProfile,
  resolveAppearanceSettings,
  type AccessibleAppearancePreset,
} from "../../lib/appearanceProfiles";
import { primaryRoot, useWorkspaceStore } from "../../store/workspaceStore";
import { useSettingsStore } from "../../store/settingsStore";
import { Button } from "../ui";

type AppearanceScope = "device" | "workspace";

function classNames(...classes: Array<string | false | null | undefined>): string {
  return classes.filter(Boolean).join(" ");
}

function Toggle({
  checked,
  onChange,
  label,
}: {
  checked: boolean;
  onChange: (value: boolean) => void;
  label: string;
}) {
  return (
    <button
      type="button"
      role="switch"
      aria-checked={checked}
      aria-label={label}
      onClick={() => onChange(!checked)}
      className={classNames(
        "relative h-7 w-12 shrink-0 cursor-pointer rounded-full transition-colors focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-accent",
        checked ? "bg-accent" : "border border-border bg-surface-2",
      )}
    >
      <span
        className={classNames(
          "absolute top-1 h-5 w-5 rounded-full bg-white shadow-sm transition-[left]",
          checked ? "left-6" : "left-1",
        )}
      />
    </button>
  );
}

function Section({
  icon,
  title,
  description,
  children,
}: {
  icon: ReactNode;
  title: string;
  description?: string;
  children: ReactNode;
}) {
  return (
    <section className="rounded-lg border border-border bg-surface p-4">
      <div className="flex items-start gap-3">
        <span className="mt-0.5 flex h-8 w-8 shrink-0 items-center justify-center rounded-md bg-surface-2 text-muted">
          {icon}
        </span>
        <div className="min-w-0 flex-1">
          <h3 className="text-sm font-semibold text-foreground">{title}</h3>
          {description && <p className="mt-1 text-xs leading-5 text-muted">{description}</p>}
          <div className="mt-3">{children}</div>
        </div>
      </div>
    </section>
  );
}

function OptionButton<T extends string | number>({
  selected,
  value,
  label,
  description,
  icon: Icon,
  onSelect,
  disabled = false,
}: {
  selected: boolean;
  value: T;
  label: string;
  description?: string;
  icon?: LucideIcon;
  onSelect: (value: T) => void;
  disabled?: boolean;
}) {
  return (
    <button
      type="button"
      role="radio"
      aria-checked={selected}
      disabled={disabled}
      onClick={() => onSelect(value)}
      className={classNames(
        "min-h-16 cursor-pointer rounded-lg border px-3 py-2 text-left transition-colors focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-accent disabled:cursor-not-allowed disabled:opacity-50",
        selected
          ? "border-accent bg-accent-soft text-foreground"
          : "border-border bg-background text-muted hover:border-border-strong hover:bg-surface-2 hover:text-foreground",
      )}
    >
      <span className="flex items-center gap-2 text-sm font-medium">
        {Icon && <Icon size={15} className="shrink-0" />}
        {label}
      </span>
      {description && <span className="mt-1 block text-xs leading-4 text-muted">{description}</span>}
    </button>
  );
}

function SelectField<T extends string | number>({
  id,
  label,
  value,
  options,
  onChange,
}: {
  id: string;
  label: string;
  value: T;
  options: ReadonlyArray<{ value: T; label: string }>;
  onChange: (value: T) => void;
}) {
  return (
    <label htmlFor={id} className="block min-w-0">
      <span className="mb-1.5 block text-xs font-medium text-muted">{label}</span>
      <select
        id={id}
        value={String(value)}
        onChange={(event) => {
          const option = options.find((candidate) => String(candidate.value) === event.target.value);
          if (option) onChange(option.value);
        }}
        className="h-10 w-full cursor-pointer rounded-md border border-border bg-background px-3 text-sm text-foreground focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-accent"
      >
        {options.map((option) => (
          <option key={String(option.value)} value={String(option.value)}>{option.label}</option>
        ))}
      </select>
    </label>
  );
}

const THEME_OPTIONS: Array<{ value: ThemePreference; labelKey: string; descriptionKey: string; icon: LucideIcon }> = [
  { value: "system", labelKey: "AppearancePanel.themeSystem", descriptionKey: "AppearancePanel.themeSystemDescription", icon: Monitor },
  { value: "light", labelKey: "AppearancePanel.themeLight", descriptionKey: "AppearancePanel.themeLightDescription", icon: Sun },
  { value: "dark", labelKey: "AppearancePanel.themeDark", descriptionKey: "AppearancePanel.themeDarkDescription", icon: Moon },
];

const ACCENT_OPTIONS: Array<{ value: AccentColor; labelKey: string; swatch: string }> = [
  { value: "default", labelKey: "AppearancePanel.accentDefault", swatch: "var(--c-accent)" },
  { value: "indigo", labelKey: "AppearancePanel.accentIndigo", swatch: "#4f46e5" },
  { value: "blue", labelKey: "AppearancePanel.accentBlue", swatch: "#2563eb" },
  { value: "teal", labelKey: "AppearancePanel.accentTeal", swatch: "#0f766e" },
  { value: "rose", labelKey: "AppearancePanel.accentRose", swatch: "#e11d48" },
  { value: "amber", labelKey: "AppearancePanel.accentAmber", swatch: "#d97706" },
];

const TEXT_SCALE_OPTIONS: Array<{ value: TextScale; labelKey: string; descriptionKey: string }> = [
  { value: "small", labelKey: "AppearancePanel.textSmall", descriptionKey: "AppearancePanel.textSmallDescription" },
  { value: "medium", labelKey: "AppearancePanel.textMedium", descriptionKey: "AppearancePanel.textMediumDescription" },
  { value: "large", labelKey: "AppearancePanel.textLarge", descriptionKey: "AppearancePanel.textLargeDescription" },
];

const CODE_FONT_OPTIONS: Array<{ value: CodeFontSize; labelKey: string }> = [
  { value: 12, labelKey: "AppearancePanel.codeFontSmall" },
  { value: 14, labelKey: "AppearancePanel.codeFontMedium" },
  { value: 16, labelKey: "AppearancePanel.codeFontLarge" },
];

const DENSITY_OPTIONS: Array<{ value: UiDensity; labelKey: string }> = [
  { value: "compact", labelKey: "AppearancePanel.densityCompact" },
  { value: "comfortable", labelKey: "AppearancePanel.densityComfortable" },
  { value: "spacious", labelKey: "AppearancePanel.densitySpacious" },
];

const SIDEBAR_OPTIONS: Array<{ value: SidebarLayout; labelKey: string }> = [
  { value: "compact", labelKey: "AppearancePanel.sidebarCompact" },
  { value: "standard", labelKey: "AppearancePanel.sidebarStandard" },
  { value: "wide", labelKey: "AppearancePanel.sidebarWide" },
];

const BUBBLE_OPTIONS: Array<{ value: ChatBubbleStyle; labelKey: string }> = [
  { value: "bubbles", labelKey: "AppearancePanel.bubblesRounded" },
  { value: "flat", labelKey: "AppearancePanel.bubblesFlat" },
  { value: "compact", labelKey: "AppearancePanel.bubblesCompact" },
];

const MOTION_OPTIONS: Array<{ value: MotionPreference; labelKey: string; descriptionKey: string }> = [
  { value: "system", labelKey: "AppearancePanel.motionSystem", descriptionKey: "AppearancePanel.motionSystemDescription" },
  { value: "reduced", labelKey: "AppearancePanel.motionReduced", descriptionKey: "AppearancePanel.motionReducedDescription" },
];

const FOCUS_OPTIONS: Array<{ value: FocusVisibility; labelKey: string }> = [
  { value: "standard", labelKey: "AppearancePanel.focusStandard" },
  { value: "enhanced", labelKey: "AppearancePanel.focusEnhanced" },
];

const PRESET_OPTIONS: Array<{ value: AccessibleAppearancePreset; labelKey: string; descriptionKey: string; icon: LucideIcon }> = [
  { value: "low-vision", labelKey: "AppearancePanel.presetLowVision", descriptionKey: "AppearancePanel.presetLowVisionDescription", icon: Eye },
  { value: "keyboard", labelKey: "AppearancePanel.presetKeyboard", descriptionKey: "AppearancePanel.presetKeyboardDescription", icon: Keyboard },
  { value: "reduced-motion", labelKey: "AppearancePanel.presetReducedMotion", descriptionKey: "AppearancePanel.presetReducedMotionDescription", icon: ZapOff },
];

function hasOwnWorkspaceOverride(overrides: Record<string, unknown>, workspaceKey: string | null): boolean {
  return Boolean(workspaceKey && Object.prototype.hasOwnProperty.call(overrides, workspaceKey));
}

export function AppearancePanel() {
  const { t } = useT();
  const deviceAppearance = useSettingsStore((state) => state.deviceAppearance);
  const workspaceOverrides = useSettingsStore((state) => state.appearanceWorkspaceOverrides);
  const setDeviceAppearance = useSettingsStore((state) => state.setDeviceAppearance);
  const setWorkspaceAppearanceOverride = useSettingsStore((state) => state.setWorkspaceAppearanceOverride);
  const clearWorkspaceAppearanceOverride = useSettingsStore((state) => state.clearWorkspaceAppearanceOverride);
  const workspaceRoot = useWorkspaceStore((state) => primaryRoot(state.roots));
  const workspaceKey = workspaceRoot?.path ?? null;
  const hasWorkspaceOverride = hasOwnWorkspaceOverride(workspaceOverrides, workspaceKey);
  const committedEffective = useMemo(
    () => resolveAppearanceSettings(deviceAppearance, workspaceOverrides, workspaceKey),
    [deviceAppearance, workspaceKey, workspaceOverrides],
  );
  const initialScope: AppearanceScope = workspaceKey && hasWorkspaceOverride ? "workspace" : "device";
  const [scope, setScope] = useState<AppearanceScope>(initialScope);
  const [workspaceOverrideEnabled, setWorkspaceOverrideEnabled] = useState(hasWorkspaceOverride);
  const [draft, setDraft] = useState<AppearanceSettings>(
    initialScope === "workspace" ? committedEffective : deviceAppearance,
  );
  const [feedback, setFeedback] = useState<{ kind: "success" | "error"; text: string } | null>(null);
  const importInputRef = useRef<HTMLInputElement>(null);
  const committedEffectiveRef = useRef(committedEffective);
  const draftRef = useRef(draft);
  const skipNextDraftPreviewRef = useRef(false);
  committedEffectiveRef.current = committedEffective;
  draftRef.current = draft;

  useEffect(() => {
    const nextScope: AppearanceScope = workspaceKey && hasWorkspaceOverride ? "workspace" : "device";
    setScope(nextScope);
    setWorkspaceOverrideEnabled(hasWorkspaceOverride);
    setDraft(nextScope === "workspace" ? committedEffective : deviceAppearance);
    setFeedback(null);
  // Reset only when the primary workspace identity changes. Store commits are
  // already reflected by the local draft used to create them.
  // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [workspaceKey]);

  useEffect(() => {
    if (skipNextDraftPreviewRef.current) {
      skipNextDraftPreviewRef.current = false;
      return;
    }
    applyAppearance(draft, { persistThemePreference: false });
  }, [draft]);

  useEffect(() => () => {
    applyAppearance(committedEffectiveRef.current);
  }, []);

  useEffect(() => subscribeToSystemTheme(() => {
    // The app-level listener reapplies committed settings when the OS theme
    // changes. While this panel owns a preview lease, its draft stays on top.
    applyAppearance(draftRef.current, { persistThemePreference: false });
  }), []);

  const resolvedTheme = useResolvedTheme(draft.themePreference);
  const committedTarget = scope === "workspace" ? committedEffective : deviceAppearance;
  const valuesChanged = !appearanceSettingsEqual(draft, committedTarget);
  const dirty = scope === "workspace"
    ? workspaceOverrideEnabled !== hasWorkspaceOverride || (workspaceOverrideEnabled && valuesChanged)
    : valuesChanged;

  function updateDraft(patch: Partial<AppearanceSettings>): void {
    if (scope === "workspace") setWorkspaceOverrideEnabled(true);
    setDraft((current) => ({ ...current, ...patch }));
    setFeedback(null);
  }

  function selectScope(nextScope: AppearanceScope): void {
    if (nextScope === "workspace" && !workspaceKey) return;
    setScope(nextScope);
    setFeedback(null);
    if (nextScope === "device") {
      setDraft(deviceAppearance);
      return;
    }
    setWorkspaceOverrideEnabled(hasWorkspaceOverride);
    setDraft(committedEffective);
  }

  function toggleWorkspaceOverride(enabled: boolean): void {
    setWorkspaceOverrideEnabled(enabled);
    setDraft(enabled && hasWorkspaceOverride ? committedEffective : deviceAppearance);
    setFeedback(null);
  }

  function applyDraft(): void {
    if (scope === "workspace" && workspaceKey) {
      if (workspaceOverrideEnabled) {
        committedEffectiveRef.current = draft;
        setWorkspaceAppearanceOverride(
          workspaceKey,
          createAppearanceWorkspaceOverride(deviceAppearance, draft),
        );
      } else {
        committedEffectiveRef.current = deviceAppearance;
        clearWorkspaceAppearanceOverride(workspaceKey);
      }
    } else {
      committedEffectiveRef.current = resolveAppearanceSettings(
        draft,
        workspaceOverrides,
        workspaceKey,
      );
      setDeviceAppearance(draft);
    }
    setFeedback({ kind: "success", text: t("AppearancePanel.savedStatus") });
  }

  function cancelDraft(): void {
    const resetDraft = scope === "workspace" ? committedEffective : deviceAppearance;
    if (!appearanceSettingsEqual(draft, resetDraft)) skipNextDraftPreviewRef.current = true;
    setDraft(resetDraft);
    setWorkspaceOverrideEnabled(hasWorkspaceOverride);
    applyAppearance(committedEffective, { persistThemePreference: false });
    setFeedback({ kind: "success", text: t("AppearancePanel.cancelledStatus") });
  }

  function exportDraft(): void {
    try {
      const serialized = exportAppearanceProfile(draft, "Little Monkey appearance");
      const url = URL.createObjectURL(new Blob([serialized], { type: "application/json" }));
      const anchor = document.createElement("a");
      anchor.href = url;
      anchor.download = "little-monkey-appearance.json";
      anchor.click();
      URL.revokeObjectURL(url);
      setFeedback({ kind: "success", text: t("AppearancePanel.exportedStatus") });
    } catch (error) {
      setFeedback({ kind: "error", text: t("AppearancePanel.profileError", { error: String(error) }) });
    }
  }

  async function importDraft(event: ChangeEvent<HTMLInputElement>): Promise<void> {
    const file = event.target.files?.[0];
    event.target.value = "";
    if (!file) return;
    try {
      const profile = importAppearanceProfile(await file.text());
      if (scope === "workspace") setWorkspaceOverrideEnabled(true);
      setDraft(profile.appearance);
      setFeedback({ kind: "success", text: t("AppearancePanel.importedStatus") });
    } catch (error) {
      setFeedback({ kind: "error", text: t("AppearancePanel.profileError", { error: String(error) }) });
    }
  }

  const codeFontOptions = CODE_FONT_OPTIONS.map((option) => ({ value: option.value, label: t(option.labelKey) }));
  const densityOptions = DENSITY_OPTIONS.map((option) => ({ value: option.value, label: t(option.labelKey) }));
  const sidebarOptions = SIDEBAR_OPTIONS.map((option) => ({ value: option.value, label: t(option.labelKey) }));
  const bubbleOptions = BUBBLE_OPTIONS.map((option) => ({ value: option.value, label: t(option.labelKey) }));
  const focusOptions = FOCUS_OPTIONS.map((option) => ({ value: option.value, label: t(option.labelKey) }));
  const previewGap = draft.uiDensity === "compact" ? "0.5rem" : draft.uiDensity === "spacious" ? "1rem" : "0.75rem";
  const previewBubbleClass = draft.chatBubbleStyle === "flat"
    ? "rounded-sm border-l-2 border-accent bg-background"
    : draft.chatBubbleStyle === "compact"
      ? "rounded-md border border-border bg-surface-2"
      : "rounded-xl border border-border bg-surface-2";

  return (
    <div className="grid gap-5 xl:grid-cols-[minmax(0,1fr)_19rem]">
      <div className="flex min-w-0 flex-col gap-4">
        <Section
          icon={<Monitor size={17} />}
          title={t("AppearancePanel.scopeTitle")}
          description={t("AppearancePanel.scopeDescription")}
        >
          <div className="grid gap-2 sm:grid-cols-2" role="radiogroup" aria-label={t("AppearancePanel.scopeTitle")}>
            <OptionButton
              value="device"
              selected={scope === "device"}
              label={t("AppearancePanel.scopeDevice")}
              description={t("AppearancePanel.scopeDeviceDescription")}
              onSelect={selectScope}
            />
            <OptionButton
              value="workspace"
              selected={scope === "workspace"}
              label={t("AppearancePanel.scopeWorkspace")}
              description={workspaceRoot
                ? t("AppearancePanel.scopeWorkspaceDescription", { workspace: workspaceRoot.label })
                : t("AppearancePanel.scopeWorkspaceUnavailable")}
              onSelect={selectScope}
              disabled={!workspaceRoot}
            />
          </div>
          {scope === "workspace" && workspaceRoot && (
            <div className="mt-3 flex items-center justify-between gap-4 rounded-lg border border-border bg-background px-3 py-3">
              <div className="min-w-0">
                <p className="text-sm font-medium text-foreground">{t("AppearancePanel.workspaceOverrideLabel")}</p>
                <p className="mt-1 text-xs leading-5 text-muted">{t("AppearancePanel.workspaceOverrideDescription")}</p>
              </div>
              <Toggle
                checked={workspaceOverrideEnabled}
                onChange={toggleWorkspaceOverride}
                label={t("AppearancePanel.workspaceOverrideLabel")}
              />
            </div>
          )}
        </Section>

        <Section
          icon={<Monitor size={17} />}
          title={t("AppearancePanel.themeTitle")}
          description={t("AppearancePanel.themeDescription")}
        >
          <div className="grid gap-2 sm:grid-cols-3" role="radiogroup" aria-label={t("AppearancePanel.themeTitle")}>
            {THEME_OPTIONS.map((option) => (
              <OptionButton
                key={option.value}
                value={option.value}
                selected={draft.themePreference === option.value}
                label={t(option.labelKey)}
                description={t(option.descriptionKey)}
                icon={option.icon}
                onSelect={(value) => updateDraft({ themePreference: value })}
              />
            ))}
          </div>
        </Section>

        <Section
          icon={<Palette size={17} />}
          title={t("AppearancePanel.accentTitle")}
          description={t("AppearancePanel.accentDescription")}
        >
          <div className="grid grid-cols-2 gap-2 sm:grid-cols-3" role="radiogroup" aria-label={t("AppearancePanel.accentTitle")}>
            {ACCENT_OPTIONS.map((option) => {
              const selected = draft.accentColor === option.value;
              return (
                <button
                  key={option.value}
                  type="button"
                  role="radio"
                  aria-checked={selected}
                  onClick={() => updateDraft({ accentColor: option.value })}
                  className={classNames(
                    "flex min-h-11 cursor-pointer items-center gap-2 rounded-lg border px-3 py-2 text-left text-sm transition-colors focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-accent",
                    selected
                      ? "border-accent bg-accent-soft font-medium text-foreground"
                      : "border-border bg-background text-muted hover:border-border-strong hover:bg-surface-2 hover:text-foreground",
                  )}
                >
                  <span
                    className="h-5 w-5 shrink-0 rounded-full border border-black/10"
                    style={{ background: option.swatch }}
                    aria-hidden="true"
                  />
                  <span className="truncate">{t(option.labelKey)}</span>
                </button>
              );
            })}
          </div>
        </Section>

        <Section
          icon={<Type size={17} />}
          title={t("AppearancePanel.typographyTitle")}
          description={t("AppearancePanel.typographyDescription")}
        >
          <div className="grid gap-2 sm:grid-cols-3" role="radiogroup" aria-label={t("AppearancePanel.textTitle")}>
            {TEXT_SCALE_OPTIONS.map((option) => (
              <OptionButton
                key={option.value}
                value={option.value}
                selected={draft.textScale === option.value}
                label={t(option.labelKey)}
                description={t(option.descriptionKey)}
                onSelect={(value) => updateDraft({ textScale: value })}
              />
            ))}
          </div>
          <div className="mt-3 max-w-xs">
            <SelectField
              id="appearance-code-font-size"
              label={t("AppearancePanel.codeFontLabel")}
              value={draft.codeFontSize}
              options={codeFontOptions}
              onChange={(value) => updateDraft({ codeFontSize: value })}
            />
          </div>
        </Section>

        <Section
          icon={<LayoutPanelLeft size={17} />}
          title={t("AppearancePanel.layoutTitle")}
          description={t("AppearancePanel.layoutDescription")}
        >
          <div className="grid gap-3 sm:grid-cols-3">
            <SelectField
              id="appearance-density"
              label={t("AppearancePanel.densityLabel")}
              value={draft.uiDensity}
              options={densityOptions}
              onChange={(value) => updateDraft({ uiDensity: value })}
            />
            <SelectField
              id="appearance-sidebar"
              label={t("AppearancePanel.sidebarLabel")}
              value={draft.sidebarLayout}
              options={sidebarOptions}
              onChange={(value) => updateDraft({ sidebarLayout: value })}
            />
            <SelectField
              id="appearance-bubbles"
              label={t("AppearancePanel.bubblesLabel")}
              value={draft.chatBubbleStyle}
              options={bubbleOptions}
              onChange={(value) => updateDraft({ chatBubbleStyle: value })}
            />
          </div>
        </Section>

        <Section
          icon={<Accessibility size={17} />}
          title={t("AppearancePanel.accessibilityTitle")}
          description={t("AppearancePanel.accessibilityDescription")}
        >
          <div className="grid gap-2 sm:grid-cols-3">
            {PRESET_OPTIONS.map((preset) => (
              <button
                key={preset.value}
                type="button"
                onClick={() => {
                  if (scope === "workspace") setWorkspaceOverrideEnabled(true);
                  setDraft((current) => applyAccessibleAppearancePreset(current, preset.value));
                  setFeedback(null);
                }}
                className="cursor-pointer rounded-lg border border-border bg-background px-3 py-2 text-left transition-colors hover:border-border-strong hover:bg-surface-2 focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-accent"
              >
                <span className="flex items-center gap-2 text-sm font-medium text-foreground">
                  <preset.icon size={15} />
                  {t(preset.labelKey)}
                </span>
                <span className="mt-1 block text-xs leading-4 text-muted">{t(preset.descriptionKey)}</span>
              </button>
            ))}
          </div>
          <div className="mt-3 grid gap-3 sm:grid-cols-2">
            <div role="radiogroup" aria-label={t("AppearancePanel.motionTitle")} className="grid gap-2 sm:grid-cols-2">
              {MOTION_OPTIONS.map((option) => (
                <OptionButton
                  key={option.value}
                  value={option.value}
                  selected={draft.motionPreference === option.value}
                  label={t(option.labelKey)}
                  description={t(option.descriptionKey)}
                  onSelect={(value) => updateDraft({ motionPreference: value })}
                />
              ))}
            </div>
            <SelectField
              id="appearance-focus"
              label={t("AppearancePanel.focusLabel")}
              value={draft.focusVisibility}
              options={focusOptions}
              onChange={(value) => updateDraft({ focusVisibility: value })}
            />
          </div>
          <div className="mt-3 flex items-center justify-between gap-4 rounded-lg border border-border bg-background px-3 py-3">
            <div className="min-w-0">
              <p className="text-sm font-medium text-foreground">{t("AppearancePanel.highContrastLabel")}</p>
              <p className="mt-1 text-xs leading-5 text-muted">{t("AppearancePanel.highContrastDescription")}</p>
            </div>
            <Toggle
              checked={draft.highContrastEnabled}
              onChange={(value) => updateDraft({ highContrastEnabled: value })}
              label={t("AppearancePanel.highContrastLabel")}
            />
          </div>
        </Section>

        <Section
          icon={<Download size={17} />}
          title={t("AppearancePanel.profilesTitle")}
          description={t("AppearancePanel.profilesDescription")}
        >
          <input
            ref={importInputRef}
            type="file"
            accept="application/json,.json"
            className="hidden"
            onChange={(event) => void importDraft(event)}
          />
          <div className="flex flex-wrap gap-2">
            <Button type="button" size="sm" onClick={() => importInputRef.current?.click()}>
              <Upload size={14} />
              {t("AppearancePanel.importButton")}
            </Button>
            <Button type="button" size="sm" onClick={exportDraft}>
              <Download size={14} />
              {t("AppearancePanel.exportButton")}
            </Button>
          </div>
        </Section>
      </div>

      <aside className="rounded-lg border border-border bg-surface p-4 xl:sticky xl:top-0 xl:self-start">
        <div className="flex items-center justify-between gap-3">
          <h3 className="text-sm font-semibold text-foreground">{t("AppearancePanel.previewTitle")}</h3>
          <span className="rounded-md bg-surface-2 px-2 py-1 text-xs font-medium text-muted">
            {resolvedTheme === "dark" ? t("AppearancePanel.previewDark") : t("AppearancePanel.previewLight")}
          </span>
        </div>
        <p className="mt-1 text-xs leading-5 text-muted">{t("AppearancePanel.previewDescription")}</p>
        <div className="mt-4 overflow-hidden rounded-lg border border-border bg-background">
          <div className="flex min-h-56">
            <div
              className="shrink-0 border-r border-border bg-surface p-2"
              style={{ width: draft.sidebarLayout === "compact" ? "28%" : draft.sidebarLayout === "wide" ? "42%" : "35%" }}
            >
              <div className="h-5 rounded bg-accent-soft" />
              <div className="mt-2 h-3 rounded bg-surface-2" />
              <div className="mt-1.5 h-3 w-4/5 rounded bg-surface-2" />
            </div>
            <div className="min-w-0 flex-1 p-3" style={{ display: "flex", flexDirection: "column", gap: previewGap }}>
              <div className="flex items-center gap-2">
                <span className="h-7 w-7 rounded-md bg-accent" />
                <div className="min-w-0">
                  <p className="truncate text-xs font-semibold text-foreground">{t("AppearancePanel.previewWorkspace")}</p>
                  <p className="truncate text-[10px] text-muted">{t("AppearancePanel.previewSubline")}</p>
                </div>
              </div>
              <div className="rounded-md border-l-2 border-accent px-2 py-1.5 text-[11px] text-muted">
                {t("AppearancePanel.previewAssistant")}
              </div>
              <div className={classNames("ml-auto max-w-[85%] px-2 py-1.5 text-[11px] text-foreground", previewBubbleClass)}>
                {t("AppearancePanel.previewUser")}
              </div>
              <code className="rounded border border-border bg-surface-2 px-2 py-1 font-mono text-foreground" style={{ fontSize: draft.codeFontSize }}>
                npm run verify
              </code>
              <button
                type="button"
                className="mt-auto cursor-pointer self-end rounded-md bg-accent px-2 py-1 text-xs font-medium text-accent-foreground focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-accent"
              >
                {t("AppearancePanel.previewPrimary")}
              </button>
            </div>
          </div>
        </div>

        <div className="mt-4 flex items-center gap-2 text-xs text-muted">
          {dirty ? <Contrast size={14} className="text-accent" /> : <Gauge size={14} />}
          <span>{dirty ? t("AppearancePanel.unsavedStatus") : t("AppearancePanel.savedDraftStatus")}</span>
        </div>
        {feedback && (
          <p
            role={feedback.kind === "error" ? "alert" : "status"}
            aria-live="polite"
            className={classNames(
              "mt-2 rounded-md border px-2.5 py-2 text-xs leading-5",
              feedback.kind === "error"
                ? "border-danger/30 bg-danger-soft text-danger"
                : "border-border bg-surface-2 text-muted",
            )}
          >
            {feedback.text}
          </p>
        )}
        <div className="mt-4 grid grid-cols-2 gap-2">
          <Button type="button" onClick={cancelDraft} disabled={!dirty}>
            {t("AppearancePanel.cancelButton")}
          </Button>
          <Button type="button" variant="primary" onClick={applyDraft} disabled={!dirty}>
            {t("AppearancePanel.applyButton")}
          </Button>
        </div>
      </aside>
    </div>
  );
}
