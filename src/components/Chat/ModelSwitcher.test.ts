import { describe, expect, it } from "vitest";

import {
  visibleProviderModels,
  visibleProviderModelsForProvider,
} from "../../lib/providerModelSelection";
import { modelMatchesQuery } from "./ModelSwitcher";

const models = [{ id: "model-a" }, { id: "model-b" }, { id: "model-c" }];

describe("visibleProviderModels", () => {
  it("shows the full inventory only when showAll is true", () => {
    expect(
      visibleProviderModels(models, { showAll: true, selectedModelIds: [] }),
    ).toEqual(models);
  });

  it("treats an explicitly empty selection as no visible models", () => {
    expect(
      visibleProviderModels(models, { showAll: false, selectedModelIds: [] }),
    ).toEqual([]);
  });

  it("shows only selected provider models when partially curated", () => {
    expect(
      visibleProviderModels(models, {
        showAll: false,
        selectedModelIds: ["model-b"],
      }),
    ).toEqual([{ id: "model-b" }]);
  });

  it("keeps the active provider model visible when curation hides it", () => {
    expect(
      visibleProviderModels(
        models,
        { showAll: false, selectedModelIds: ["model-a"] },
        "model-c",
      ),
    ).toEqual([{ id: "model-a" }, { id: "model-c" }]);
    expect(
      visibleProviderModels(
        models,
        { showAll: false, selectedModelIds: [] },
        "model-b",
      ),
    ).toEqual([{ id: "model-b" }]);
  });

  it("applies the active-model exception only to the matching provider", () => {
    const filter = { showAll: false, selectedModelIds: ["model-a"] };
    const active = {
      activeProvider: "provider" as const,
      activeProviderId: "openrouter",
      activeProviderModel: "model-c",
    };

    expect(
      visibleProviderModelsForProvider("openrouter", models, filter, active),
    ).toEqual([{ id: "model-a" }, { id: "model-c" }]);
    expect(
      visibleProviderModelsForProvider("openai", models, filter, active),
    ).toEqual([{ id: "model-a" }]);
  });
});

describe("modelMatchesQuery", () => {
  it("matches model ids, display names, and provider labels case-insensitively", () => {
    expect(modelMatchesQuery("SONNET", "claude-sonnet-4-5", "Claude Sonnet 4.5")).toBe(true);
    expect(modelMatchesQuery("anthropic", "claude-sonnet-4-5", "Anthropic")).toBe(true);
    expect(modelMatchesQuery("gpt", "claude-sonnet-4-5", "Anthropic")).toBe(false);
  });

  it("treats blank search as showing every model", () => {
    expect(modelMatchesQuery("   ", "anything")).toBe(true);
  });
});
