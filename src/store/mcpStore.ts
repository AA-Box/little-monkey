import { create } from "zustand";
import { invoke, isTauri } from "@tauri-apps/api/core";
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
  /** Whether this server currently has OAuth-derived credentials saved (see
   * `src-tauri/src/mcp_oauth.rs`) — never the credentials themselves.
   * Always `false` for `stdio` servers. When both this and `hasHttpToken`
   * are `true`, the backend prefers the OAuth-derived token. */
  hasOauth: boolean;
}

/** Whether the backend has actually observed an authentication failure for a
 * credential-free HTTP server. HTTP transport alone is not evidence that a
 * server is protected: public MCP endpoints are valid and must not be shown as
 * broken merely because they have no saved token. */
export function mcpServerNeedsAuthentication(server: McpServerInfo): boolean {
  if (
    server.transport.type !== "http" ||
    server.status !== "error" ||
    server.hasOauth ||
    server.hasHttpToken ||
    !server.error
  ) {
    return false;
  }

  return /(?:\b401\b|\b403\b|\bunauthori[sz]ed\b|\bforbidden\b|\bauthentication (?:is )?required\b|\bauthorization (?:is )?required\b|\b(?:missing|invalid|expired) (?:access |bearer |oauth )?token\b)/i.test(
    server.error,
  );
}

/** Progress phase of an in-flight (or just-finished) `oauthConnect` call —
 * mirrors the string values `mcp_oauth.rs`'s `emit_progress` produces via
 * the `mcp-oauth://status` event. `"idle"` is a frontend-only value (never
 * emitted by the backend) used before any connect attempt has been made for
 * a server this session. */
export type McpOAuthPhase =
  | "idle"
  | "discovering"
  | "needs_client_id"
  | "opening_browser"
  | "waiting_for_browser"
  | "exchanging_token"
  | "connected"
  | "error"
  | "cancelled";

export interface McpOAuthStatus {
  phase: McpOAuthPhase;
  error: string | null;
}

/** Payload of the `mcp-oauth://status` Tauri event emitted by
 * `src-tauri/src/mcp_oauth.rs::emit_progress`. */
interface McpOAuthStatusEvent {
  serverId: string;
  phase: Exclude<McpOAuthPhase, "idle">;
  error: string | null;
}

/** Payload of the `hosted-oauth://status` Tauri event emitted by
 * `src-tauri/src/hosted_oauth.rs::emit_progress` — same shape as
 * `McpOAuthStatusEvent`/`McpOAuthPhase`, but for the broker-hosted flow, which
 * needs a deployed service holding OAuth client secrets and is therefore not
 * wired into any connector card in a public build (Slack/Google Drive/Gmail
 * connect with the user's own OAuth app through `oauthConnect` instead — see
 * `docs/byo-oauth-clients.md`). Kept for builds that run their own broker.
 * Reuses `McpOAuthPhase`: the phases that
 * actually appear on this event are a subset (never `"needs_client_id"`,
 * since the Worker either has both providers' credentials configured or it
 * doesn't — there's no per-connect manual client id step). */
interface HostedOAuthStatusEvent {
  serverId: string;
  phase: Exclude<McpOAuthPhase, "idle">;
  error: string | null;
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
  /** Live progress of an in-flight/last `oauthConnect` call, keyed by server id — updated by `mcp-oauth://status` events. */
  oauthStatus: Record<string, McpOAuthStatus>;
  /** Runs a full generic MCP-spec OAuth 2.0 connect for an HTTP server (RFC 8414 discovery, RFC 7591 dynamic client registration or the user's own `clientId`/`clientSecret` as a fallback, PKCE, opening the system browser, awaiting the loopback redirect). Progress streams via `oauthStatus`; this promise resolves once the flow finishes (or rejects on failure/cancellation).
   *
   * `clientSecret` is only for providers that authenticate the client at the
   * token endpoint (Google's installed-app clients do; most MCP-native
   * providers don't). Both are remembered in the OS keychain by the backend,
   * so omitting them on a later connect for the same server reuses what was
   * saved. */
  oauthConnect: (id: string, clientId?: string, clientSecret?: string) => Promise<void>;
  /** The loopback redirect URI `oauthConnect` will use for `id` — what the user
   * registers with their provider when bringing their own OAuth app. Stable per
   * server id (see `loopback_port_for` in `mcp_oauth.rs`), so registering it
   * once is enough, and computable without any network call or saved
   * credential. */
  oauthRedirectUri: (id: string) => Promise<string>;
  /** Cancels an in-flight `oauthConnect` for `id`. A no-op if none is running. */
  oauthCancel: (id: string) => Promise<void>;
  /** Disconnects the live MCP transport, then clears this HTTP server's saved
   * OAuth credentials from the keychain. If credential removal fails, the
   * transport stays disconnected and the saved-credential state is retained. */
  oauthDisconnect: (id: string) => Promise<void>;
  /** Materializes a bundled MCP server's embedded source (see
   * `src-tauri/src/bundled_mcp_servers.rs`) under the app data directory and
   * returns its absolute path — used by `McpPanel`'s quick-add templates
   * that need a real local file (e.g. the AppleScript-control server)
   * rather than an externally installed command. */
  stageBundledServer: (id: string) => Promise<string>;
  /** Live progress of an in-flight/last `hostedOauthConnect` call, keyed by
   * server id — updated by `hosted-oauth://status` events. Separate from
   * `oauthStatus` since a given server id only ever uses one of the two
   * flows, but nothing stops both maps existing side by side. */
  hostedOauthStatus: Record<string, McpOAuthStatus>;
  /** Starts the broker-hosted OAuth flow for `id` against `provider`
   * (`"slack"` or `"google"`) — opens the system browser on the
   * provider's real login page and returns as soon as that succeeds. Rejects
   * outright on a build whose broker client ids are still placeholders, which
   * is every public build; nothing in the UI calls this today.
   * Completion streams later via `hostedOauthStatus`/`hosted-oauth://status`
   * events (there's no local redirect listener to await here, unlike
   * `oauthConnect` — see `hosted_oauth.rs`'s module doc). */
  hostedOauthConnect: (id: string, provider: "slack" | "google") => Promise<void>;
  /** Drops a pending `hostedOauthConnect` attempt for `id` so a later,
   * now-unwanted deep-link callback is ignored. There's no in-flight task
   * to interrupt (unlike `oauthCancel`) — this only resets local/pending
   * state. */
  hostedOauthCancel: (id: string) => Promise<void>;
  /** Clears an HTTP server's saved hosted-OAuth credentials from the
   * keychain. */
  hostedOauthDisconnect: (id: string) => Promise<void>;
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

