import type { ReactNode } from "react";
import { Contrast, Moon, Monitor, Palette, Sun, Type, ZapOff, type LucideIcon } from "lucide-react";
import { useT } from "../../lib/i18n";
import {
  useResolvedTheme,
  type AccentColor,
  type MotionPreference,
  type TextScale,
  type ThemePreference,
} from "../../lib/theme";
import { useSettingsStore } from "../../store/settingsStore";

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

function OptionButton<T extends string>({
  selected,
  value,
  label,
  description,
  icon: Icon,
  onSelect,
}: {
  selected: boolean;
  value: T;
  label: string;
  description?: string;
  icon?: LucideIcon;
  onSelect: (value: T) => void;
}) {
  return (
    <button
      type="button"
      role="radio"
      aria-checked={selected}
      onClick={() => onSelect(value)}
      className={classNames(
        "min-h-20 rounded-lg border px-3 py-2 text-left transition-colors focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-accent",
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

const MOTION_OPTIONS: Array<{ value: MotionPreference; labelKey: string; descriptionKey: string }> = [
  { value: "system", labelKey: "AppearancePanel.motionSystem", descriptionKey: "AppearancePanel.motionSystemDescription" },
  { value: "reduced", labelKey: "AppearancePanel.motionReduced", descriptionKey: "AppearancePanel.motionReducedDescription" },
];

export function AppearancePanel() {
  const { t } = useT();
  const themePreference = useSettingsStore((state) => state.themePreference);
  const accentColor = useSettingsStore((state) => state.accentColor);
  const textScale = useSettingsStore((state) => state.textScale);
  const motionPreference = useSettingsStore((state) => state.motionPreference);
  const highContrastEnabled = useSettingsStore((state) => state.highContrastEnabled);
  const setThemePreference = useSettingsStore((state) => state.setThemePreference);
  const setAccentColor = useSettingsStore((state) => state.setAccentColor);
  const setTextScale = useSettingsStore((state) => state.setTextScale);
  const setMotionPreference = useSettingsStore((state) => state.setMotionPreference);
  const setHighContrastEnabled = useSettingsStore((state) => state.setHighContrastEnabled);
  const resolvedTheme = useResolvedTheme(themePreference);

  return (
    <div className="grid gap-5 xl:grid-cols-[minmax(0,1fr)_18rem]">
      <div className="flex min-w-0 flex-col gap-4">
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
                selected={themePreference === option.value}
                label={t(option.labelKey)}
                description={t(option.descriptionKey)}
                icon={option.icon}
                onSelect={setThemePreference}
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
              const selected = accentColor === option.value;
              return (
                <button
                  key={option.value}
                  type="button"
                  role="radio"
                  aria-checked={selected}
                  onClick={() => setAccentColor(option.value)}
                  className={classNames(
                    "flex min-h-11 items-center gap-2 rounded-lg border px-3 py-2 text-left text-sm transition-colors focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-accent",
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
          title={t("AppearancePanel.textTitle")}
          description={t("AppearancePanel.textDescription")}
        >
          <div className="grid gap-2 sm:grid-cols-3" role="radiogroup" aria-label={t("AppearancePanel.textTitle")}>
            {TEXT_SCALE_OPTIONS.map((option) => (
              <OptionButton
                key={option.value}
                value={option.value}
                selected={textScale === option.value}
                label={t(option.labelKey)}
                description={t(option.descriptionKey)}
                onSelect={setTextScale}
              />
            ))}
          </div>
        </Section>

        <Section
          icon={<ZapOff size={17} />}
          title={t("AppearancePanel.motionTitle")}
          description={t("AppearancePanel.motionDescription")}
        >
          <div className="grid gap-2 sm:grid-cols-2" role="radiogroup" aria-label={t("AppearancePanel.motionTitle")}>
            {MOTION_OPTIONS.map((option) => (
              <OptionButton
                key={option.value}
                value={option.value}
                selected={motionPreference === option.value}
                label={t(option.labelKey)}
                description={t(option.descriptionKey)}
                onSelect={setMotionPreference}
              />
            ))}
          </div>
        </Section>

        <Section icon={<Contrast size={17} />} title={t("AppearancePanel.contrastTitle")}>
          <div className="flex items-center justify-between gap-4 rounded-lg border border-border bg-background px-3 py-3">
            <div className="min-w-0">
              <p className="text-sm font-medium text-foreground">{t("AppearancePanel.highContrastLabel")}</p>
              <p className="mt-1 text-xs leading-5 text-muted">{t("AppearancePanel.highContrastDescription")}</p>
            </div>
            <Toggle
              checked={highContrastEnabled}
              onChange={setHighContrastEnabled}
              label={t("AppearancePanel.highContrastLabel")}
            />
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
        <div className="mt-4 rounded-lg border border-border bg-background p-3">
          <div className="flex items-center gap-2">
            <span className="h-8 w-8 rounded-md bg-accent" />
            <div className="min-w-0">
              <p className="truncate text-sm font-semibold text-foreground">{t("AppearancePanel.previewWorkspace")}</p>
              <p className="truncate text-xs text-muted">{t("AppearancePanel.previewSubline")}</p>
            </div>
          </div>
          <div className="mt-4 space-y-2">
            <div className="h-2.5 w-full rounded-full bg-surface-2" />
            <div className="h-2.5 w-5/6 rounded-full bg-surface-2" />
            <div className="h-2.5 w-2/3 rounded-full bg-accent-soft" />
          </div>
          <div className="mt-4 flex items-center justify-between gap-2">
            <span className="rounded-md border border-border px-2 py-1 text-xs text-muted">{t("AppearancePanel.previewSecondary")}</span>
            <span className="rounded-md bg-accent px-2 py-1 text-xs font-medium text-accent-foreground">
              {t("AppearancePanel.previewPrimary")}
            </span>
          </div>
        </div>
      </aside>
    </div>
  );
}
