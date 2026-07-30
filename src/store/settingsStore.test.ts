import { beforeEach, describe, expect, it, vi } from "vitest";

import { DEFAULT_APPEARANCE_SETTINGS, THEME_STORAGE_KEY } from "../lib/theme";
import {
  normalizeProviderModelSelection,
  STORAGE_KEY,
  useSettingsStore,
} from "./settingsStore";

describe("settingsStore.providerModelFilters", () => {
  beforeEach(() => {
    useSettingsStore.setState({ providerModelFilters: {} });
  });

  it("normalizes selections to available ids and recognizes a complete selection", () => {
    expect(
      normalizeProviderModelSelection(
        ["model-b", "missing", "model-a", "model-a"],
        ["model-a", "model-b"],
      ),
    ).toEqual({
      showAll: true,
      selectedModelIds: ["model-a", "model-b"],
    });
  });

  it("selects every available model in one persisted filter update", () => {
    useSettingsStore
      .getState()
      .setProviderModelSelection(
        "openrouter",
        ["model-a", "model-b"],
        ["model-a", "model-b"],
      );

    expect(useSettingsStore.getState().providerModelFilters.openrouter).toEqual({
      showAll: true,
      selectedModelIds: ["model-a", "model-b"],
    });
  });

  it("unchecking one model materializes the implicit show-all selection", () => {
    useSettingsStore
      .getState()
      .toggleProviderModelSelected(
        "openrouter",
        "model-a",
        ["model-a", "model-b", "model-c"],
      );

    expect(useSettingsStore.getState().providerModelFilters.openrouter).toEqual({
      showAll: false,
      selectedModelIds: ["model-b", "model-c"],
    });
  });

  it("rechecking the last missing model makes show-all truthful again", () => {
    useSettingsStore.setState({
      providerModelFilters: {
        openrouter: {
          showAll: false,
          selectedModelIds: ["model-b", "model-c"],
        },
      },
    });

    useSettingsStore
      .getState()
      .toggleProviderModelSelected(
        "openrouter",
        "model-a",
        ["model-a", "model-b", "model-c"],
      );

    expect(useSettingsStore.getState().providerModelFilters.openrouter).toEqual({
      showAll: true,
      selectedModelIds: ["model-a", "model-b", "model-c"],
    });
  });

  it("clears both the all-model flag and every selected id", () => {
    useSettingsStore.setState({
      providerModelFilters: {
        openrouter: {
          showAll: true,
          selectedModelIds: ["model-a", "model-b"],
        },
      },
    });

    useSettingsStore.getState().clearProviderModelSelection("openrouter");

    expect(useSettingsStore.getState().providerModelFilters.openrouter).toEqual({
      showAll: false,
      selectedModelIds: [],
    });
  });
});

describe("settingsStore.checkpointRetention", () => {
  beforeEach(() => {
    useSettingsStore.setState({ checkpointRetention: 20 });
  });

  it("defaults to 20 when nothing is persisted", async () => {
    // Exercises the real default-hydration path (`hydrate()`/`defaults()`)
    // instead of asserting against state this suite's own `beforeEach` just
    // set by hand — a fresh module instance with no persisted blob is the
    // only way to actually cover that code path. Without `resetModules` +
    // a dynamic re-import, this test would pass even if
    // `DEFAULT_CHECKPOINT_RETENTION` were changed to something else,
    // because `beforeEach` would still be forcing the value to 20.
    // (This suite runs under vitest's `node` environment, which has no
    // `localStorage` global at all — guarded rather than assumed, since
    // `hydrate()` itself tolerates that via its own try/catch.)
    if (typeof localStorage !== "undefined") {
      localStorage.removeItem(STORAGE_KEY);
    }
    vi.resetModules();
    const fresh = await import("./settingsStore");
    expect(fresh.useSettingsStore.getState().checkpointRetention).toBe(20);
  });

  it("clamps below the 5-checkpoint floor", () => {
    useSettingsStore.getState().setCheckpointRetention(0);
    expect(useSettingsStore.getState().checkpointRetention).toBe(5);
  });

  it("clamps above the 100-checkpoint ceiling", () => {
    useSettingsStore.getState().setCheckpointRetention(500);
    expect(useSettingsStore.getState().checkpointRetention).toBe(100);
  });

  it("rounds fractional input", () => {
    useSettingsStore.getState().setCheckpointRetention(42.6);
    expect(useSettingsStore.getState().checkpointRetention).toBe(43);
  });

  it("accepts an in-range value unchanged", () => {
    useSettingsStore.getState().setCheckpointRetention(50);
    expect(useSettingsStore.getState().checkpointRetention).toBe(50);
  });
});

