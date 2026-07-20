import { describe, expect, it } from "vitest";

import { DEFAULT_APPEARANCE_SETTINGS, type AppearanceSettings } from "./theme";
import {
  APPEARANCE_PROFILE_SCHEMA,
  APPEARANCE_PROFILE_VERSION,
  AppearanceProfileError,
  appearanceSettingsEqual,
  applyAccessibleAppearancePreset,
  createAppearanceWorkspaceOverride,
  exportAppearanceProfile,
  importAppearanceProfile,
  migrateAppearanceProfile,
  resolveAppearanceSettings,
  sanitizeAppearanceWorkspaceOverrides,
  validateAppearanceSettings,
} from "./appearanceProfiles";

const CUSTOM_APPEARANCE: AppearanceSettings = {
  themePreference: "dark",
  accentColor: "teal",
  textScale: "large",
  codeFontSize: 16,
  uiDensity: "spacious",
  sidebarLayout: "wide",
  chatBubbleStyle: "flat",
  motionPreference: "reduced",
  highContrastEnabled: true,
  focusVisibility: "enhanced",
};

describe("appearance profile validation and migration", () => {
  it("accepts a complete current appearance and rejects malformed fields", () => {
    expect(validateAppearanceSettings(CUSTOM_APPEARANCE)).toEqual(CUSTOM_APPEARANCE);
    expect(() => validateAppearanceSettings({ ...CUSTOM_APPEARANCE, codeFontSize: 72 }))
      .toThrow(/codeFontSize/);
    expect(() => validateAppearanceSettings({ ...CUSTOM_APPEARANCE, focusVisibility: "invisible" }))
      .toThrow(/focusVisibility/);
  });

  it("migrates a version-0 profile and fills fields introduced in version 1", () => {
    const migrated = migrateAppearanceProfile({
      version: 0,
      name: "Legacy team profile",
      appearance: {
        themePreference: "dark",
        accentColor: "blue",
        textScale: "large",
        motionPreference: "reduced",
        highContrastEnabled: true,
      },
    });

    expect(migrated).toEqual({
      schema: APPEARANCE_PROFILE_SCHEMA,
      version: APPEARANCE_PROFILE_VERSION,
      name: "Legacy team profile",
      appearance: {
        ...DEFAULT_APPEARANCE_SETTINGS,
        themePreference: "dark",
        accentColor: "blue",
        textScale: "large",
        motionPreference: "reduced",
        highContrastEnabled: true,
      },
    });
  });

  it("rejects invalid legacy values instead of silently defaulting them", () => {
    expect(() => migrateAppearanceProfile({ themePreference: "midnight" }))
      .toThrow(AppearanceProfileError);
    expect(() => migrateAppearanceProfile({ version: 99, appearance: CUSTOM_APPEARANCE }))
      .toThrow(/Unsupported appearance profile version/);
  });
});

describe("appearance profile import and export", () => {
  it("round-trips a validated versioned profile without workspace identity", () => {
    const serialized = exportAppearanceProfile(CUSTOM_APPEARANCE, "Homelab shared profile");
    const parsedJson = JSON.parse(serialized) as Record<string, unknown>;

    expect(parsedJson).not.toHaveProperty("workspacePath");
    expect(importAppearanceProfile(serialized)).toEqual({
      schema: APPEARANCE_PROFILE_SCHEMA,
      version: APPEARANCE_PROFILE_VERSION,
      name: "Homelab shared profile",
      appearance: CUSTOM_APPEARANCE,
    });
  });

  it("rejects non-JSON, wrong-schema, and oversized imports", () => {
    expect(() => importAppearanceProfile("not-json")).toThrow(/valid JSON/);
    expect(() => importAppearanceProfile(JSON.stringify({
      schema: "other.product",
      version: 1,
      appearance: CUSTOM_APPEARANCE,
    }))).toThrow(/schema/);
    expect(() => importAppearanceProfile(" ".repeat(70_000))).toThrow(/too large/);
  });
});

describe("appearance workspace overrides", () => {
  it("inherits device fields and changes only explicit workspace fields", () => {
    const overrides = {
      "/work/project": { accentColor: "rose" as const, sidebarLayout: "compact" as const },
    };

    expect(resolveAppearanceSettings(DEFAULT_APPEARANCE_SETTINGS, overrides, "/work/project")).toEqual({
      ...DEFAULT_APPEARANCE_SETTINGS,
      accentColor: "rose",
      sidebarLayout: "compact",
    });
    expect(resolveAppearanceSettings(DEFAULT_APPEARANCE_SETTINGS, overrides, "/work/other"))
      .toEqual(DEFAULT_APPEARANCE_SETTINGS);
  });

  it("creates a sparse override that resolves back to the full workspace profile", () => {
    const override = createAppearanceWorkspaceOverride(DEFAULT_APPEARANCE_SETTINGS, CUSTOM_APPEARANCE);
    const resolved = resolveAppearanceSettings(DEFAULT_APPEARANCE_SETTINGS, { workspace: override }, "workspace");

    expect(Object.keys(override).length).toBeGreaterThan(0);
    expect(appearanceSettingsEqual(resolved, CUSTOM_APPEARANCE)).toBe(true);
  });

  it("keeps valid entries while dropping a malformed persisted workspace", () => {
    expect(sanitizeAppearanceWorkspaceOverrides({
      valid: { focusVisibility: "enhanced" },
      invalid: { codeFontSize: 999 },
    })).toEqual({ valid: { focusVisibility: "enhanced" } });
  });

  it("provides accessibility presets without mutating the input", () => {
    const lowVision = applyAccessibleAppearancePreset(DEFAULT_APPEARANCE_SETTINGS, "low-vision");
    expect(lowVision).toMatchObject({
      textScale: "large",
      codeFontSize: 16,
      highContrastEnabled: true,
      focusVisibility: "enhanced",
    });
    expect(DEFAULT_APPEARANCE_SETTINGS.textScale).toBe("medium");
  });
});
