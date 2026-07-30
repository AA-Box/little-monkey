import { renderToStaticMarkup } from "react-dom/server";
import { beforeEach, describe, expect, it, vi } from "vitest";

const modelStoreState = vi.hoisted(() => ({
  providerModels: {
    openrouter: [
      { id: "model-a" },
      { id: "model-b" },
      { id: "model-c" },
    ],
  } as Record<string, { id: string }[]>,
  refreshProviderModels: vi.fn(),
}));

const settingsStoreState = vi.hoisted(() => ({
  providerModelFilters: {
    openrouter: {
      showAll: true,
      selectedModelIds: [] as string[],
    },
  } as Record<string, { showAll: boolean; selectedModelIds: string[] }>,
  setProviderModelSelection: vi.fn(),
  toggleProviderModelSelected: vi.fn(),
  clearProviderModelSelection: vi.fn(),
}));

vi.mock("../../store/modelStore", () => ({
  useModelStore: (selector: (state: typeof modelStoreState) => unknown) =>
    selector(modelStoreState),
}));

vi.mock("../../store/settingsStore", () => ({
  DEFAULT_PROVIDER_MODEL_FILTER: { showAll: true, selectedModelIds: [] },
  useSettingsStore: (selector: (state: typeof settingsStoreState) => unknown) =>
    selector(settingsStoreState),
}));

vi.mock("../../lib/i18n", () => ({
  useT: () => ({
    t: (key: string, vars?: Record<string, string | number>) => {
      const labels: Record<string, string> = {
        "OpenRouterModelsPanel.description":
          "Choose which {{provider}} models appear in model pickers.",
        "OpenRouterModelsPanel.showAllToggle": "Show all models",
        "OpenRouterModelsPanel.showAllDescription":
          "Check to select every available model; uncheck to clear the selection.",
        "OpenRouterModelsPanel.noModelsLoaded": "No models loaded",
        "OpenRouterModelsPanel.clearSelection": "Clear selection",
        "OpenRouterModelsPanel.selectedCount":
          "{{selected}} of {{total}} selected",
        "ProviderCard.filterModelsPlaceholder": "Filter {{count}} models",
        "ProviderCard.noModelsMatch": "No models match {{filter}}",
      };
      return (labels[key] ?? key).replace(/\{\{(\w+)\}\}/g, (match, name) =>
        vars && name in vars ? String(vars[name]) : match,
      );
    },
  }),
}));

import { ProviderModelsPanel } from "./OpenRouterModelsPanel";

describe("ProviderModelsPanel", () => {
  beforeEach(() => {
    settingsStoreState.providerModelFilters.openrouter = {
      showAll: true,
      selectedModelIds: [],
    };
  });

  it("fills the available panel height while keeping the model list scrollable", () => {
    const markup = renderToStaticMarkup(
      <ProviderModelsPanel providerId="openrouter" providerLabel="OpenRouter" />,
    );

    expect(markup).toContain("h-full min-h-0");
    expect(markup).toContain("min-h-0 flex-1");
    expect(markup).toContain("overflow-y-auto");
    expect(markup).not.toContain("max-h-96");
  });

  it("renders implicit show-all as a truthful checked master and checked rows", () => {
    const markup = renderToStaticMarkup(
      <ProviderModelsPanel providerId="openrouter" providerLabel="OpenRouter" />,
    );

    expect(markup).toContain("Choose which OpenRouter models");
    expect(markup.match(/checked=""/g)).toHaveLength(4);
    expect(markup).toContain("3 of 3 selected");
  });

  it("renders a partial selection with an unchecked master", () => {
    settingsStoreState.providerModelFilters.openrouter = {
      showAll: false,
      selectedModelIds: ["model-b"],
    };
    const markup = renderToStaticMarkup(
      <ProviderModelsPanel providerId="openrouter" providerLabel="OpenRouter" />,
    );

    expect(markup.match(/checked=""/g)).toHaveLength(1);
    expect(markup).toContain("1 of 3 selected");
  });
});