describe("settingsStore.memoryEnabled", () => {
  beforeEach(() => {
    useSettingsStore.setState({ memoryEnabled: true });
  });

  it("defaults to true when nothing is persisted", async () => {
    // Same "exercise the real hydration path" rationale as the
    // checkpointRetention default test above — `beforeEach` forces `true`
    // regardless, so only a fresh module import actually covers `defaults()`.
    if (typeof localStorage !== "undefined") {
      localStorage.removeItem(STORAGE_KEY);
    }
    vi.resetModules();
    const fresh = await import("./settingsStore");
    expect(fresh.useSettingsStore.getState().memoryEnabled).toBe(true);
  });

  it("toggles off and on", () => {
    useSettingsStore.getState().setMemoryEnabled(false);
    expect(useSettingsStore.getState().memoryEnabled).toBe(false);
    useSettingsStore.getState().setMemoryEnabled(true);
    expect(useSettingsStore.getState().memoryEnabled).toBe(true);
  });

  it("persists across a hydrate() reload", async () => {
    // Guarded like the sibling tests above/below — this suite runs under
    // vitest's `node` environment, which has no `localStorage` global, so
    // `persist()`'s best-effort write silently no-ops there.
    if (typeof localStorage === "undefined") return;
    useSettingsStore.getState().setMemoryEnabled(false);
    vi.resetModules();
    const fresh = await import("./settingsStore");
    expect(fresh.useSettingsStore.getState().memoryEnabled).toBe(false);
    localStorage.removeItem(STORAGE_KEY);
  });

  it("ignores a non-boolean persisted value and falls back to the default", async () => {
    if (typeof localStorage === "undefined") return;
    localStorage.setItem(STORAGE_KEY, JSON.stringify({ memoryEnabled: "nope" }));
    vi.resetModules();
    const fresh = await import("./settingsStore");
    expect(fresh.useSettingsStore.getState().memoryEnabled).toBe(true);
    localStorage.removeItem(STORAGE_KEY);
  });
});

describe("settingsStore.webToolsEnabled", () => {
  beforeEach(() => {
    useSettingsStore.setState({ webToolsEnabled: true });
  });

  it("defaults to true when nothing is persisted", async () => {
    // Same "exercise the real hydration path" rationale as memoryEnabled's
    // own default test above — `beforeEach` forces `true` regardless, so
    // only a fresh module import actually covers `defaults()`.
    if (typeof localStorage !== "undefined") {
      localStorage.removeItem(STORAGE_KEY);
    }
    vi.resetModules();
    const fresh = await import("./settingsStore");
    expect(fresh.useSettingsStore.getState().webToolsEnabled).toBe(true);
  });

  it("toggles off and on", () => {
    useSettingsStore.getState().setWebToolsEnabled(false);
    expect(useSettingsStore.getState().webToolsEnabled).toBe(false);
    useSettingsStore.getState().setWebToolsEnabled(true);
    expect(useSettingsStore.getState().webToolsEnabled).toBe(true);
  });

  it("persists across a hydrate() reload", async () => {
    if (typeof localStorage === "undefined") return;
    useSettingsStore.getState().setWebToolsEnabled(false);
    vi.resetModules();
    const fresh = await import("./settingsStore");
    expect(fresh.useSettingsStore.getState().webToolsEnabled).toBe(false);
    localStorage.removeItem(STORAGE_KEY);
  });

  it("ignores a non-boolean persisted value and falls back to the default", async () => {
    if (typeof localStorage === "undefined") return;
    localStorage.setItem(STORAGE_KEY, JSON.stringify({ webToolsEnabled: "nope" }));
    vi.resetModules();
    const fresh = await import("./settingsStore");
    expect(fresh.useSettingsStore.getState().webToolsEnabled).toBe(true);
    localStorage.removeItem(STORAGE_KEY);
  });
});

