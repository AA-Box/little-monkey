import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it, vi } from "vitest";

const modelState = vi.hoisted(() => ({
  installed: [],
  ollamaModels: [],
  providers: [{
    id: "openrouter",
    label: "OpenRouter",
    base_url: "https://openrouter.ai/api/v1",
    is_custom: false,
    has_key: true,
  }],
  providerModels: {
    openrouter: [
      { id: "selected-model" },
      { id: "active-model" },
      { id: "hidden-model" },
    ],
  },
  active: null,
  activeProvider: "provider" as const,
  activeProviderId: "openrouter",
  activeProviderModel: "active-model",
  activeOllamaModel: null,
  start: vi.fn(),
  useOllamaModel: vi.fn(),
  useProviderModel: vi.fn(),
}));

const settingsState = vi.hoisted(() => ({
  providerModelFilters: {
    openrouter: {
      showAll: false,
      selectedModelIds: ["selected-model"],
    },
  },
}));

vi.mock("../../store/modelStore", () => {
  const useModelStore = Object.assign(
    (selector?: (state: typeof modelState) => unknown) =>
      selector ? selector(modelState) : modelState,
    { getState: () => modelState },
  );
  return { useModelStore };
});

vi.mock("../../store/settingsStore", () => {
  const useSettingsStore = Object.assign(
    (selector: (state: typeof settingsState) => unknown) =>
      selector(settingsState),
    { getState: () => settingsState },
  );
  return {
    DEFAULT_PROVIDER_MODEL_FILTER: { showAll: true, selectedModelIds: [] },
    useSettingsStore,
  };
});

import { CommandPalette } from "./CommandPalette";

describe("CommandPalette provider model curation", () => {
  it("shows selected and active models without leaking unselected inventory", () => {
    const markup = renderToStaticMarkup(
      <CommandPalette onClose={vi.fn()} onOpenSettingsTab={vi.fn()} />,
    );

    expect(markup).toContain("selected-model");
    expect(markup).toContain("active-model");
    expect(markup).not.toContain("hidden-model");
  });
});
