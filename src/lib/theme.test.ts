import { afterEach, describe, expect, it, vi } from "vitest";

import { applyAppearance, THEME_STORAGE_KEY } from "./theme";

describe("applyAppearance", () => {
  afterEach(() => vi.unstubAllGlobals());

  it("applies every appearance choice to stable root attributes and CSS variables", () => {
    const attributes = new Map<string, string>();
    const properties = new Map<string, string>();
    const stored = new Map<string, string>();
    vi.stubGlobal("document", {
      documentElement: {
        getAttribute: (name: string) => attributes.get(name) ?? null,
        setAttribute: (name: string, value: string) => { attributes.set(name, value); },
        style: { setProperty: (name: string, value: string) => { properties.set(name, value); } },
      },
    });
    vi.stubGlobal("window", { matchMedia: () => ({ matches: false }) });
    vi.stubGlobal("localStorage", {
      getItem: (key: string) => stored.get(key) ?? null,
      setItem: (key: string, value: string) => { stored.set(key, value); },
    });

    applyAppearance({
      themePreference: "dark",
      accentColor: "rose",
      textScale: "large",
      codeFontSize: 16,
      uiDensity: "spacious",
      sidebarLayout: "wide",
      chatBubbleStyle: "flat",
      motionPreference: "reduced",
      highContrastEnabled: true,
      focusVisibility: "enhanced",
    });

    expect(Object.fromEntries(attributes)).toMatchObject({
      "data-theme": "dark",
      "data-theme-preference": "dark",
      "data-accent": "rose",
      "data-text-scale": "large",
      "data-code-font-size": "16",
      "data-ui-density": "spacious",
      "data-sidebar-layout": "wide",
      "data-chat-bubble-style": "flat",
      "data-motion": "reduced",
      "data-contrast": "high",
      "data-focus-visibility": "enhanced",
    });
    expect(properties.get("--code-font-size")).toBe("16px");
    expect(stored.get(THEME_STORAGE_KEY)).toBe("dark");
  });

  it("does not persist the legacy theme fallback while previewing a draft", () => {
    const setItem = vi.fn();
    vi.stubGlobal("document", {
      documentElement: {
        setAttribute: vi.fn(),
        style: { setProperty: vi.fn() },
      },
    });
    vi.stubGlobal("window", { matchMedia: () => ({ matches: false }) });
    vi.stubGlobal("localStorage", { getItem: vi.fn(() => null), setItem });

    applyAppearance({ themePreference: "dark" }, { persistThemePreference: false });
    expect(setItem).not.toHaveBeenCalled();
  });
});
