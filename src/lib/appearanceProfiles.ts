import {
  DEFAULT_APPEARANCE_SETTINGS,
  isAccentColor,
  isChatBubbleStyle,
  isCodeFontSize,
  isFocusVisibility,
  isMotionPreference,
  isSidebarLayout,
  isTextScale,
  isThemePreference,
  isUiDensity,
  normalizeAppearanceSettings,
  type AppearanceSettings,
} from "./theme";

export const APPEARANCE_PROFILE_SCHEMA = "little-monkey.appearance-profile";
export const APPEARANCE_PROFILE_VERSION = 1 as const;
export const MAX_APPEARANCE_PROFILE_LENGTH = 64 * 1024;

export const APPEARANCE_SETTING_KEYS = [
  "themePreference",
  "accentColor",
  "textScale",
  "codeFontSize",
  "uiDensity",
  "sidebarLayout",
  "chatBubbleStyle",
  "motionPreference",
  "highContrastEnabled",
  "focusVisibility",
] as const satisfies readonly (keyof AppearanceSettings)[];

export type AppearanceWorkspaceOverride = Partial<AppearanceSettings>;
export type AppearanceWorkspaceOverrides = Record<string, AppearanceWorkspaceOverride>;
export type AccessibleAppearancePreset = "low-vision" | "keyboard" | "reduced-motion";

export interface AppearanceProfileV1 {
  schema: typeof APPEARANCE_PROFILE_SCHEMA;
  version: typeof APPEARANCE_PROFILE_VERSION;
  name?: string;
  appearance: AppearanceSettings;
}

export class AppearanceProfileError extends Error {
  constructor(message: string) {
    super(message);
    this.name = "AppearanceProfileError";
  }
}

function recordOf(value: unknown, label: string): Record<string, unknown> {
  if (!value || typeof value !== "object" || Array.isArray(value)) {
    throw new AppearanceProfileError(`${label} must be a JSON object.`);
  }
  return value as Record<string, unknown>;
}

function profileName(value: unknown): string | undefined {
  if (value === undefined) return undefined;
  if (typeof value !== "string") throw new AppearanceProfileError("Profile name must be text.");
  const trimmed = value.trim();
  if (!trimmed || trimmed.length > 80) {
    throw new AppearanceProfileError("Profile name must contain 1 to 80 characters.");
  }
  return trimmed;
}

/** Strict validation used for current-version imports and before export. */
export function validateAppearanceSettings(value: unknown): AppearanceSettings {
  const record = recordOf(value, "Appearance settings");
  const errors: string[] = [];

  if (!isThemePreference(record.themePreference)) errors.push("themePreference");
  if (!isAccentColor(record.accentColor)) errors.push("accentColor");
  if (!isTextScale(record.textScale)) errors.push("textScale");
  if (!isCodeFontSize(record.codeFontSize)) errors.push("codeFontSize");
  if (!isUiDensity(record.uiDensity)) errors.push("uiDensity");
  if (!isSidebarLayout(record.sidebarLayout)) errors.push("sidebarLayout");
  if (!isChatBubbleStyle(record.chatBubbleStyle)) errors.push("chatBubbleStyle");
  if (!isMotionPreference(record.motionPreference)) errors.push("motionPreference");
  if (typeof record.highContrastEnabled !== "boolean") errors.push("highContrastEnabled");
  if (!isFocusVisibility(record.focusVisibility)) errors.push("focusVisibility");

  if (errors.length > 0) {
    throw new AppearanceProfileError(`Invalid or missing appearance field(s): ${errors.join(", ")}.`);
  }

  return {
    themePreference: record.themePreference as AppearanceSettings["themePreference"],
    accentColor: record.accentColor as AppearanceSettings["accentColor"],
    textScale: record.textScale as AppearanceSettings["textScale"],
    codeFontSize: record.codeFontSize as AppearanceSettings["codeFontSize"],
    uiDensity: record.uiDensity as AppearanceSettings["uiDensity"],
    sidebarLayout: record.sidebarLayout as AppearanceSettings["sidebarLayout"],
    chatBubbleStyle: record.chatBubbleStyle as AppearanceSettings["chatBubbleStyle"],
    motionPreference: record.motionPreference as AppearanceSettings["motionPreference"],
    highContrastEnabled: record.highContrastEnabled as boolean,
    focusVisibility: record.focusVisibility as AppearanceSettings["focusVisibility"],
  };
}

