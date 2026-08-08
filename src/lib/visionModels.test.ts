import { describe, expect, it } from "vitest";

import { isVisionCapableProviderModel } from "./visionModels";

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
});
