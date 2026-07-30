import { describe, expect, it } from "vitest";

import type { ProviderConfig } from "../../store/modelStore";
import {
  connectedProviderNavigationItems,
  isOllamaConfigured,
  providerIdFromSettingsTab,
  providerSettingsTabId,
} from "./providerSettingsNavigation";

function provider(
  id: string,
  label: string,
  hasKey: boolean,
  isCustom = false,
): ProviderConfig {
  return {
    id,
    label,
    base_url: `https://${id}.example/v1`,
    is_custom: isCustom,
    has_key: hasKey,
  };
}

describe("provider settings navigation", () => {
  it("hides disconnected built-in and custom providers", () => {
    expect(
      connectedProviderNavigationItems([
        provider("openrouter", "OpenRouter", false),
        provider("team-gateway", "Team Gateway", false, true),
      ]),
    ).toEqual([]);
  });

  it("shows every connected built-in and dynamic custom provider", () => {
    expect(
      connectedProviderNavigationItems([
        provider("openai", "OpenAI", true),
        provider("openrouter", "OpenRouter", false),
        provider("team-gateway", "Team Gateway", true, true),
      ]),
    ).toEqual([
      {
        tabId: "provider:openai",
        providerId: "openai",
        label: "OpenAI",
      },
      {
        tabId: "provider:team-gateway",
        providerId: "team-gateway",
        label: "Team Gateway",
      },
    ]);
  });

  it("round-trips provider ids safely through dynamic tab ids", () => {
    const tabId = providerSettingsTabId("custom/team gateway");
    expect(tabId).toBe("provider:custom%2Fteam%20gateway");
    expect(providerIdFromSettingsTab(tabId)).toBe("custom/team gateway");
    expect(providerIdFromSettingsTab("providers")).toBeNull();
  });

  it("only shows Ollama after a local runtime, model, session, or account is present", () => {
    const disconnected = {
      reachable: false,
      binaryFound: false,
      installedModelCount: 0,
      signedInUser: null,
    };
    expect(isOllamaConfigured(disconnected)).toBe(false);
    expect(isOllamaConfigured({ ...disconnected, reachable: true })).toBe(true);
    expect(isOllamaConfigured({ ...disconnected, binaryFound: true })).toBe(true);
    expect(isOllamaConfigured({ ...disconnected, installedModelCount: 1 })).toBe(true);
    expect(isOllamaConfigured({ ...disconnected, signedInUser: "ahmad" })).toBe(true);
  });
});