/** Partial validation is used for future-proof workspace overrides and v0 migration. */
export function validateAppearanceOverride(value: unknown): AppearanceWorkspaceOverride {
  const record = recordOf(value, "Appearance override");
  const result: AppearanceWorkspaceOverride = {};
  const invalid: string[] = [];

  if ("themePreference" in record) {
    if (isThemePreference(record.themePreference)) result.themePreference = record.themePreference;
    else invalid.push("themePreference");
  }
  if ("accentColor" in record) {
    if (isAccentColor(record.accentColor)) result.accentColor = record.accentColor;
    else invalid.push("accentColor");
  }
  if ("textScale" in record) {
    if (isTextScale(record.textScale)) result.textScale = record.textScale;
    else invalid.push("textScale");
  }
  if ("codeFontSize" in record) {
    if (isCodeFontSize(record.codeFontSize)) result.codeFontSize = record.codeFontSize;
    else invalid.push("codeFontSize");
  }
  if ("uiDensity" in record) {
    if (isUiDensity(record.uiDensity)) result.uiDensity = record.uiDensity;
    else invalid.push("uiDensity");
  }
  if ("sidebarLayout" in record) {
    if (isSidebarLayout(record.sidebarLayout)) result.sidebarLayout = record.sidebarLayout;
    else invalid.push("sidebarLayout");
  }
  if ("chatBubbleStyle" in record) {
    if (isChatBubbleStyle(record.chatBubbleStyle)) result.chatBubbleStyle = record.chatBubbleStyle;
    else invalid.push("chatBubbleStyle");
  }
  if ("motionPreference" in record) {
    if (isMotionPreference(record.motionPreference)) result.motionPreference = record.motionPreference;
    else invalid.push("motionPreference");
  }
  if ("highContrastEnabled" in record) {
    if (typeof record.highContrastEnabled === "boolean") result.highContrastEnabled = record.highContrastEnabled;
    else invalid.push("highContrastEnabled");
  }
  if ("focusVisibility" in record) {
    if (isFocusVisibility(record.focusVisibility)) result.focusVisibility = record.focusVisibility;
    else invalid.push("focusVisibility");
  }

  if (invalid.length > 0) {
    throw new AppearanceProfileError(`Invalid appearance override field(s): ${invalid.join(", ")}.`);
  }
  return result;
}

function legacyAppearance(record: Record<string, unknown>): AppearanceSettings {
  const nested = record.appearance && typeof record.appearance === "object" && !Array.isArray(record.appearance)
    ? record.appearance
    : record;
  const partial = validateAppearanceOverride(nested);
  const hasAppearanceField = APPEARANCE_SETTING_KEYS.some((key) => key in (nested as object));
  if (!hasAppearanceField && record.version !== 0) {
    throw new AppearanceProfileError("The file does not contain an appearance profile.");
  }
  return normalizeAppearanceSettings({ ...DEFAULT_APPEARANCE_SETTINGS, ...partial });
}

/**
 * Migrates the pre-versioned and version-0 shapes used by early prototypes.
 * Missing modern fields receive safe defaults; supplied legacy fields remain
 * strictly validated so malformed imports never silently become defaults.
 */
export function migrateAppearanceProfile(value: unknown): AppearanceProfileV1 {
  const record = recordOf(value, "Appearance profile");
  const version = record.version;

  if (version === APPEARANCE_PROFILE_VERSION) {
    if (record.schema !== APPEARANCE_PROFILE_SCHEMA) {
      throw new AppearanceProfileError("Unsupported appearance profile schema.");
    }
    const name = profileName(record.name);
    return {
      schema: APPEARANCE_PROFILE_SCHEMA,
      version: APPEARANCE_PROFILE_VERSION,
      ...(name ? { name } : {}),
      appearance: validateAppearanceSettings(record.appearance),
    };
  }

  if (version !== undefined && version !== 0) {
    throw new AppearanceProfileError(`Unsupported appearance profile version: ${String(version)}.`);
  }

  const name = profileName(record.name);
  return {
    schema: APPEARANCE_PROFILE_SCHEMA,
    version: APPEARANCE_PROFILE_VERSION,
    ...(name ? { name } : {}),
    appearance: legacyAppearance(record),
  };
}

