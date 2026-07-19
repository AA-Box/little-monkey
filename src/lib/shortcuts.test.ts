import { describe, expect, it } from "vitest";

import {
  defaultShortcutBindings,
  detectShortcutPlatform,
  effectiveShortcutBindings,
  findShortcutConflict,
  formatShortcutBinding,
  formatShortcutAriaLabel,
  matchesShortcut,
  sanitizeShortcutBinding,
  shortcutBindingFromEvent,
  shortcutBindingSeparator,
  shortcutBindingsConflict,
  shortcutById,
  shortcutDisplayLabel,
  shortcutIdForEvent,
  shortcutMatchesQuery,
  SHORTCUT_GROUPS,
  SHORTCUTS,
  shouldHandleGlobalShortcut,
  usesMacShortcuts,
  validateShortcutBinding,
  type ShortcutOverrides,
} from "./shortcuts";
import { en } from "./i18n/locales/en";

type ShortcutEvent = Parameters<typeof shortcutIdForEvent>[0];

function keyboardEvent(overrides: Partial<ShortcutEvent> = {}): ShortcutEvent {
  return {
    key: "",
    code: "",
    metaKey: false,
    ctrlKey: false,
    altKey: false,
    shiftKey: false,
    ...overrides,
  };
}

describe("shortcut registry", () => {
  it("has unique ids and complete searchable metadata", () => {
    const ids = SHORTCUTS.map((shortcut) => shortcut.id);
    expect(new Set(ids).size).toBe(ids.length);
    for (const shortcut of SHORTCUTS) {
      expect(shortcut.labelKey).not.toBe("");
      expect(shortcut.descriptionKey).not.toBe("");
      expect(shortcut.bindings.length).toBeGreaterThan(0);
      expect(en[shortcut.labelKey], `missing English label ${shortcut.labelKey}`).toBeTruthy();
      expect(en[shortcut.descriptionKey], `missing English description ${shortcut.descriptionKey}`).toBeTruthy();
    }
  });

  it("represents every scope once in the group metadata", () => {
    const groupIds = SHORTCUT_GROUPS.map((group) => group.id);
    expect(new Set(groupIds).size).toBe(groupIds.length);
    expect(new Set(groupIds)).toEqual(new Set(SHORTCUTS.map((shortcut) => shortcut.scope)));
    for (const group of SHORTCUT_GROUPS) {
      expect(en[group.labelKey], `missing English group label ${group.labelKey}`).toBeTruthy();
    }
  });

  it("has no duplicate chord within a scope on any platform", () => {
    for (const platform of ["macos", "windows", "linux"] as const) {
      for (const group of SHORTCUT_GROUPS) {
        const seen = new Set<string>();
        for (const shortcut of SHORTCUTS.filter((entry) => entry.scope === group.id)) {
          for (const binding of defaultShortcutBindings(shortcut, platform)) {
            const chord = formatShortcutBinding(binding, platform).join("+");
            expect(seen.has(chord), `${group.id} contains duplicate ${chord}`).toBe(false);
            seen.add(chord);
          }
        }
      }
    }
  });
});