  oauthStatus: {},

  oauthConnect: async (id, clientId, clientSecret) => {
    // `mcp-oauth://status` events (see the listener below) are the source
    // of truth for phase transitions while this is in flight; this just
    // seeds an immediate "discovering" phase so the UI doesn't sit blank
    // for the brief window before the first event arrives.
    set((state) => ({ oauthStatus: { ...state.oauthStatus, [id]: { phase: "discovering", error: null } } }));
    await invoke("mcp_oauth_connect", {
      server_id: id,
      client_id: clientId ?? null,
      client_secret: clientSecret ?? null,
    });
  },

  oauthRedirectUri: async (id) => {
    return await invoke<string>("mcp_oauth_redirect_uri", { server_id: id });
  },

  oauthCancel: async (id) => {
    await invoke("mcp_oauth_cancel", { server_id: id });
  },

  oauthDisconnect: async (id) => {
    // Credentials can back an already-running transport. Stop that transport
    // first so "Disconnect OAuth" cannot leave a usable authenticated
    // connection alive after the UI says it was disconnected.
    await invoke("mcp_disconnect", { server_id: id });
    set((state) => ({
      servers: state.servers.map((server) =>
        server.id === id
          ? {
              ...server,
              status: "disconnected",
              error: null,
              tools: [],
              instructions: null,
            }
          : server,
      ),
    }));

    try {
      await invoke("mcp_oauth_disconnect", { server_id: id });
    } catch (error) {
      // The transport is definitely stopped, but credentials may still exist.
      // Refresh best-effort so the UI keeps that distinction truthful without
      // hiding the original keychain error.
      await get().refresh().catch(() => {});
      throw error;
    }

    set((state) => {
      const { [id]: _removed, ...rest } = state.oauthStatus;
      return {
        oauthStatus: rest,
        servers: state.servers.map((server) =>
          server.id === id ? { ...server, hasOauth: false } : server,
        ),
      };
    });
    await get().refresh();
  },

  stageBundledServer: async (id) => {
    return await invoke<string>("mcp_stage_bundled_server", { id });
  },

  hostedOauthStatus: {},

  hostedOauthConnect: async (id, provider) => {
    set((state) => ({
      hostedOauthStatus: { ...state.hostedOauthStatus, [id]: { phase: "opening_browser", error: null } },
    }));
    await invoke("hosted_oauth_connect", { server_id: id, provider });
  },

  hostedOauthCancel: async (id) => {
    await invoke("hosted_oauth_cancel", { server_id: id });
  },

  hostedOauthDisconnect: async (id) => {
    await invoke("hosted_oauth_disconnect", { server_id: id });
    set((state) => {
      const { [id]: _removed, ...rest } = state.hostedOauthStatus;
      return { hostedOauthStatus: rest };
    });
    await get().refresh();
  },
}));

// Tauri-shell only: in plain-browser dev `listen` itself throws.
if (isTauri()) {
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

  void listen<McpOAuthStatusEvent>("mcp-oauth://status", (event) => {
    const { serverId, phase, error } = event.payload;
    useMcpStore.setState((state) => ({
      oauthStatus: { ...state.oauthStatus, [serverId]: { phase, error } },
    }));
    if (phase === "connected") {
      // Picks up the just-saved OAuth credentials as `hasOauth: true` — the
      // event itself doesn't carry the server's full config/status.
      void useMcpStore.getState().refresh();
    }
  }).catch((error) => {
    console.error("Failed to listen for mcp-oauth://status events", error);
  });

  void listen<HostedOAuthStatusEvent>("hosted-oauth://status", (event) => {
    const { serverId, phase, error } = event.payload;
    useMcpStore.setState((state) => ({
      hostedOauthStatus: { ...state.hostedOauthStatus, [serverId]: { phase, error } },
    }));
    if (phase === "connected") {
      // Picks up the just-saved hosted-OAuth credentials as `hasOauth:
      // true` — the event itself doesn't carry the server's full
      // config/status, same reasoning as the `mcp-oauth://status` handler
      // above.
      void useMcpStore.getState().refresh();
    }
  }).catch((error) => {
    console.error("Failed to listen for hosted-oauth://status events", error);
  });
}
