import { afterEach, describe, expect, it } from "vitest";

import { isVisionCapableLocalModel, isVisionCapableProviderModel } from "./visionModels";
import { useModelStore } from "../store/modelStore";
import { useSettingsStore } from "../store/settingsStore";

afterEach(() => {
  useModelStore.setState({ providerModels: {}, activeProvider: "local", llamaStatus: "stopped", llamaVisionEnabled: false });
  useSettingsStore.setState({ visionOverrides: {} });
});

describe("isVisionCapableLocalModel", () => {
  it("requires a ready runtime that reports a loaded projector", () => {
    useModelStore.setState({ activeProvider: "local", llamaStatus: "ready", llamaVisionEnabled: true });
    expect(isVisionCapableLocalModel()).toBe(true);
    useModelStore.setState({ llamaStatus: "starting" });
    expect(isVisionCapableLocalModel()).toBe(false);
  });
});

/** A false "no" here is silent data loss: `stripImagesForTextOnlyTarget` in
 *  `agentLoop.ts` drops the attached image and leaves a marker behind, so a
 *  model that could have read the image never sees it. Pin the real ids. */
describe("isVisionCapableProviderModel", () => {
  it("classifies real Anthropic model ids", () => {
    for (const [modelId, vision] of [
      ["claude-3-opus-20240229", true],
      ["claude-3-5-sonnet-20241022", true],
      ["claude-3-5-haiku-20241022", true],
      ["claude-3-7-sonnet-20250219", true],
      ["claude-sonnet-4-20250514", true],
      ["claude-sonnet-4-5-20250929", true],
      ["claude-haiku-4-5-20251001", true],
      ["claude-opus-4-8", true],
      ["claude-opus-5", true],
      ["claude-sonnet-5", true],
      ["claude-fable-5", true],
      ["claude-2.1", false],
      ["claude-instant-1.2", false],
    ] as const) {
      expect(isVisionCapableProviderModel("anthropic", modelId), modelId).toBe(vision);
    }
  });

  it("prefers what the provider reported over the name heuristic", () => {
    useModelStore.setState({
      providerModels: {
        openrouter: [
          // A name the heuristic can't know about, that OpenRouter says sees images.
          { id: "some-vendor/brand-new-vlm", vision: true },
          // And the reverse: a name that looks vision-capable but isn't.
          { id: "some-vendor/gemini-text-only", vision: false },
          // Provider said nothing — the field is absent, not null.
          { id: "some-vendor/unreported" },
        ],
      },
    });
    expect(isVisionCapableProviderModel("openrouter", "some-vendor/brand-new-vlm")).toBe(true);
    expect(isVisionCapableProviderModel("openrouter", "some-vendor/gemini-text-only")).toBe(false);
    // Nothing reported, and no pattern matches → the heuristic's "no".
    expect(isVisionCapableProviderModel("openrouter", "some-vendor/unreported")).toBe(false);
  });

  it("lets the user's override beat the provider", () => {
    useModelStore.setState({ providerModels: { openrouter: [{ id: "vendor/model", vision: false }] } });
    useSettingsStore.setState({ visionOverrides: { "provider:openrouter:vendor/model": true } });
    expect(isVisionCapableProviderModel("openrouter", "vendor/model")).toBe(true);
  });
});