describe("shortcut matching", () => {
  it("maps the primary modifier to Command on macOS", () => {
    expect(
      shortcutIdForEvent(keyboardEvent({ key: "n", code: "KeyN", metaKey: true }), "global", true),
    ).toBe("newSession");
    expect(
      shortcutIdForEvent(keyboardEvent({ key: "n", code: "KeyN", ctrlKey: true }), "global", true),
    ).toBeNull();
  });

  it("maps the primary modifier to Control on Windows and Linux", () => {
    expect(
      shortcutIdForEvent(keyboardEvent({ key: ",", code: "Comma", ctrlKey: true }), "global", false),
    ).toBe("openSettings");
    expect(
      shortcutIdForEvent(keyboardEvent({ key: ",", code: "Comma", metaKey: true }), "global", false),
    ).toBeNull();
  });

  it("requires the exact modifiers for a chord", () => {
    const newSession = shortcutById("newSession").bindings[0];
    expect(matchesShortcut(keyboardEvent({ key: "n", metaKey: true }), newSession, true)).toBe(true);
    expect(matchesShortcut(keyboardEvent({ key: "n" }), newSession, true)).toBe(false);
    expect(matchesShortcut(keyboardEvent({ key: "n", metaKey: true, shiftKey: true }), newSession, true)).toBe(false);
    expect(matchesShortcut(keyboardEvent({ key: "n", metaKey: true, altKey: true }), newSession, true)).toBe(false);
  });

  it("matches the conventional question-mark shortcut", () => {
    expect(
      shortcutIdForEvent(
        keyboardEvent({ key: "?", code: "Slash", metaKey: true, shiftKey: true }),
        "global",
        true,
      ),
    ).toBe("openShortcuts");
    expect(
      shortcutIdForEvent(
        keyboardEvent({ key: "?", code: "Slash", metaKey: true }),
        "global",
        true,
      ),
    ).toBe("openShortcuts");
  });

  it("uses the slash default on Windows and Linux", () => {
    for (const platform of ["windows", "linux"] as const) {
      expect(
        shortcutIdForEvent(
          keyboardEvent({ key: "/", code: "Slash", ctrlKey: true }),
          "global",
          platform,
        ),
      ).toBe("openShortcuts");
      expect(
        shortcutIdForEvent(
          keyboardEvent({ key: "?", code: "Slash", ctrlKey: true, shiftKey: true }),
          "global",
          platform,
        ),
      ).toBeNull();
    }
  });

  it("matches the displayed logical key instead of a QWERTY physical position", () => {
    expect(
      shortcutIdForEvent(keyboardEvent({ key: "q", code: "KeyN", metaKey: true }), "global", true),
    ).toBeNull();
    expect(
      shortcutIdForEvent(keyboardEvent({ key: "n", code: "KeyQ", metaKey: true }), "global", true),
    ).toBe("newSession");
  });

  it("supports alternative keys and keeps scopes separate", () => {
    expect(shortcutIdForEvent(keyboardEvent({ key: "Enter" }), "suggestions", true)).toBe("chooseSuggestion");
    expect(shortcutIdForEvent(keyboardEvent({ key: "Tab" }), "suggestions", true)).toBe("chooseSuggestion");
    expect(shortcutIdForEvent(keyboardEvent({ key: "Enter" }), "composer", true)).toBe("sendMessage");
    expect(shortcutIdForEvent(keyboardEvent({ key: "Enter", shiftKey: true }), "composer", true)).toBe("insertLineBreak");
  });

  it("uses effective overrides immediately and never lets an empty override disable an action", () => {
    const overrides: ShortcutOverrides = {
      newSession: [{ key: "k", code: "KeyK", primary: true }],
      sendMessage: [],
    };
    expect(
      shortcutIdForEvent(keyboardEvent({ key: "n", metaKey: true }), "global", true, overrides),
    ).toBeNull();
    expect(
      shortcutIdForEvent(keyboardEvent({ key: "k", metaKey: true }), "global", true, overrides),
    ).toBe("newSession");
    expect(effectiveShortcutBindings(shortcutById("sendMessage"), overrides)).toEqual([{ key: "Enter" }]);
  });

  it("allows layout Shift when it is required to produce a logical number key", () => {
    expect(
      shortcutIdForEvent(keyboardEvent({ key: "1", code: "Digit1", shiftKey: true }), "sessionMenu", true),
    ).toBe("sessionOpenSplit");
    expect(
      shortcutIdForEvent(keyboardEvent({ key: "N", code: "KeyN", metaKey: true, shiftKey: true }), "global", true),
    ).toBeNull();
  });

  it("does not treat modified session-menu letters as bare mnemonics", () => {
    expect(shortcutIdForEvent(keyboardEvent({ key: "d", code: "KeyD" }), "sessionMenu", true)).toBe("sessionDelete");
    expect(
      shortcutIdForEvent(keyboardEvent({ key: "d", code: "KeyD", metaKey: true }), "sessionMenu", true),
    ).toBeNull();
  });

  it("gates global commands during repeats, composition, handled events, and permissions", () => {
    const event = {
      ...keyboardEvent({ key: "n", metaKey: true }),
      defaultPrevented: false,
      repeat: false,
      isComposing: false,
    };
    expect(shouldHandleGlobalShortcut(event, false)).toBe(true);
    expect(shouldHandleGlobalShortcut({ ...event, repeat: true }, false)).toBe(false);
    expect(shouldHandleGlobalShortcut({ ...event, isComposing: true }, false)).toBe(false);
    expect(shouldHandleGlobalShortcut({ ...event, defaultPrevented: true }, false)).toBe(false);
    expect(shouldHandleGlobalShortcut(event, true)).toBe(false);
    expect(shouldHandleGlobalShortcut(event, false, true)).toBe(false);
  });
});

