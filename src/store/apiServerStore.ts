import { create } from "zustand";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";

/** Mirrors the Rust `ApiServerStatusPayload` struct (src-tauri/src/server.rs)
 * exactly — the shape both `api_server_start`/`_stop`/`_status` return and
 * the `apiserver://status` event carries, same convention as
 * `llama.rs`/`ollama.rs`'s own status payloads. */
export interface ApiServerStatus {
  status: "stopped" | "starting" | "running" | "error";
  port: number;
  request_count: number;
  last_error: string | null;
  /** Plaintext bearer token, in memory only — `null` whenever the server
   * isn't running (phase 1: a single auto-generated default token; the full
   * multi-token store with scopes/backends is phase 2). */
  token: string | null;
}

/** LM Studio-compatible default, matching `server.rs::DEFAULT_PORT` — used
 * only until the first `refresh()` resolves. */
export const DEFAULT_API_SERVER_STATUS: ApiServerStatus = {
  status: "stopped",
  port: 1234,
  request_count: 0,
  last_error: null,
  token: null,
};

export interface ApiServerStore {
  status: ApiServerStatus;
  /** Whether `refresh()` has resolved at least once. */
  loaded: boolean;
  /** Port the user is editing in the panel before hitting Start — kept
   * separate from `status.port` (the last port the server actually bound
   * to) so editing the field doesn't look like a live status change. */
  portInput: number;

  refresh: () => Promise<void>;
  setPortInput: (port: number) => void;
  start: (port: number) => Promise<void>;
  stop: () => Promise<void>;
}

export const useApiServerStore = create<ApiServerStore>((set, get) => ({
  status: DEFAULT_API_SERVER_STATUS,
  loaded: false,
  portInput: DEFAULT_API_SERVER_STATUS.port,

  refresh: async () => {
    const status = await invoke<ApiServerStatus>("api_server_status");
    set({ status, loaded: true, portInput: status.port || get().portInput });
  },

  setPortInput: (port) => set({ portInput: port }),

  start: async (port) => {
    const status = await invoke<ApiServerStatus>("api_server_start", { port });
    set({ status });
  },

  stop: async () => {
    const status = await invoke<ApiServerStatus>("api_server_stop");
    set({ status });
  },
}));

// Keeps `status` (and, whenever the server (re)starts, `request_count`/
// `token`) live without polling — same event-listen pattern as
// `modelStore.ts`'s `llama://status`/`ollama://status` subscriptions.
void listen<ApiServerStatus>("apiserver://status", (event) => {
  useApiServerStore.setState({ status: event.payload });
}).catch((error) => {
  console.error("Failed to listen for apiserver://status events", error);
});

export default useApiServerStore;
