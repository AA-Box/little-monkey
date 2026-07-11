import { create } from "zustand";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";

/**
 * Mirrors the Rust `McpTransport` enum (src-tauri/src/mcp.rs) exactly — a
 * tagged union (`#[serde(tag = "type", rename_all = "snake_case")]`):
 * `{ type: "stdio", command, args, env }` for a child-process server, or
 * `{ type: "http", url }` for a remote one (phase 4 — `mcp_connect` errors
 * on this variant for now).
 */
export type McpTransport =
  | { type: "stdio"; command: string; args: string[]; env: Record<string, string> }
  | { type: "http"; url: string };

/**
 * Mirrors the Rust `CachedMcpTool` struct exactly — that struct uses
 * `#[serde(rename_all = "camelCase")]`, so `input_schema` becomes
 * `inputSchema` on the wire.
 */
export interface CachedMcpTool {
  name: string;
  description: string | null;
  inputSchema: object;
}

/**
 * Mirrors the Rust `McpServerEntry` struct exactly — plain snake_case field
 * names (no serde rename), since `mcp_servers.json` is meant to be
 * hand-editable, same convention as `providers.json`. This is the shape
 * `addServer`/`updateServer` send as the `entry` argument.
 */
export interface McpServerEntry {
  id: string;
  label: string;
  transport: McpTransport;
  enabled: boolean;
  tool_allowlist: string[] | null;
  timeout_secs: number | null;
}

/** Live connection status of one configured server — mirrors the string
 * values `mcp.rs`'s `emit_status`/`build_info` produce. */
export type McpStatus = "connecting" | "connected" | "error" | "disconnected";

/**
 * Mirrors the Rust `McpServerInfo` struct exactly (camelCase — that struct
 * uses `#[serde(rename_all = "camelCase")]`), as returned by
 * `mcp_list_servers`/`mcp_connect`: a server's config plus its live status
 * and (when connected) cached tool list.
 */
export interface McpServerInfo {
  id: string;
  label: string;
  transport: McpTransport;
  enabled: boolean;
  toolAllowlist: string[] | null;
  timeoutSecs: number | null;
  status: McpStatus;
  error: string | null;
  tools: CachedMcpTool[];
  instructions: string | null;
  /** Whether a bearer token is currently saved in the keychain for this
   * server (never the token itself) — always `false` for `stdio` servers. */
  hasHttpToken: boolean;
}

/**
 * Payload of the `mcp://status` Tauri event emitted by src-tauri/src/mcp.rs
 * — mirrors modelStore.ts's `llama://status` handling. Deliberately doesn't
 * carry the full tool list (just a count), so a `"connected"` transition
 * triggers a full `refresh()` to pick up what `mcp_connect` just cached;
 * every other status is patched into `servers` in place.
 */
interface McpStatusEvent {
  serverId: string;
  status: McpStatus;
  error: string | null;
  toolCount: number | null;
}

export interface McpStore {
  /** Every configured MCP server, config + live status + cached tools. */
  servers: McpServerInfo[];
  /** Reload every configured server's config + live status from the backend. */
  refresh: () => Promise<void>;
  /** Register a new MCP server. Does not connect it — call `connect` right after, as the (future) Settings UI does. */
  addServer: (entry: McpServerEntry) => Promise<void>;
  /** Replace an existing server's config by id. Does not reconnect — a caller that changed connection-affecting fields should follow up with `disconnect` + `connect`. */
  updateServer: (entry: McpServerEntry) => Promise<void>;
  /** Remove a server from the config, disconnecting it first if connected. */
  removeServer: (id: string) => Promise<void>;
  /** Enable or disable a configured server; disabling a connected one also disconnects it. */
  setEnabled: (id: string, enabled: boolean) => Promise<void>;
  /** Connect to a configured server (stdio or HTTP), caching its tool list. */
  connect: (id: string) => Promise<void>;
  /** Disconnect a currently-connected server. */
  disconnect: (id: string) => Promise<void>;
  /** Save (or overwrite) an HTTP server's bearer token in the OS keychain — never sent to `mcp_servers.json`. Takes effect on the next `connect`. */
  setHttpToken: (id: string, token: string) => Promise<void>;
  /** Remove an HTTP server's saved bearer token from the OS keychain. */
  removeHttpToken: (id: string) => Promise<void>;
}

export const useMcpStore = create<McpStore>((set, get) => ({
  servers: [],

  refresh: async () => {
    const servers = await invoke<McpServerInfo[]>("mcp_list_servers");
    set({ servers });
  },

  addServer: async (entry) => {
    await invoke("mcp_add_server", { entry });
    await get().refresh();
  },

  updateServer: async (entry) => {
    await invoke("mcp_update_server", { entry });
    await get().refresh();
  },

  removeServer: async (id) => {
    await invoke("mcp_remove_server", { server_id: id });
    await get().refresh();
  },

  setEnabled: async (id, enabled) => {
    await invoke("mcp_set_enabled", { server_id: id, enabled });
    await get().refresh();
  },

  connect: async (id) => {
    await invoke("mcp_connect", { server_id: id });
    await get().refresh();
  },

  disconnect: async (id) => {
    await invoke("mcp_disconnect", { server_id: id });
    await get().refresh();
  },

  setHttpToken: async (id, token) => {
    await invoke("mcp_set_http_token", { server_id: id, token });
    await get().refresh();
  },

  removeHttpToken: async (id) => {
    await invoke("mcp_remove_http_token", { server_id: id });
    await get().refresh();
  },
}));

void listen<McpStatusEvent>("mcp://status", (event) => {
  const { serverId, status, error } = event.payload;

  if (status === "connected") {
    // The event doesn't carry the full tool list (just a count) — refresh
    // to pick up what `mcp_connect` just cached, same reasoning as
    // modelStore's `llama://status` handler re-deriving `active` from a
    // fresh list rather than trusting a partial event payload.
    void useMcpStore.getState().refresh();
    return;
  }

  useMcpStore.setState((state) => ({
    servers: state.servers.map((server) =>
      server.id === serverId
        ? {
            ...server,
            status,
            error,
            // A disconnect (manual or from disabling the server) invalidates
            // the cached tool list on the Rust side too — mirror that here
            // so `mcpTools.ts` never offers a stale tool from a server that
            // isn't actually reachable anymore.
            ...(status === "disconnected" ? { tools: [], instructions: null } : {}),
          }
        : server
    ),
  }));
}).catch((error) => {
  console.error("Failed to listen for mcp://status events", error);
});