describe("shortcut recording and validation", () => {
  it("captures portable primary and explicit platform modifiers", () => {
    expect(
      shortcutBindingFromEvent(keyboardEvent({ key: "K", code: "KeyK", metaKey: true }), true),
    ).toEqual({ key: "k", code: "KeyK", primary: true });
    expect(
      shortcutBindingFromEvent(keyboardEvent({ key: "k", code: "KeyK", ctrlKey: true }), true),
    ).toEqual({ key: "k", code: "KeyK", control: true });
    expect(
      shortcutBindingFromEvent(keyboardEvent({ key: "k", code: "KeyK", ctrlKey: true }), false),
    ).toEqual({ key: "k", code: "KeyK", primary: true });
    expect(
      shortcutBindingFromEvent(keyboardEvent({ key: "k", code: "KeyK", metaKey: true }), false),
    ).toEqual({ key: "k", code: "KeyK", meta: true });
  });

  it("ignores modifier/dead input and normalizes layout Shift", () => {
    expect(shortcutBindingFromEvent(keyboardEvent({ key: "Shift", shiftKey: true }), true)).toBeNull();
    expect(shortcutBindingFromEvent(keyboardEvent({ key: "Dead" }), true)).toBeNull();
    expect(shortcutBindingFromEvent(keyboardEvent({ key: "Process" }), true)).toBeNull();
    expect(shortcutBindingFromEvent(keyboardEvent({ key: "Unidentified" }), true)).toBeNull();
    expect(
      shortcutBindingFromEvent(
        {
          ...keyboardEvent({ key: "€", ctrlKey: true, altKey: true }),
          getModifierState: (modifier) => modifier === "AltGraph",
        },
        false,
      ),
    ).toBeNull();
    expect(
      matchesShortcut(
        {
          ...keyboardEvent({ key: "€", ctrlKey: true, altKey: true }),
          getModifierState: (modifier) => modifier === "AltGraph",
        },
        { key: "€", primary: true, alt: true },
        false,
      ),
    ).toBe(false);
    expect(shortcutBindingFromEvent(keyboardEvent({ key: "?", code: "Slash", shiftKey: true }), true)).toEqual({
      key: "?",
      code: "Slash",
    });
    expect(shortcutBindingFromEvent(keyboardEvent({ key: "K", code: "KeyK", shiftKey: true }), true)).toEqual({
      key: "k",
      code: "KeyK",
      shift: true,
    });
  });

  it("treats Unicode letters like letters when normalizing Shift", () => {
    const binding = shortcutBindingFromEvent(
      keyboardEvent({ key: "Å", code: "BracketLeft", metaKey: true, shiftKey: true }),
      true,
    );
    expect(binding).toEqual({
      key: "å",
      code: "BracketLeft",
      primary: true,
      shift: true,
    });
    expect(
      matchesShortcut(
        keyboardEvent({ key: "å", code: "BracketLeft", metaKey: true }),
        binding as NonNullable<typeof binding>,
        true,
      ),
    ).toBe(false);
    expect(
      matchesShortcut(
        keyboardEvent({ key: "Å", code: "BracketLeft", metaKey: true, shiftKey: true }),
        binding as NonNullable<typeof binding>,
        true,
      ),
    ).toBe(true);
  });

  it("sanitizes malformed persisted input", () => {
    expect(sanitizeShortcutBinding({ key: "Dead" })).toBeNull();
    expect(sanitizeShortcutBinding({ key: "  K  ", shift: true, primary: true, unexpected: true })).toEqual({
      key: "k",
      primary: true,
      shift: true,
    });
    expect(sanitizeShortcutBinding({ key: "?", shift: true })).toEqual({ key: "?" });
  });

  it("rejects unsafe exact chords without blocking useful variants", () => {
    expect(validateShortcutBinding("newSession", { key: "k" }, true)).toBe("globalNeedsModifier");
    expect(validateShortcutBinding("sendMessage", { key: "k" }, true)).toBe("typingKey");
    expect(validateShortcutBinding("newSession", { key: "c", primary: true }, true)).toBe("reserved");
    expect(validateShortcutBinding("newSession", { key: "c", primary: true, shift: true }, true)).toBeNull();
    expect(validateShortcutBinding("newSession", { key: "z", primary: true, shift: true }, true)).toBe("reserved");
    expect(validateShortcutBinding("newSession", { key: "F4", alt: true }, false)).toBe("reserved");
    expect(validateShortcutBinding("newSession", { key: "F4", alt: true, shift: true }, false)).toBe("reserved");
    expect(validateShortcutBinding("newSession", { key: "k", meta: true }, "windows")).toBe("reserved");
    expect(validateShortcutBinding("newSession", { key: "k", meta: true }, "linux")).toBeNull();
  });

  it("detects same-scope/global conflicts but permits contextual scope reuse", () => {
    const overrides: ShortcutOverrides = {
      openSettings: [{ key: "k", primary: true }],
      sendMessage: [{ key: "F8" }],
      sessionOpenSplit: [{ key: "F9" }],
    };
    expect(findShortcutConflict("newSession", { key: "k", primary: true }, overrides)).toBe("openSettings");
    expect(findShortcutConflict("insertLineBreak", { key: "F8" }, overrides)).toBe("sendMessage");
    expect(findShortcutConflict("previousSuggestion", { key: "F8" }, overrides)).toBeNull();
    expect(findShortcutConflict("nextSuggestion", { key: "F9" }, overrides)).toBeNull();
    expect(findShortcutConflict("sessionCloseMenu", { key: "Escape" }, overrides)).toBeNull();
  });

  it("detects primary aliases on the active platform only", () => {
    expect(shortcutBindingsConflict({ key: "k", primary: true }, { key: "k", meta: true }, "macos")).toBe(true);
    expect(shortcutBindingsConflict({ key: "k", primary: true }, { key: "k", meta: true }, "windows")).toBe(false);
    expect(shortcutBindingsConflict({ key: "k", primary: true }, { key: "k", meta: true }, "linux")).toBe(false);
    expect(shortcutBindingsConflict({ key: "k", primary: true }, { key: "k", control: true }, "macos")).toBe(false);
    expect(shortcutBindingsConflict({ key: "k", primary: true }, { key: "k", control: true }, "windows")).toBe(true);
    expect(shortcutBindingsConflict({ key: "k", primary: true }, { key: "k", control: true }, "linux")).toBe(true);
    expect(
      shortcutBindingsConflict(
        { key: "k", primary: true, shift: true },
        { key: "k", control: true },
        "windows",
      ),
    ).toBe(false);
  });
});

