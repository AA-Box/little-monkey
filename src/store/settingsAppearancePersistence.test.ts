import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { DEFAULT_APPEARANCE_SETTINGS, type AppearanceSettings } from "../lib/theme";

const SETTINGS_STORAGE_KEY = "little-monkey-automation-settings";

function memoryStorage(): Storage {
  const values = new Map<string, string>();
  return {
    get length() { return values.size; },
    clear: () => values.clear(),
    getItem: (key) => values.get(key) ?? null,
    key: (index) => [...values.keys()][index] ?? null,
    removeItem: (key) => { values.delete(key); },
    setItem: (key, value) => { values.set(key, value); },
  };
}

const SAVED_APPEARANCE: AppearanceSettings = {
  themePreference: "dark",
  accentColor: "amber",
  textScale: "large",
  codeFontSize: 16,
  uiDensity: "spacious",
  sidebarLayout: "wide",
  chatBubbleStyle: "compact",
  motionPreference: "reduced",
  highContrastEnabled: true,
  focusVisibility: "enhanced",
};

describe("settingsStore appearance persistence", () => {
  beforeEach(() => {
    vi.stubGlobal("localStorage", memoryStorage());
    vi.resetModules();
  });

  afterEach(() => {
    vi.unstubAllGlobals();
    vi.resetModules();
  });

  it("restores device defaults and workspace overrides after a module restart", async () => {
    const first = await import("./settingsStore");
    first.useSettingsStore.getState().setDeviceAppearance(SAVED_APPEARANCE);
    first.useSettingsStore.getState().setWorkspaceAppearanceOverride("/workspace/app", {
      accentColor: "rose",
      sidebarLayout: "compact",
    });

    vi.resetModules();
    const restarted = await import("./settingsStore");
    expect(restarted.useSettingsStore.getState().deviceAppearance).toEqual(SAVED_APPEARANCE);
    expect(restarted.useSettingsStore.getState().appearanceWorkspaceOverrides).toEqual({
      "/workspace/app": { accentColor: "rose", sidebarLayout: "compact" },
    });
  });

  it("migrates the original flat appearance fields into the nested device profile", async () => {
    localStorage.setItem(SETTINGS_STORAGE_KEY, JSON.stringify({
      themePreference: "light",
      accentColor: "blue",
      textScale: "small",
      motionPreference: "reduced",
      highContrastEnabled: true,
    }));

    const migrated = await import("./settingsStore");
    expect(migrated.useSettingsStore.getState().deviceAppearance).toEqual({
      ...DEFAULT_APPEARANCE_SETTINGS,
      themePreference: "light",
      accentColor: "blue",
      textScale: "small",
      motionPreference: "reduced",
      highContrastEnabled: true,
    });
    expect(migrated.useSettingsStore.getState().appearanceProfileVersion).toBe(1);
  });
});
