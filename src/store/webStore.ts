import { create } from "zustand";
import { invoke } from "@tauri-apps/api/core";

/** Mirrors the Rust `SearchProvider` enum (src-tauri/src/web.rs) exactly —
 * `#[serde(rename_all = "snake_case")]` on that enum's single-uppercase-run
 * variant names (`Duckduckgo`/`Brave`/`Searxng`) serializes to exactly these
 * three lowercase strings. */
export type SearchProvider = "duckduckgo" | "brave" | "searxng";

/**
 * Mirrors the Rust `WebSettings` struct (src-tauri/src/web.rs) exactly —
 * plain snake_case field names (no serde rename), the same hand-editable-
 * file convention as `McpServerEntry`/`ProvidersFile`. This is the shape
 * `web_get_settings` returns and `web_set_settings` takes as its `settings`
 * argument. Notably absent: the Brave API key itself — see `hasBraveKey`/
 * `setBraveKey`/`removeBraveKey` below, which go through the OS keychain via
 * separate commands instead.
 */
export interface WebSettings {
  search_provider: SearchProvider;
  /** Required when `search_provider === "searxng"`; `null` (never an empty
   * string) is the "unset" state — the backend normalizes a blank input back
   * to `null` before persisting. */
  searxng_base_url: string | null;
  /** Whether `web_fetch`/`web_search` may target a loopback/private/
   * link-local host (llama-server, Ollama, ...). Defaults to `false` — see
   * `AutomationPanel.tsx`'s Web section's warning copy for this toggle. */
  allow_local_network: boolean;
  /** Char-window size `web_fetch` uses when the model doesn't pass its own
   * `max_chars`. */
  fetch_max_chars: number;
}

/**
 * Stable "not loaded yet" fallback — must be a module-level constant, not a
 * fresh object literal inlined in the store, for the same infinite-re-render
 * reason `ProviderCard.tsx`'s `EMPTY_MODELS` and `settingsStore.ts`'s
 * `DEFAULT_PROVIDER_MODEL_FILTER` are: matches the Rust `WebSettings::default()`
 * exactly, so a component reading `settings` before the first `refresh()`
 * resolves sees the same defaults the backend would hand back anyway.
 */
export const DEFAULT_WEB_SETTINGS: WebSettings = {
  search_provider: "duckduckgo",
  searxng_base_url: null,
  allow_local_network: false,
  fetch_max_chars: 20_000,
};

export interface WebStore {
  /** Mirrors `<app_data>/web_settings.json` — workspaceStore's
   * mirror-Rust-state pattern (`roots`/`recent` mirroring the backend's
   * own persisted state). */
  settings: WebSettings;
  /** Live OS-keychain probe — never the Brave key itself — refreshed
   * alongside `settings` so `AutomationPanel.tsx`'s Web section's Brave key field always reflects
   * reality, mirroring `ProviderCard`'s `provider.has_key` pattern. */
  hasBraveKey: boolean;
  /** Whether `refresh()` has resolved at least once, so `AutomationPanel.tsx`'s Web section can
   * avoid flashing "no key saved" before the first load completes. */
  loaded: boolean;

  /** Re-fetch `settings` + `hasBraveKey` from the backend. */
  refresh: () => Promise<void>;
  /** Persist a full settings update, then re-fetch to confirm what was
   * actually saved (the backend normalizes/validates — e.g. blanking a
   * whitespace-only SearXNG URL back to `null`). */
  setSettings: (settings: WebSettings) => Promise<void>;
  /** Validates `apiKey` with a live 1-result Brave query before saving it to
   * the OS keychain — throws (without saving) if the key is rejected. */
  setBraveKey: (apiKey: string) => Promise<void>;
  /** Removes the saved Brave API key from the OS keychain. */
  removeBraveKey: () => Promise<void>;
}

export const useWebStore = create<WebStore>((set, get) => ({
  settings: DEFAULT_WEB_SETTINGS,
  hasBraveKey: false,
  loaded: false,

  refresh: async () => {
    const [settings, hasBraveKey] = await Promise.all([
      invoke<WebSettings>("web_get_settings"),
      invoke<boolean>("web_has_brave_key"),
    ]);
    set({ settings, hasBraveKey, loaded: true });
  },

  setSettings: async (settings) => {
    await invoke("web_set_settings", { settings });
    await get().refresh();
  },

  setBraveKey: async (apiKey) => {
    await invoke("web_set_brave_key", { apiKey });
    await get().refresh();
  },

  removeBraveKey: async () => {
    await invoke("web_remove_brave_key");
    await get().refresh();
  },
}));

export default useWebStore;
