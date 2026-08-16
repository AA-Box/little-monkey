import { beforeEach, describe, expect, it, vi } from "vitest";

const invokeMock = vi.hoisted(() => vi.fn());

vi.mock("@tauri-apps/api/core", () => ({
  invoke: (...args: unknown[]) => invokeMock(...args),
  isTauri: () => true,
}));

import type { ActiveCapability } from "../lib/executableExtensionsClient";
import {
  DEFAULT_WEB_SETTINGS,
  useWebStore,
  type WebSettings,
} from "./webStore";

const searchCapability: ActiveCapability = {
  kind: "web_search",
  capability_id: "private-search",
  extension_id: "dev.example.web",
  version: "1.2.3",
  display_name: "Private Search",
  description: "Search a private index",
  input_schema: { type: "object" },
};

const fetchCapability: ActiveCapability = {
  ...searchCapability,
  kind: "web_fetch",
  capability_id: "private-fetch",
  display_name: "Private Fetch",
};

beforeEach(() => {
  invokeMock.mockReset();
  useWebStore.setState({
    settings: DEFAULT_WEB_SETTINGS,
    hasBraveKey: false,
    loaded: false,
    searchCapabilities: [],
    fetchCapabilities: [],
  });
});

describe("webStore executable providers", () => {
  it("discovers and stores typed active search and fetch capabilities", async () => {
    invokeMock.mockImplementation((command: string, payload?: { kind?: string }) => {
      if (command === "web_get_settings") return Promise.resolve(DEFAULT_WEB_SETTINGS);
      if (command === "web_has_brave_key") return Promise.resolve(false);
      if (command === "extensions_active_capabilities") {
        return Promise.resolve(payload?.kind === "web_search" ? [searchCapability] : [fetchCapability]);
      }
      return Promise.resolve(undefined);
    });

    await useWebStore.getState().refresh();

    expect(invokeMock.mock.calls).toEqual([
      ["web_get_settings"],
      ["web_has_brave_key"],
      ["extensions_active_capabilities", { kind: "web_search" }],
      ["extensions_active_capabilities", { kind: "web_fetch" }],
    ]);
    expect(useWebStore.getState()).toMatchObject({
      settings: DEFAULT_WEB_SETTINGS,
      loaded: true,
      searchCapabilities: [searchCapability],
      fetchCapabilities: [fetchCapability],
    });
  });

  it("persists exact executable selections before refreshing all provider state", async () => {
    const selected: WebSettings = {
      ...DEFAULT_WEB_SETTINGS,
      search_provider: "executable_extension",
      search_extension_id: searchCapability.extension_id,
      search_extension_capability_id: searchCapability.capability_id,
      fetch_provider: "executable_extension",
      fetch_extension_id: fetchCapability.extension_id,
      fetch_extension_capability_id: fetchCapability.capability_id,
    };
    invokeMock.mockImplementation((command: string, payload?: { kind?: string }) => {
      if (command === "web_get_settings") return Promise.resolve(selected);
      if (command === "web_has_brave_key") return Promise.resolve(true);
      if (command === "extensions_active_capabilities") {
        return Promise.resolve(payload?.kind === "web_search" ? [searchCapability] : [fetchCapability]);
      }
      return Promise.resolve(undefined);
    });

    await useWebStore.getState().setSettings(selected);

    expect(invokeMock.mock.calls).toEqual([
      ["web_set_settings", { settings: selected }],
      ["web_get_settings"],
      ["web_has_brave_key"],
      ["extensions_active_capabilities", { kind: "web_search" }],
      ["extensions_active_capabilities", { kind: "web_fetch" }],
    ]);
    expect(useWebStore.getState()).toMatchObject({ settings: selected, hasBraveKey: true });
  });

  it("keeps built-in settings usable when extension discovery is unavailable", async () => {
    invokeMock.mockImplementation((command: string) => {
      if (command === "web_get_settings") return Promise.resolve(DEFAULT_WEB_SETTINGS);
      if (command === "web_has_brave_key") return Promise.resolve(false);
      if (command === "extensions_active_capabilities") {
        return Promise.reject(new Error("extension registry unavailable"));
      }
      return Promise.resolve(undefined);
    });

    await useWebStore.getState().refresh();

    expect(useWebStore.getState()).toMatchObject({
      settings: DEFAULT_WEB_SETTINGS,
      loaded: true,
      searchCapabilities: [],
      fetchCapabilities: [],
    });
  });
});