describe("settingsStore.verifyEnabled", () => {
  beforeEach(() => {
    useSettingsStore.setState({ verifyEnabled: false });
  });

  it("defaults to false when nothing is persisted", async () => {
    // Same "exercise the real hydration path" rationale as memoryEnabled's
    // own default test above — `beforeEach` forces `false` regardless, so
    // only a fresh module import actually covers `defaults()`. Unlike
    // memoryEnabled/webToolsEnabled, this one defaults OFF: running
    // arbitrary configured shell automatically should be opt-in.
    if (typeof localStorage !== "undefined") {
      localStorage.removeItem(STORAGE_KEY);
    }
    vi.resetModules();
    const fresh = await import("./settingsStore");
    expect(fresh.useSettingsStore.getState().verifyEnabled).toBe(false);
  });

  it("toggles off and on", () => {
    useSettingsStore.getState().setVerifyEnabled(true);
    expect(useSettingsStore.getState().verifyEnabled).toBe(true);
    useSettingsStore.getState().setVerifyEnabled(false);
    expect(useSettingsStore.getState().verifyEnabled).toBe(false);
  });

  it("persists across a hydrate() reload", async () => {
    if (typeof localStorage === "undefined") return;
    useSettingsStore.getState().setVerifyEnabled(true);
    vi.resetModules();
    const fresh = await import("./settingsStore");
    expect(fresh.useSettingsStore.getState().verifyEnabled).toBe(true);
    localStorage.removeItem(STORAGE_KEY);
  });

  it("ignores a non-boolean persisted value and falls back to the default", async () => {
    if (typeof localStorage === "undefined") return;
    localStorage.setItem(STORAGE_KEY, JSON.stringify({ verifyEnabled: "nope" }));
    vi.resetModules();
    const fresh = await import("./settingsStore");
    expect(fresh.useSettingsStore.getState().verifyEnabled).toBe(false);
    localStorage.removeItem(STORAGE_KEY);
  });
});

describe("settingsStore.verifyMaxRounds", () => {
  beforeEach(() => {
    useSettingsStore.setState({ verifyMaxRounds: 1 });
  });

  it("defaults to 1 when nothing is persisted", async () => {
    // Same "exercise the real hydration path" rationale as checkpointRetention's
    // own default test above — `beforeEach` forces `1` regardless, so only a
    // fresh module import actually covers `defaults()`.
    if (typeof localStorage !== "undefined") {
      localStorage.removeItem(STORAGE_KEY);
    }
    vi.resetModules();
    const fresh = await import("./settingsStore");
    expect(fresh.useSettingsStore.getState().verifyMaxRounds).toBe(1);
  });

  it("clamps below the 0-round floor", () => {
    useSettingsStore.getState().setVerifyMaxRounds(-1);
    expect(useSettingsStore.getState().verifyMaxRounds).toBe(0);
  });

  it("clamps above the 3-round ceiling", () => {
    useSettingsStore.getState().setVerifyMaxRounds(10);
    expect(useSettingsStore.getState().verifyMaxRounds).toBe(3);
  });

  it("rounds fractional input", () => {
    useSettingsStore.getState().setVerifyMaxRounds(2.4);
    expect(useSettingsStore.getState().verifyMaxRounds).toBe(2);
  });

  it("accepts an in-range value unchanged", () => {
    useSettingsStore.getState().setVerifyMaxRounds(2);
    expect(useSettingsStore.getState().verifyMaxRounds).toBe(2);
  });

  it("persists across a hydrate() reload", async () => {
    if (typeof localStorage === "undefined") return;
    useSettingsStore.getState().setVerifyMaxRounds(3);
    vi.resetModules();
    const fresh = await import("./settingsStore");
    expect(fresh.useSettingsStore.getState().verifyMaxRounds).toBe(3);
    localStorage.removeItem(STORAGE_KEY);
  });

  it("ignores an out-of-range persisted value and falls back to the default", async () => {
    if (typeof localStorage === "undefined") return;
    localStorage.setItem(STORAGE_KEY, JSON.stringify({ verifyMaxRounds: 99 }));
    vi.resetModules();
    const fresh = await import("./settingsStore");
    expect(fresh.useSettingsStore.getState().verifyMaxRounds).toBe(1);
    localStorage.removeItem(STORAGE_KEY);
  });
});