export function importAppearanceProfile(serialized: string): AppearanceProfileV1 {
  if (typeof serialized !== "string" || serialized.length === 0) {
    throw new AppearanceProfileError("Appearance profile is empty.");
  }
  if (serialized.length > MAX_APPEARANCE_PROFILE_LENGTH) {
    throw new AppearanceProfileError("Appearance profile is too large.");
  }
  let parsed: unknown;
  try {
    parsed = JSON.parse(serialized);
  } catch {
    throw new AppearanceProfileError("Appearance profile is not valid JSON.");
  }
  return migrateAppearanceProfile(parsed);
}

export function exportAppearanceProfile(appearance: unknown, name?: string): string {
  const normalizedName = profileName(name);
  const profile: AppearanceProfileV1 = {
    schema: APPEARANCE_PROFILE_SCHEMA,
    version: APPEARANCE_PROFILE_VERSION,
    ...(normalizedName ? { name: normalizedName } : {}),
    appearance: validateAppearanceSettings(appearance),
  };
  return `${JSON.stringify(profile, null, 2)}\n`;
}

export function sanitizeAppearanceWorkspaceOverrides(value: unknown): AppearanceWorkspaceOverrides {
  if (!value || typeof value !== "object" || Array.isArray(value)) return {};
  const sanitized: AppearanceWorkspaceOverrides = {};
  for (const [workspaceKey, candidate] of Object.entries(value as Record<string, unknown>).slice(0, 200)) {
    if (!workspaceKey || workspaceKey.length > 4096) continue;
    try {
      sanitized[workspaceKey] = validateAppearanceOverride(candidate);
    } catch {
      // One hand-edited or stale entry must not discard every valid workspace.
    }
  }
  return sanitized;
}

export function resolveAppearanceSettings(
  deviceAppearance: unknown,
  workspaceOverrides: AppearanceWorkspaceOverrides,
  workspaceKey: string | null | undefined,
): AppearanceSettings {
  const device = normalizeAppearanceSettings(deviceAppearance);
  if (!workspaceKey || !Object.prototype.hasOwnProperty.call(workspaceOverrides, workspaceKey)) return device;
  return normalizeAppearanceSettings({ ...device, ...workspaceOverrides[workspaceKey] });
}

export function createAppearanceWorkspaceOverride(
  deviceAppearance: AppearanceSettings,
  workspaceAppearance: AppearanceSettings,
): AppearanceWorkspaceOverride {
  const override: AppearanceWorkspaceOverride = {};
  for (const key of APPEARANCE_SETTING_KEYS) {
    if (deviceAppearance[key] !== workspaceAppearance[key]) {
      Object.assign(override, { [key]: workspaceAppearance[key] });
    }
  }
  return override;
}

export function appearanceSettingsEqual(left: AppearanceSettings, right: AppearanceSettings): boolean {
  return APPEARANCE_SETTING_KEYS.every((key) => left[key] === right[key]);
}

export function applyAccessibleAppearancePreset(
  appearance: AppearanceSettings,
  preset: AccessibleAppearancePreset,
): AppearanceSettings {
  if (preset === "low-vision") {
    return {
      ...appearance,
      textScale: "large",
      codeFontSize: 16,
      uiDensity: "spacious",
      sidebarLayout: "wide",
      highContrastEnabled: true,
      focusVisibility: "enhanced",
    };
  }
  if (preset === "keyboard") {
    return { ...appearance, focusVisibility: "enhanced", highContrastEnabled: true };
  }
  return { ...appearance, motionPreference: "reduced", focusVisibility: "enhanced" };
}
