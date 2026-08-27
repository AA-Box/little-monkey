// @vitest-environment jsdom
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

vi.mock("../../lib/i18n", () => ({
  useT: () => ({ t: (key: string) => key, locale: "en-US" }),
}));
vi.mock("../Models", () => ({ ModelManager: () => <div data-testid="local-model-manager" /> }));
vi.mock("../Ollama", () => ({ OllamaPanel: () => <div data-testid="ollama-panel" /> }));

import { useModelStore } from "../../store/modelStore";
import { useSettingsStore } from "../../store/settingsStore";
import { AddModelDialog } from "./AddModelDialog";

const refreshProviders = vi.fn(async () => undefined);
const refreshProviderModels = vi.fn(async () => undefined);
const addCustomProvider = vi.fn(async () => undefined);
const useProviderModel = vi.fn();
const setProviderModelSelection = vi.fn();
const setProviderKey = vi.fn(async (providerId: string) => {
  useModelStore.setState((state) => ({
    providers: state.providers.map((provider) =>
      provider.id === providerId ? { ...provider, has_key: true } : provider,
    ),
    providerModels: {
      ...state.providerModels,
      [providerId]: [{ id: "claude-sonnet-test" }],
    },
  }));
});

describe("AddModelDialog point-of-use cloud setup", () => {
  beforeEach(() => {
    vi.clearAllMocks();
    useModelStore.setState({
      providers: [
        {
          id: "anthropic",
          label: "Anthropic",
          base_url: "https://api.anthropic.com/v1",
          is_custom: false,
          has_key: false,
          is_extension: false,
        },
      ],
      providerModels: {},
      providerKeyError: {},
      activeProvider: "local",
      active: null,
      activeOllamaModel: null,
      activeProviderId: null,
      activeProviderModel: null,
      refreshProviders,
      refreshProviderModels,
      setProviderKey,
      addCustomProvider,
      useProviderModel,
    });
    useSettingsStore.setState({
      providerModelFilters: {
        anthropic: { showAll: false, selectedModelIds: [] },
      },
      setProviderModelSelection,
    });
  });

  afterEach(cleanup);

  it("connects a provider, discovers its models, selects one, and returns to chat", async () => {
    const onClose = vi.fn();
    render(<AddModelDialog open onClose={onClose} />);

    const providerHeading = await screen.findByRole("heading", { name: "Anthropic" });
    const providerSetup = providerHeading.parentElement?.parentElement;
    expect(providerSetup).toBeTruthy();

    const keyInput = providerSetup?.querySelector<HTMLInputElement>(
      'input[placeholder="ProviderCard.apiKeyPlaceholder"]',
    );
    expect(keyInput).toBeTruthy();
    fireEvent.change(keyInput!, { target: { value: "sk-ant-test" } });

    const connectButton = providerSetup?.querySelector("button");
    expect(connectButton).toBeTruthy();
    fireEvent.click(connectButton!);

    await waitFor(() => {
      expect(setProviderKey).toHaveBeenCalledWith("anthropic", "sk-ant-test");
    });

    const model = await screen.findByText("claude-sonnet-test");
    fireEvent.click(model.closest("button")!);

    expect(setProviderModelSelection).toHaveBeenCalledWith(
      "anthropic",
      ["claude-sonnet-test"],
      ["claude-sonnet-test"],
    );
    expect(useProviderModel).toHaveBeenCalledWith("anthropic", "claude-sonnet-test");
    expect(onClose).toHaveBeenCalledTimes(1);
  });
});
