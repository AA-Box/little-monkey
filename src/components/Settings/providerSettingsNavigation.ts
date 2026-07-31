import type { ProviderConfig } from "../../store/modelStore";

export type ProviderSettingsTab = `provider:${string}`;

export interface ConnectedProviderNavigationItem {
  tabId: ProviderSettingsTab;
  providerId: string;
  label: string;
}

export interface OllamaConfigurationState {
  reachable: boolean;
  binaryFound: boolean;
  installedModelCount: number;
  signedInUser: string | null;
}

export function providerSettingsTabId(providerId: string): ProviderSettingsTab {
  return `provider:${encodeURIComponent(providerId)}`;
}

export function providerIdFromSettingsTab(tab: string): string | null {
  if (!tab.startsWith("provider:")) return null;
  const encoded = tab.slice("provider:".length);
  if (!encoded) return null;
  try {
    return decodeURIComponent(encoded);
  } catch {
    return null;
  }
}

/** Only providers with a live keychain-backed connection get a model-selection tab. */
export function connectedProviderNavigationItems(
  providers: ProviderConfig[],
): ConnectedProviderNavigationItem[] {
  return providers
    .filter((provider) => provider.has_key)
    .map((provider) => ({
      tabId: providerSettingsTabId(provider.id),
      providerId: provider.id,
      label: provider.label,
    }));
}

/** Ollama is configured when its local runtime, models, or signed-in account is present. */
export function isOllamaConfigured(state: OllamaConfigurationState): boolean {
  return (
    state.reachable ||
    state.binaryFound ||
    state.installedModelCount > 0 ||
    Boolean(state.signedInUser)
  );
}
