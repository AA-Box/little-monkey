import { beforeEach, describe, expect, it } from "vitest";

import { useModelStore } from "../../store/modelStore";
import { useSettingsStore } from "../../store/settingsStore";
import { switchModelFromSlash } from "./ChatWindow";

const provider = {
  id: "openrouter",
  label: "OpenRouter",
  base_url: "https://openrouter.ai/api/v1",
  is_custom: false,
  has_key: true,
};

describe("/model provider curation", () => {
  beforeEach(() => {
    useModelStore.setState({
      installed: [],
      ollamaModels: [],
      providers: [provider],
      providerModels: {
        openrouter: [{ id: "model-a" }, { id: "model-b" }],
      },
      activeProvider: "local",
      activeProviderId: null,
      activeProviderModel: null,
    });
    useSettingsStore.setState({
      providerModelFilters: {
        openrouter: {
          showAll: false,
          selectedModelIds: ["model-a"],
        },
      },
    });
  });

  it("rejects an unselected provider model", async () => {
    await expect(
      switchModelFromSlash("openrouter:model-b"),
    ).rejects.toThrow(/No configured model matches/);
  });

  it("switches to a selected provider model", async () => {
    await expect(
      switchModelFromSlash("openrouter:model-a"),
    ).resolves.toBe("openrouter:model-a");

    expect(useModelStore.getState()).toMatchObject({
      activeProvider: "provider",
      activeProviderId: "openrouter",
      activeProviderModel: "model-a",
    });
  });

  it("keeps the active model addressable after it is unchecked", async () => {
    useModelStore.setState({
      activeProvider: "provider",
      activeProviderId: "openrouter",
      activeProviderModel: "model-b",
    });

    await expect(
      switchModelFromSlash("openrouter:model-b"),
    ).resolves.toBe("openrouter:model-b");
  });
});
