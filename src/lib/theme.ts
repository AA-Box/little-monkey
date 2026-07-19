import { useSyncExternalStore } from "react";

export type Theme = "light" | "dark";
export type ThemePreference = Theme | "system";
export type AccentColor = "default" | "indigo" | "blue" | "teal" | "rose" | "amber";
export type TextScale = "small" | "medium" | "large";
export type MotionPreference = "system" | "reduced";

export interface AppearanceSettings {
  themePreference: ThemePreference;
  accentColor: AccentColor;
  textScale: TextScale;
  motionPreference: MotionPreference;
  highContrastEnabled: boolean;
}

export const THEME_STORAGE_KEY = "little-monkey-theme";

export const DEFAULT_APPEARANCE_SETTINGS: AppearanceSettings = {
  themePreference: "system",
  accentColor: "default",
  textScale: "medium",
  motionPreference: "system",
  highContrastEnabled: false,
};

const THEME_PREFERENCES = new Set<ThemePreference>(["system", "light", "dark"]);
const ACCENT_COLORS = new Set<AccentColor>(["default", "indigo", "blue", "teal", "rose", "amber"]);
const TEXT_SCALES = new Set<TextScale>(["small", "medium", "large"]);
const MOTION_PREFERENCES = new Set<MotionPreference>(["system", "reduced"]);

function safeGetStorage(key: string): string | null {
  try {
    return typeof localStorage === "undefined" ? null : localStorage.getItem(key);
  } catch {
    return null;
  }
}

function safeSetStorage(key: string, value: string): void {
  try {
    if (typeof localStorage !== "undefined") localStorage.setItem(key, value);
  } catch {
    // Appearance should still update even when persistence is unavailable.
  }
}

export function isThemePreference(value: unknown): value is ThemePreference {
  return typeof value === "string" && THEME_PREFERENCES.has(value as ThemePreference);
}

export function isAccentColor(value: unknown): value is AccentColor {
  return typeof value === "string" && ACCENT_COLORS.has(value as AccentColor);
}

export function isTextScale(value: unknown): value is TextScale {
  return typeof value === "string" && TEXT_SCALES.has(value as TextScale);
}

export function isMotionPreference(value: unknown): value is MotionPreference {
  return typeof value === "string" && MOTION_PREFERENCES.has(value as MotionPreference);
}

export function getSystemTheme(): Theme {
  if (typeof window === "undefined" || typeof window.matchMedia !== "function") return "light";
  return window.matchMedia("(prefers-color-scheme: dark)").matches ? "dark" : "light";
}

export function resolveTheme(preference: ThemePreference): Theme {
  return preference === "system" ? getSystemTheme() : preference;
}

/** React-safe resolved theme that updates when the OS preference changes. */
export function useResolvedTheme(preference: ThemePreference): Theme {
  const systemTheme = useSyncExternalStore<Theme>(subscribeToSystemTheme, getSystemTheme, () => "light");
  return preference === "system" ? systemTheme : preference;
}

export function getStoredThemePreference(): ThemePreference {
  const stored = safeGetStorage(THEME_STORAGE_KEY);
  return isThemePreference(stored) ? stored : DEFAULT_APPEARANCE_SETTINGS.themePreference;
}

export function getStoredTheme(): Theme {
  if (typeof document !== "undefined") {
    const applied = document.documentElement.getAttribute("data-theme");
    if (applied === "light" || applied === "dark") return applied;
  }
  return resolveTheme(getStoredThemePreference());
}

export function normalizeAppearanceSettings(settings: Partial<AppearanceSettings> = {}): AppearanceSettings {
  return {
    themePreference: isThemePreference(settings.themePreference)
      ? settings.themePreference
      : DEFAULT_APPEARANCE_SETTINGS.themePreference,
    accentColor: isAccentColor(settings.accentColor)
      ? settings.accentColor
      : DEFAULT_APPEARANCE_SETTINGS.accentColor,
    textScale: isTextScale(settings.textScale)
      ? settings.textScale
      : DEFAULT_APPEARANCE_SETTINGS.textScale,
    motionPreference: isMotionPreference(settings.motionPreference)
      ? settings.motionPreference
      : DEFAULT_APPEARANCE_SETTINGS.motionPreference,
    highContrastEnabled: typeof settings.highContrastEnabled === "boolean"
      ? settings.highContrastEnabled
      : DEFAULT_APPEARANCE_SETTINGS.highContrastEnabled,
  };
}

export function applyTheme(preference: ThemePreference): Theme {
  const themePreference = isThemePreference(preference) ? preference : DEFAULT_APPEARANCE_SETTINGS.themePreference;
  const resolvedTheme = resolveTheme(themePreference);
  if (typeof document !== "undefined") {
    document.documentElement.setAttribute("data-theme", resolvedTheme);
    document.documentElement.setAttribute("data-theme-preference", themePreference);
  }
  safeSetStorage(THEME_STORAGE_KEY, themePreference);
  return resolvedTheme;
}

export function applyAppearance(settings: Partial<AppearanceSettings> = {}): Theme {
  const appearance = normalizeAppearanceSettings(settings);
  const resolvedTheme = applyTheme(appearance.themePreference);
  if (typeof document !== "undefined") {
    const root = document.documentElement;
    root.setAttribute("data-accent", appearance.accentColor);
    root.setAttribute("data-text-scale", appearance.textScale);
    root.setAttribute("data-motion", appearance.motionPreference);
    root.setAttribute("data-contrast", appearance.highContrastEnabled ? "high" : "normal");
  }
  return resolvedTheme;
}

export function subscribeToSystemTheme(callback: () => void): () => void {
  if (typeof window === "undefined" || typeof window.matchMedia !== "function") return () => {};
  const query = window.matchMedia("(prefers-color-scheme: dark)");
  if (typeof query.addEventListener === "function") {
    query.addEventListener("change", callback);
    return () => query.removeEventListener("change", callback);
  }
  query.addListener(callback);
  return () => query.removeListener(callback);
}
