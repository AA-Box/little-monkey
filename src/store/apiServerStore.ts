import { create } from "zustand";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";

/** Mirrors the Rust `ApiServerStatusPayload` struct (src-tauri/src/server.rs)
 * exactly — the shape both `api_server_start`/`_stop`/`_status` return and
 * the `apiserver://status` event carries, same convention as
 * `llama.rs`/`ollama.rs`'s own status payloads. No token material lives
 * here (that was a phase-1-only stopgap) — see `mintedToken` below for the
 * "show once" flow. */
export interface ApiServerStatus {
  status: "stopped" | "starting" | "running" | "error";
  port: number;
  request_count: number;
  last_error: string | null;
}

/** Mirrors the Rust `Scope`/`Backend` enums (src-tauri/src/server.rs)
 * exactly — `#[serde(rename_all = "snake_case")]` on already-lowercase
 * single-word variant names serializes to exactly these strings. */
export type Scope = "chat" | "models" | "embeddings";
export type Backend = "local" | "ollama" | "providers";

/** Mirrors the Rust `TokenEntryView` struct — never the digest, which stays
 * on the Rust side (see `TokenEntry::sha256`'s doc comment). */
export interface TokenEntry {
  id: string;
  label: string;
  scopes: Scope[];
  backends: Backend[];
  created_at: number;
  last_used_at: number | null;
}

/** Mirrors the Rust `ApiServerConfigView` struct — the subset of
 * `api_server.json` the Settings panel gets/sets directly. Tokens are
 * managed separately via their own create/revoke/list commands. */
export interface ApiServerConfig {
  port: number;
  autostart: boolean;
  require_token: boolean;
  expose_ollama: boolean;
  expose_providers: boolean;
}

/** Mirrors the Rust `CreateTokenResult` struct — the plaintext token,
 * returned exactly once by `api_server_create_token`. Held in
 * `mintedToken` until the user dismisses it; never persisted anywhere on
 * the frontend (not even to `localStorage`) and never refetchable — only
 * `entry.sha256`'s absence-from-the-frontend digest lives on afterward. */
export interface MintedToken {
  token: string;
  entry: TokenEntry;
}

/** LM Studio-compatible default, matching `server.rs::DEFAULT_PORT` — used
 * only until the first `refresh()` resolves. */
export const DEFAULT_API_SERVER_STATUS: ApiServerStatus = {
  status: "stopped",
  port: 1234,
  request_count: 0,
  last_error: null,
};

/** Mirrors `ApiServerConfig::default()` on the Rust side exactly, for the
 * same "no flash of the wrong value before the first load" reason
 * `webStore.ts`'s `DEFAULT_WEB_SETTINGS` exists. */
export const DEFAULT_API_SERVER_CONFIG: ApiServerConfig = {
  port: 1234,
  autostart: false,
  require_token: true,
  expose_ollama: true,
  expose_providers: false,
};

export interface ApiServerStore {
  status: ApiServerStatus;
  config: ApiServerConfig;
  tokens: TokenEntry[];
  /** Whether `refresh()` has resolved at least once. */
  loaded: boolean;
  /** The most recently minted token's plaintext, shown once by the panel's
   * "copy it now" banner. Cleared by `dismissMintedToken()` (the user
   * closing the banner) or by minting another one. */
  mintedToken: MintedToken | null;

  refresh: () => Promise<void>;
  start: () => Promise<void>;
  stop: () => Promise<void>;
  /** Persists a full config update — port/autostart/require_token/
   * expose_ollama/expose_providers — then re-fetches to reflect what was
   * actually saved. If the server is currently running, the Rust side
   * gracefully restarts it with the new settings (the `apiserver://status`
   * event subscription below picks up the resulting status change live). */
  setConfig: (config: ApiServerConfig) => Promise<void>;
  createToken: (label: string, scopes: Scope[], backends: Backend[]) => Promise<void>;
  revokeToken: (id: string) => Promise<void>;
  dismissMintedToken: () => void;
}

export const useApiServerStore = create<ApiServerStore>((set) => ({
  status: DEFAULT_API_SERVER_STATUS,
  config: DEFAULT_API_SERVER_CONFIG,
  tokens: [],
  loaded: false,
  mintedToken: null,

  refresh: async () => {
    const [status, config, tokens] = await Promise.all([
      invoke<ApiServerStatus>("api_server_status"),
      invoke<ApiServerConfig>("api_server_get_config"),
      invoke<TokenEntry[]>("api_server_list_tokens"),
    ]);
    set({ status, config, tokens, loaded: true });
  },

  start: async () => {
    const status = await invoke<ApiServerStatus>("api_server_start");
    set({ status });
  },

  stop: async () => {
    const status = await invoke<ApiServerStatus>("api_server_stop");
    set({ status });
  },

  setConfig: async (config) => {
    const updated = await invoke<ApiServerConfig>("api_server_set_config", { config });
    set({ config: updated });
  },

  createToken: async (label, scopes, backends) => {
    const result = await invoke<MintedToken>("api_server_create_token", { label, scopes, backends });
    set((state) => ({ mintedToken: result, tokens: [...state.tokens, result.entry] }));
  },

  revokeToken: async (id) => {
    await invoke("api_server_revoke_token", { id });
    set((state) => ({ tokens: state.tokens.filter((t) => t.id !== id) }));
  },

  dismissMintedToken: () => set({ mintedToken: null }),
}));

// Keeps `status` live without polling — same event-listen pattern as
// `modelStore.ts`'s `llama://status`/`ollama://status` subscriptions.
void listen<ApiServerStatus>("apiserver://status", (event) => {
  useApiServerStore.setState({ status: event.payload });
}).catch((error) => {
  console.error("Failed to listen for apiserver://status events", error);
});

export default useApiServerStore;