describe("settingsStore.artifactScriptsEnabled", () => {
  beforeEach(() => {
    useSettingsStore.setState({ artifactScriptsEnabled: true });
  });

  it("defaults to true when nothing is persisted", async () => {
    // Same "exercise the real hydration path" rationale as webToolsEnabled's
    // own default test above — `beforeEach` forces `true` regardless, so
    // only a fresh module import actually covers `defaults()`.
    if (typeof localStorage !== "undefined") {
      localStorage.removeItem(STORAGE_KEY);
    }
    vi.resetModules();
    const fresh = await import("./settingsStore");
    expect(fresh.useSettingsStore.getState().artifactScriptsEnabled).toBe(true);
  });

  it("toggles off and on", () => {
    useSettingsStore.getState().setArtifactScriptsEnabled(false);
    expect(useSettingsStore.getState().artifactScriptsEnabled).toBe(false);
    useSettingsStore.getState().setArtifactScriptsEnabled(true);
    expect(useSettingsStore.getState().artifactScriptsEnabled).toBe(true);
  });

  it("persists across a hydrate() reload", async () => {
    if (typeof localStorage === "undefined") return;
    useSettingsStore.getState().setArtifactScriptsEnabled(false);
    vi.resetModules();
    const fresh = await import("./settingsStore");
    expect(fresh.useSettingsStore.getState().artifactScriptsEnabled).toBe(false);
    localStorage.removeItem(STORAGE_KEY);
  });

  it("ignores a non-boolean persisted value and falls back to the default", async () => {
    if (typeof localStorage === "undefined") return;
    localStorage.setItem(STORAGE_KEY, JSON.stringify({ artifactScriptsEnabled: "nope" }));
    vi.resetModules();
    const fresh = await import("./settingsStore");
    expect(fresh.useSettingsStore.getState().artifactScriptsEnabled).toBe(true);
    localStorage.removeItem(STORAGE_KEY);
  });
});

describe("settingsStore.artifactAutoPreview", () => {
  beforeEach(() => {
    useSettingsStore.setState({ artifactAutoPreview: false });
  });

  it("defaults to false when nothing is persisted", async () => {
    // Same "exercise the real hydration path" rationale as verifyEnabled's
    // own default test above — `beforeEach` forces `false` regardless, so
    // only a fresh module import actually covers `defaults()`. Defaults OFF:
    // auto-opening the workspace panel on the user's behalf should be
    // opt-in, mirroring verifyEnabled's posture.
    if (typeof localStorage !== "undefined") {
      localStorage.removeItem(STORAGE_KEY);
    }
    vi.resetModules();
    const fresh = await import("./settingsStore");
    expect(fresh.useSettingsStore.getState().artifactAutoPreview).toBe(false);
  });

  it("toggles off and on", () => {
    useSettingsStore.getState().setArtifactAutoPreview(true);
    expect(useSettingsStore.getState().artifactAutoPreview).toBe(true);
    useSettingsStore.getState().setArtifactAutoPreview(false);
    expect(useSettingsStore.getState().artifactAutoPreview).toBe(false);
  });

  it("persists across a hydrate() reload", async () => {
    if (typeof localStorage === "undefined") return;
    useSettingsStore.getState().setArtifactAutoPreview(true);
    vi.resetModules();
    const fresh = await import("./settingsStore");
    expect(fresh.useSettingsStore.getState().artifactAutoPreview).toBe(true);
    localStorage.removeItem(STORAGE_KEY);
  });

  it("ignores a non-boolean persisted value and falls back to the default", async () => {
    if (typeof localStorage === "undefined") return;
    localStorage.setItem(STORAGE_KEY, JSON.stringify({ artifactAutoPreview: "nope" }));
    vi.resetModules();
    const fresh = await import("./settingsStore");
    expect(fresh.useSettingsStore.getState().artifactAutoPreview).toBe(false);
    localStorage.removeItem(STORAGE_KEY);
  });
});

