import { describe, expect, it } from "vitest";

import { FEATURE_PANEL_IDS, featurePanelReducer } from "./appShellPanels";

describe("featurePanelReducer", () => {
  it("keeps feature panel identifiers unique", () => {
    expect(new Set(FEATURE_PANEL_IDS).size).toBe(FEATURE_PANEL_IDS.length);
    expect(FEATURE_PANEL_IDS).toContain("design-to-app");
  });

  it("replaces the active center surface when another one opens", () => {
    const state = featurePanelReducer("run-center", {
      type: "open",
      panel: "knowledge-graph",
    });

    expect(state).toBe("knowledge-graph");
  });

  it("ignores a stale close from the surface that was replaced", () => {
    const state = featurePanelReducer("knowledge-graph", {
      type: "close",
      panel: "run-center",
    });

    expect(state).toBe("knowledge-graph");
  });

  it("closes the active surface and can reset the shell", () => {
    expect(featurePanelReducer("run-center", { type: "close", panel: "run-center" })).toBeNull();
    expect(featurePanelReducer("settings", { type: "reset" })).toBeNull();
  });
});
