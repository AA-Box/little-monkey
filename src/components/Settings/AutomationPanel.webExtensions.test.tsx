// @vitest-environment jsdom
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";

vi.mock("@tauri-apps/api/core", () => ({ invoke: vi.fn(), isTauri: () => false }));

import type { ActiveCapability } from "../../lib/executableExtensionsClient";
import { useSettingsStore } from "../../store/settingsStore";
import {
  DEFAULT_WEB_SETTINGS,
  useWebStore,
  type WebSettings,
} from "../../store/webStore";
import { WebSettingsSection } from "./AutomationPanel";

const searchCapabilities: ActiveCapability[] = [
  {
    kind: "web_search",
    capability_id: "search",
    extension_id: "dev.example.first",
    version: "1.0.0",
    display_name: "First search",
    description: "",
    input_schema: { type: "object" },
  },
  {
    kind: "web_search",
    capability_id: "search",
    extension_id: "dev.example.second",
    version: "1.0.0",
    display_name: "Second search",
    description: "",
    input_schema: { type: "object" },
  },
];

const fetchCapabilities: ActiveCapability[] = searchCapabilities.map((capability) => ({
  ...capability,
  kind: "web_fetch",
  capability_id: "fetch",
  display_name: capability.display_name.replace("search", "fetch"),
}));

const settings: WebSettings = {
  ...DEFAULT_WEB_SETTINGS,
  search_provider: "executable_extension",
  search_extension_id: searchCapabilities[0].extension_id,
  search_extension_capability_id: searchCapabilities[0].capability_id,
  fetch_provider: "executable_extension",
  fetch_extension_id: fetchCapabilities[0].extension_id,
  fetch_extension_capability_id: fetchCapabilities[0].capability_id,
};

const refresh = vi.fn(async () => undefined);
const setSettings = vi.fn(async () => undefined);

beforeEach(() => {
  refresh.mockClear();
  setSettings.mockClear();
  useSettingsStore.setState({ webToolsEnabled: true });
  useWebStore.setState({
    settings,
    hasBraveKey: false,
    loaded: true,
    searchCapabilities,
    fetchCapabilities,
    refresh,
    setSettings,
  });
});

afterEach(() => {
  cleanup();
});

describe("WebSettingsSection executable providers", () => {
  it("persists the selected capability together with its exact owning extension", async () => {
    render(<WebSettingsSection />);

    const search = screen.getByLabelText("Executable web-search capability") as HTMLSelectElement;
    const secondSearch = JSON.stringify([
      searchCapabilities[1].extension_id,
      searchCapabilities[1].capability_id,
    ]);
    expect(Array.from(search.options).map((option) => option.value)).toContain(secondSearch);
    fireEvent.change(search, { target: { value: secondSearch } });

    await waitFor(() => expect(setSettings).toHaveBeenCalledWith({
      ...settings,
      search_extension_id: searchCapabilities[1].extension_id,
      search_extension_capability_id: searchCapabilities[1].capability_id,
    }));

    const fetch = screen.getByLabelText("Executable web-fetch capability") as HTMLSelectElement;
    const secondFetch = JSON.stringify([
      fetchCapabilities[1].extension_id,
      fetchCapabilities[1].capability_id,
    ]);
    expect(Array.from(fetch.options).map((option) => option.value)).toContain(secondFetch);
    fireEvent.change(fetch, { target: { value: secondFetch } });

    await waitFor(() => expect(setSettings).toHaveBeenCalledWith({
      ...settings,
      fetch_extension_id: fetchCapabilities[1].extension_id,
      fetch_extension_capability_id: fetchCapabilities[1].capability_id,
    }));
  });
});