describe("settingsStore.subagentsEnabled", () => {
  beforeEach(() => {
    useSettingsStore.setState({ subagentsEnabled: false });
  });

  it("defaults to false when nothing is persisted", async () => {
    // Same "exercise the real hydration path" rationale as verifyEnabled's
    // own default test above — `beforeEach` forces `false` regardless, so
    // only a fresh module import actually covers `defaults()`. Defaults OFF,
    // same posture as `verifyEnabled`: a weak local model may misuse or loop
    // on the `task` tool, so delegation should be opt-in.
    if (typeof localStorage !== "undefined") {
      localStorage.removeItem(STORAGE_KEY);
    }
    vi.resetModules();
    const fresh = await import("./settingsStore");
    expect(fresh.useSettingsStore.getState().subagentsEnabled).toBe(false);
  });

  it("toggles off and on", () => {
    useSettingsStore.getState().setSubagentsEnabled(true);
    expect(useSettingsStore.getState().subagentsEnabled).toBe(true);
    useSettingsStore.getState().setSubagentsEnabled(false);
    expect(useSettingsStore.getState().subagentsEnabled).toBe(false);
  });

  it("persists across a hydrate() reload", async () => {
    if (typeof localStorage === "undefined") return;
    useSettingsStore.getState().setSubagentsEnabled(true);
    vi.resetModules();
    const fresh = await import("./settingsStore");
    expect(fresh.useSettingsStore.getState().subagentsEnabled).toBe(true);
    localStorage.removeItem(STORAGE_KEY);
  });

  it("ignores a non-boolean persisted value and falls back to the default", async () => {
    if (typeof localStorage === "undefined") return;
    localStorage.setItem(STORAGE_KEY, JSON.stringify({ subagentsEnabled: "nope" }));
    vi.resetModules();
    const fresh = await import("./settingsStore");
    expect(fresh.useSettingsStore.getState().subagentsEnabled).toBe(false);
    localStorage.removeItem(STORAGE_KEY);
  });
});

describe("settingsStore.skillAutoInvokeEnabled", () => {
  beforeEach(() => {
    useSettingsStore.setState({ skillAutoInvokeEnabled: false });
  });

  it("defaults to false when nothing is persisted", async () => {
    // Same posture as `subagentsEnabled`: the model acting on its own
    // initiative (here, invoking a skill without an explicit `/command`)
    // should be opt-in, not default-on.
    if (typeof localStorage !== "undefined") {
      localStorage.removeItem(STORAGE_KEY);
    }
    vi.resetModules();
    const fresh = await import("./settingsStore");
    expect(fresh.useSettingsStore.getState().skillAutoInvokeEnabled).toBe(false);
  });

  it("toggles off and on", () => {
    useSettingsStore.getState().setSkillAutoInvokeEnabled(true);
    expect(useSettingsStore.getState().skillAutoInvokeEnabled).toBe(true);
    useSettingsStore.getState().setSkillAutoInvokeEnabled(false);
    expect(useSettingsStore.getState().skillAutoInvokeEnabled).toBe(false);
  });

  it("persists across a hydrate() reload", async () => {
    if (typeof localStorage === "undefined") return;
    useSettingsStore.getState().setSkillAutoInvokeEnabled(true);
    vi.resetModules();
    const fresh = await import("./settingsStore");
    expect(fresh.useSettingsStore.getState().skillAutoInvokeEnabled).toBe(true);
    localStorage.removeItem(STORAGE_KEY);
  });

  it("ignores a non-boolean persisted value and falls back to the default", async () => {
    if (typeof localStorage === "undefined") return;
    localStorage.setItem(STORAGE_KEY, JSON.stringify({ skillAutoInvokeEnabled: "nope" }));
    vi.resetModules();
    const fresh = await import("./settingsStore");
    expect(fresh.useSettingsStore.getState().skillAutoInvokeEnabled).toBe(false);
    localStorage.removeItem(STORAGE_KEY);
  });
});

describe("settingsStore.desktopControlEnabled", () => {
  beforeEach(() => {
    useSettingsStore.setState({ desktopControlEnabled: false });
  });

  it("defaults to false when nothing is persisted", async () => {
    // Same "disabled = not offered" posture as `subagentsEnabled`/
    // `skillAutoInvokeEnabled` above: Safe Desktop Control (see
    // src-tauri/src/desktop_control.rs) is a research spike that can move
    // the real mouse/keyboard on macOS, so it defaults off regardless of
    // whatever else is persisted.
    if (typeof localStorage !== "undefined") {
      localStorage.removeItem(STORAGE_KEY);
    }
    vi.resetModules();
    const fresh = await import("./settingsStore");
    expect(fresh.useSettingsStore.getState().desktopControlEnabled).toBe(false);
  });

  it("toggles off and on", () => {
    useSettingsStore.getState().setDesktopControlEnabled(true);
    expect(useSettingsStore.getState().desktopControlEnabled).toBe(true);
    useSettingsStore.getState().setDesktopControlEnabled(false);
    expect(useSettingsStore.getState().desktopControlEnabled).toBe(false);
  });

  it("persists across a hydrate() reload", async () => {
    if (typeof localStorage === "undefined") return;
    useSettingsStore.getState().setDesktopControlEnabled(true);
    vi.resetModules();
    const fresh = await import("./settingsStore");
    expect(fresh.useSettingsStore.getState().desktopControlEnabled).toBe(true);
    localStorage.removeItem(STORAGE_KEY);
  });

  it("ignores a non-boolean persisted value and falls back to the default", async () => {
    if (typeof localStorage === "undefined") return;
    localStorage.setItem(STORAGE_KEY, JSON.stringify({ desktopControlEnabled: "nope" }));
    vi.resetModules();
    const fresh = await import("./settingsStore");
    expect(fresh.useSettingsStore.getState().desktopControlEnabled).toBe(false);
    localStorage.removeItem(STORAGE_KEY);
  });
});

