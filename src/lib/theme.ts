import { useSyncExternalStore } from "react";

export type Theme = "light" | "dark";
export type ThemePreference = Theme | "system";
export type AccentColor = "default" | "indigo" | "blue" | "teal" | "rose" | "amber";
export type TextScale = "small" | "medium" | "large";
export type MotionPreference = "system" | "reduced";
export type CodeFontSize = 12 | 14 | 16;
export type UiDensity = "compact" | "comfortable" | "spacious";
export type SidebarLayout = "compact" | "standard" | "wide";
export type ChatBubbleStyle = "bubbles" | "flat" | "compact";
export type FocusVisibility = "standard" | "enhanced";

export interface AppearanceSettings {
  themePreference: ThemePreference;
  accentColor: AccentColor;
  textScale: TextScale;
  codeFontSize: CodeFontSize;
  uiDensity: UiDensity;
  sidebarLayout: SidebarLayout;
  chatBubbleStyle: ChatBubbleStyle;
  motionPreference: MotionPreference;
  highContrastEnabled: boolean;
  focusVisibility: FocusVisibility;
}

export const THEME_STORAGE_KEY = "little-monkey-theme";

export const DEFAULT_APPEARANCE_SETTINGS: AppearanceSettings = {
  themePreference: "system",
  accentColor: "default",
  textScale: "medium",
  codeFontSize: 14,
  uiDensity: "comfortable",
  sidebarLayout: "standard",
  chatBubbleStyle: "bubbles",
  motionPreference: "system",
  highContrastEnabled: false,
  focusVisibility: "standard",
};

const THEME_PREFERENCES = new Set<ThemePreference>(["system", "light", "dark"]);
const ACCENT_COLORS = new Set<AccentColor>(["default", "indigo", "blue", "teal", "rose", "amber"]);
const TEXT_SCALES = new Set<TextScale>(["small", "medium", "large"]);
const MOTION_PREFERENCES = new Set<MotionPreference>(["system", "reduced"]);
const CODE_FONT_SIZES = new Set<CodeFontSize>([12, 14, 16]);
const UI_DENSITIES = new Set<UiDensity>(["compact", "comfortable", "spacious"]);
const SIDEBAR_LAYOUTS = new Set<SidebarLayout>(["compact", "standard", "wide"]);
const CHAT_BUBBLE_STYLES = new Set<ChatBubbleStyle>(["bubbles", "flat", "compact"]);
const FOCUS_VISIBILITIES = new Set<FocusVisibility>(["standard", "enhanced"]);

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

export function isCodeFontSize(value: unknown): value is CodeFontSize {
  return typeof value === "number" && CODE_FONT_SIZES.has(value as CodeFontSize);
}

export function isUiDensity(value: unknown): value is UiDensity {
  return typeof value === "string" && UI_DENSITIES.has(value as UiDensity);
}

export function isSidebarLayout(value: unknown): value is SidebarLayout {
  return typeof value === "string" && SIDEBAR_LAYOUTS.has(value as SidebarLayout);
}

export function isChatBubbleStyle(value: unknown): value is ChatBubbleStyle {
  return typeof value === "string" && CHAT_BUBBLE_STYLES.has(value as ChatBubbleStyle);
}

export function isFocusVisibility(value: unknown): value is FocusVisibility {
  return typeof value === "string" && FOCUS_VISIBILITIES.has(value as FocusVisibility);
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

export function normalizeAppearanceSettings(settings: unknown = {}): AppearanceSettings {
  const candidate = settings && typeof settings === "object"
    ? settings as Record<string, unknown>
    : {};
  return {
    themePreference: isThemePreference(candidate.themePreference)
      ? candidate.themePreference
      : DEFAULT_APPEARANCE_SETTINGS.themePreference,
    accentColor: isAccentColor(candidate.accentColor)
      ? candidate.accentColor
      : DEFAULT_APPEARANCE_SETTINGS.accentColor,
    textScale: isTextScale(candidate.textScale)
      ? candidate.textScale
      : DEFAULT_APPEARANCE_SETTINGS.textScale,
    codeFontSize: isCodeFontSize(candidate.codeFontSize)
      ? candidate.codeFontSize
      : DEFAULT_APPEARANCE_SETTINGS.codeFontSize,
    uiDensity: isUiDensity(candidate.uiDensity)
      ? candidate.uiDensity
      : DEFAULT_APPEARANCE_SETTINGS.uiDensity,
    sidebarLayout: isSidebarLayout(candidate.sidebarLayout)
      ? candidate.sidebarLayout
      : DEFAULT_APPEARANCE_SETTINGS.sidebarLayout,
    chatBubbleStyle: isChatBubbleStyle(candidate.chatBubbleStyle)
      ? candidate.chatBubbleStyle
      : DEFAULT_APPEARANCE_SETTINGS.chatBubbleStyle,
    motionPreference: isMotionPreference(candidate.motionPreference)
      ? candidate.motionPreference
      : DEFAULT_APPEARANCE_SETTINGS.motionPreference,
    highContrastEnabled: typeof candidate.highContrastEnabled === "boolean"
      ? candidate.highContrastEnabled
      : DEFAULT_APPEARANCE_SETTINGS.highContrastEnabled,
    focusVisibility: isFocusVisibility(candidate.focusVisibility)
      ? candidate.focusVisibility
      : DEFAULT_APPEARANCE_SETTINGS.focusVisibility,
  };
}

export interface ApplyAppearanceOptions {
  /** Draft previews must never become the legacy persisted theme fallback. */
  persistThemePreference?: boolean;
}

export function applyTheme(preference: ThemePreference, options: ApplyAppearanceOptions = {}): Theme {
  const themePreference = isThemePreference(preference) ? preference : DEFAULT_APPEARANCE_SETTINGS.themePreference;
  const resolvedTheme = resolveTheme(themePreference);
  if (typeof document !== "undefined") {
    document.documentElement.setAttribute("data-theme", resolvedTheme);
    document.documentElement.setAttribute("data-theme-preference", themePreference);
  }
  if (options.persistThemePreference !== false) safeSetStorage(THEME_STORAGE_KEY, themePreference);
  return resolvedTheme;
}

export function applyAppearance(settings: unknown = {}, options: ApplyAppearanceOptions = {}): Theme {
  const appearance = normalizeAppearanceSettings(settings);
  const resolvedTheme = applyTheme(appearance.themePreference, options);
  if (typeof document !== "undefined") {
    const root = document.documentElement;
    root.setAttribute("data-accent", appearance.accentColor);
    root.setAttribute("data-text-scale", appearance.textScale);
    root.setAttribute("data-code-font-size", String(appearance.codeFontSize));
    root.setAttribute("data-ui-density", appearance.uiDensity);
    root.setAttribute("data-sidebar-layout", appearance.sidebarLayout);
    root.setAttribute("data-chat-bubble-style", appearance.chatBubbleStyle);
    root.setAttribute("data-motion", appearance.motionPreference);
    root.setAttribute("data-contrast", appearance.highContrastEnabled ? "high" : "normal");
    root.setAttribute("data-focus-visibility", appearance.focusVisibility);
    root.style.setProperty("--code-font-size", `${appearance.codeFontSize}px`);
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