describe("shortcut display and search", () => {
  it("formats platform-native modifier labels", () => {
    const shortcut = shortcutById("openShortcuts");
    const macOpenShortcuts = defaultShortcutBindings(shortcut, "macos")[0];
    const windowsOpenShortcuts = defaultShortcutBindings(shortcut, "windows")[0];
    expect(formatShortcutBinding(macOpenShortcuts, "macos")).toEqual(["⌘", "?"]);
    expect(formatShortcutBinding(windowsOpenShortcuts, "windows")).toEqual(["Ctrl", "/"]);
    expect(formatShortcutBinding(shortcutById("nextSuggestion").bindings[0], true)).toEqual(["↓"]);
    expect(formatShortcutAriaLabel(macOpenShortcuts, "macos")).toBe("Command plus Question mark");
    expect(formatShortcutAriaLabel(shortcutById("insertLineBreak").bindings[0], false)).toBe("Shift plus Enter");
    expect(formatShortcutBinding({ key: "k", meta: true }, "windows")).toEqual(["Win", "K"]);
    expect(formatShortcutBinding({ key: "k", meta: true }, "linux")).toEqual(["Super", "K"]);
    expect(formatShortcutAriaLabel({ key: "k", meta: true }, "windows")).toBe("Windows plus K");
    expect(formatShortcutAriaLabel({ key: "k", meta: true }, "linux")).toBe("Super plus K");
    expect(shortcutBindingSeparator("macos")).toBe("");
    expect(shortcutBindingSeparator("windows")).toBe("+");
    expect(shortcutBindingSeparator("linux")).toBe("+");
  });

  it("detects macOS, Windows, and Linux explicitly", () => {
    expect(detectShortcutPlatform("MacIntel")).toBe("macos");
    expect(detectShortcutPlatform("iPhone")).toBe("macos");
    expect(detectShortcutPlatform("Win32")).toBe("windows");
    expect(detectShortcutPlatform("Windows NT 10.0")).toBe("windows");
    expect(detectShortcutPlatform("Linux x86_64")).toBe("linux");
    expect(detectShortcutPlatform("X11")).toBe("linux");
    expect(usesMacShortcuts("MacIntel")).toBe(true);
    expect(usesMacShortcuts("iPhone")).toBe(true);
    expect(usesMacShortcuts("Win32")).toBe(false);
    expect(usesMacShortcuts("Linux x86_64")).toBe(false);
  });

  it("searches translated labels, descriptions, and rendered keys", () => {
    const shortcut = shortcutById("toggleWorkspacePanel");
    const translate = (key: string) =>
      key.endsWith("toggleWorkspacePanel") ? "Toggle workspace panel" : "Show or hide files";

    expect(shortcutMatchesQuery(shortcut, "workspace", translate, false)).toBe(true);
    expect(shortcutMatchesQuery(shortcut, "files", translate, false)).toBe(true);
    expect(shortcutMatchesQuery(shortcut, "ctrl", translate, false)).toBe(true);
    expect(shortcutMatchesQuery(shortcut, "does-not-exist", translate, false)).toBe(false);

    const macShortcut = shortcutById("openShortcuts");
    expect(shortcutMatchesQuery(macShortcut, "command", translate, true)).toBe(true);
    expect(shortcutMatchesQuery(macShortcut, "slash", translate, true)).toBe(true);
    expect(shortcutMatchesQuery(macShortcut, "slash", translate, "windows")).toBe(true);
  });

  it("renders and searches effective user bindings", () => {
    const overrides: ShortcutOverrides = { newSession: [{ key: "k", primary: true, shift: true }] };
    const shortcut = shortcutById("newSession");
    const translate = () => "Start conversation";
    expect(shortcutDisplayLabel("newSession", false, overrides)).toBe("Ctrl+Shift+K");
    expect(shortcutMatchesQuery(shortcut, "shift", translate, false, overrides)).toBe(true);
    expect(shortcutMatchesQuery(shortcut, "ctrl n", translate, false, overrides)).toBe(false);
  });
});