describe("settingsStore.maxConcurrentSubagents", () => {
  beforeEach(() => {
    useSettingsStore.setState({ maxConcurrentSubagents: 2 });
  });

  it("defaults to 2 when nothing is persisted", async () => {
    // Same "exercise the real hydration path" rationale as verifyMaxRounds's
    // own default test above — `beforeEach` forces `2` regardless, so only a
    // fresh module import actually covers `defaults()`.
    if (typeof localStorage !== "undefined") {
      localStorage.removeItem(STORAGE_KEY);
    }
    vi.resetModules();
    const fresh = await import("./settingsStore");
    expect(fresh.useSettingsStore.getState().maxConcurrentSubagents).toBe(2);
  });

  it("clamps below the 1-subagent floor", () => {
    useSettingsStore.getState().setMaxConcurrentSubagents(0);
    expect(useSettingsStore.getState().maxConcurrentSubagents).toBe(1);
  });

  it("clamps above the 4-subagent ceiling", () => {
    useSettingsStore.getState().setMaxConcurrentSubagents(10);
    expect(useSettingsStore.getState().maxConcurrentSubagents).toBe(4);
  });

  it("rounds fractional input", () => {
    useSettingsStore.getState().setMaxConcurrentSubagents(3.4);
    expect(useSettingsStore.getState().maxConcurrentSubagents).toBe(3);
  });

  it("accepts an in-range value unchanged", () => {
    useSettingsStore.getState().setMaxConcurrentSubagents(3);
    expect(useSettingsStore.getState().maxConcurrentSubagents).toBe(3);
  });

  it("persists across a hydrate() reload", async () => {
    if (typeof localStorage === "undefined") return;
    useSettingsStore.getState().setMaxConcurrentSubagents(4);
    vi.resetModules();
    const fresh = await import("./settingsStore");
    expect(fresh.useSettingsStore.getState().maxConcurrentSubagents).toBe(4);
    localStorage.removeItem(STORAGE_KEY);
  });

  it("ignores an out-of-range persisted value and falls back to the default", async () => {
    if (typeof localStorage === "undefined") return;
    localStorage.setItem(STORAGE_KEY, JSON.stringify({ maxConcurrentSubagents: 99 }));
    vi.resetModules();
    const fresh = await import("./settingsStore");
    expect(fresh.useSettingsStore.getState().maxConcurrentSubagents).toBe(2);
    localStorage.removeItem(STORAGE_KEY);
  });
});

// Slice 4: optional per-profile model override — genuinely optional, so the
// default (empty map) must mean "no override for either profile", exactly
// what `subagent.ts`'s `resolveSubagentTarget` treats as "use the parent's
// own target unchanged".
describe("settingsStore.subagentProfileModels", () => {
  beforeEach(() => {
    useSettingsStore.setState({ subagentProfileModels: {} });
  });

  it("defaults to an empty map when nothing is persisted", async () => {
    if (typeof localStorage !== "undefined") {
      localStorage.removeItem(STORAGE_KEY);
    }
    vi.resetModules();
    const fresh = await import("./settingsStore");
    expect(fresh.useSettingsStore.getState().subagentProfileModels).toEqual({});
  });

  it("sets an override for one profile without touching the other", () => {
    useSettingsStore.getState().setSubagentProfileModel("explore", { providerId: "openrouter", model: "cheap-model" });
    expect(useSettingsStore.getState().subagentProfileModels).toEqual({
      explore: { providerId: "openrouter", model: "cheap-model" },
    });
    expect(useSettingsStore.getState().subagentProfileModels.code).toBeUndefined();
  });

  it("clears a previously-set override back to 'no override' for that profile", () => {
    useSettingsStore.getState().setSubagentProfileModel("code", { providerId: "anthropic", model: "claude" });
    useSettingsStore.getState().clearSubagentProfileModel("code");
    expect(useSettingsStore.getState().subagentProfileModels.code).toBeUndefined();
  });

  it("persists across a hydrate() reload", async () => {
    if (typeof localStorage === "undefined") return;
    useSettingsStore.getState().setSubagentProfileModel("explore", { providerId: "openrouter", model: "cheap-model" });
    vi.resetModules();
    const fresh = await import("./settingsStore");
    expect(fresh.useSettingsStore.getState().subagentProfileModels).toEqual({
      explore: { providerId: "openrouter", model: "cheap-model" },
    });
    localStorage.removeItem(STORAGE_KEY);
  });

  it("drops a malformed persisted entry (missing model) rather than corrupting the whole map", async () => {
    if (typeof localStorage === "undefined") return;
    localStorage.setItem(STORAGE_KEY, JSON.stringify({ subagentProfileModels: { explore: { providerId: "openrouter" }, code: { providerId: "x", model: "y" } } }));
    vi.resetModules();
    const fresh = await import("./settingsStore");
    expect(fresh.useSettingsStore.getState().subagentProfileModels).toEqual({ code: { providerId: "x", model: "y" } });
    localStorage.removeItem(STORAGE_KEY);
  });
});

describe("settingsStore.appearance", () => {
  beforeEach(() => {
    useSettingsStore.setState({
      appearanceProfileVersion: 1,
      deviceAppearance: { ...DEFAULT_APPEARANCE_SETTINGS },
      appearanceWorkspaceOverrides: {},
    });
  });

  it("defaults to the standard appearance when nothing is persisted", async () => {
    if (typeof localStorage !== "undefined") {
      localStorage.removeItem(STORAGE_KEY);
      localStorage.removeItem(THEME_STORAGE_KEY);
    }
    vi.resetModules();
    const fresh = await import("./settingsStore");
    expect(fresh.useSettingsStore.getState().deviceAppearance).toEqual({
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
    });
  });

  it("updates the complete device profile atomically", () => {
    const settings = useSettingsStore.getState();
    settings.setDeviceAppearance({
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
    });

    expect(useSettingsStore.getState().deviceAppearance).toEqual({
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
    });
  });

  it("persists appearance across a hydrate() reload", async () => {
    if (typeof localStorage === "undefined") return;
    useSettingsStore.getState().setDeviceAppearance({
      themePreference: "dark",
      accentColor: "rose",
      textScale: "small",
      codeFontSize: 12,
      uiDensity: "compact",
      sidebarLayout: "compact",
      chatBubbleStyle: "compact",
      motionPreference: "reduced",
      highContrastEnabled: true,
      focusVisibility: "enhanced",
    });

    vi.resetModules();
    const fresh = await import("./settingsStore");
    expect(fresh.useSettingsStore.getState().deviceAppearance).toEqual({
      themePreference: "dark",
      accentColor: "rose",
      textScale: "small",
      codeFontSize: 12,
      uiDensity: "compact",
      sidebarLayout: "compact",
      chatBubbleStyle: "compact",
      motionPreference: "reduced",
      highContrastEnabled: true,
      focusVisibility: "enhanced",
    });
    localStorage.removeItem(STORAGE_KEY);
    localStorage.removeItem(THEME_STORAGE_KEY);
  });

  it("falls back for malformed persisted appearance fields", async () => {
    if (typeof localStorage === "undefined") return;
    localStorage.setItem(
      STORAGE_KEY,
      JSON.stringify({
        themePreference: "midnight",
        accentColor: "ultraviolet",
        textScale: "giant",
        motionPreference: "spinny",
        highContrastEnabled: "yes",
      }),
    );
    localStorage.removeItem(THEME_STORAGE_KEY);

    vi.resetModules();
    const fresh = await import("./settingsStore");
    expect(fresh.useSettingsStore.getState().deviceAppearance).toEqual({
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
    });
    localStorage.removeItem(STORAGE_KEY);
  });

  it("migrates the old theme-only storage key when the settings blob is absent", async () => {
    if (typeof localStorage === "undefined") return;
    localStorage.removeItem(STORAGE_KEY);
    localStorage.setItem(THEME_STORAGE_KEY, "dark");

    vi.resetModules();
    const fresh = await import("./settingsStore");
    expect(fresh.useSettingsStore.getState().deviceAppearance.themePreference).toBe("dark");
    localStorage.removeItem(THEME_STORAGE_KEY);
  });
});
