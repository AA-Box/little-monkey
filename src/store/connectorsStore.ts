import { create } from "zustand";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { isTauri } from "@tauri-apps/api/core";
import { errorMessage } from "../lib/errors";
import { type McpOAuthPhase } from "./mcpStore";

/**
 * Mirrors the Rust `ConnectorProvider` enum (src-tauri/src/connectors.rs)
 * exactly — `#[serde(rename_all = "snake_case")]`, so each variant is its
 * lowercase string on the wire. Seventeen providers across five credential
 * schemes: `github` rides the `gh` CLI; `slack`/`notion`/`jira` take a pasted
 * token (`jira` covers Jira and Confluence on the same Atlassian API-token +
 * email scheme); `s3` covers S3 and R2 with access keys; `extension` is
 * supplied by a sandboxed executable extension and holds no credential here;
 * and the remaining eleven connect over authorization-code OAuth against an
 * app the user registers themselves (see `connector_oauth.rs`).
 */
export type ConnectorProvider =
  | "github"
  | "slack"
  | "notion"
  | "jira"
  | "s3"
  | "extension"
  | "google_drive"
  | "microsoft_graph"
  | "linear"
  | "asana"
  | "dropbox"
  | "box"
  | "airtable"
  | "zendesk"
  | "hubspot"
  | "discord"
  | "gitlab";

/** The eleven providers `connectors_oauth_connect` accepts. */
export type ConnectorOAuthProvider = Exclude<
  ConnectorProvider,
  "github" | "slack" | "notion" | "jira" | "s3" | "extension"
>;

/** Mirrors the phases `connector_oauth.rs::emit_progress` streams on
 * `connector-oauth://status`. No `discovering` — endpoints come from a static
 * table, so there is nothing to discover; `verifying` is the live read-only
 * identity call that runs before the account is saved. */
export type ConnectorOAuthPhase = Exclude<McpOAuthPhase, "discovering"> | "verifying";

export interface ConnectorOAuthStatus {
  phase: ConnectorOAuthPhase;
  error: string | null;
}

/** Payload of the `connector-oauth://status` Tauri event — a third,
 * non-colliding event name alongside `mcp-oauth://status` and
 * `hosted-oauth://status`, keyed by provider rather than by server id. */
interface ConnectorOAuthStatusEvent {
  provider: ConnectorProvider;
  phase: Exclude<ConnectorOAuthPhase, "idle">;
  error: string | null;
}

export interface OAuthConnectParams {
  provider: ConnectorOAuthProvider;
  label: string;
  /** GitLab/Zendesk instance host, or a Microsoft tenant. */
  host?: string;
  clientId?: string;
  clientSecret?: string;
}

/**
 * Mirrors the Rust `ConnectorAccount` struct exactly — plain snake_case
 * field names (no serde rename), same convention as `KnowledgeSourceV2`'s
 * `ConnectorConfig` mirroring. Never carries a secret: `credential_ref` is a
 * keychain account name, not a credential, and is `null` for GitHub (which
 * has none — identity comes from the `gh` CLI).
 */
export interface ConnectorAccount {
  id: string;
  provider: ConnectorProvider;
  label: string;
  scopes: string[];
  credential_ref: string | null;
  identity: string | null;
  created_at: number;
  last_verified_at: number | null;
  last_error: string | null;
  /** Non-secret provider metadata (Jira's site_url/email, S3's
   * endpoint/bucket/region/access_key) — never the token/secret key. */
  connection: Record<string, string> | null;
}

/** Mirrors the Rust `ConnectorAuditEntry` struct exactly — the redacted
 * shape `connectors_export_audit` returns (no `identity`, `credential_ref`,
 * or `connection`). */
export interface ConnectorAuditEntry {
  id: string;
  provider: ConnectorProvider;
  label: string;
  scopes: string[];
  created_at: number;
  last_verified_at: number | null;
  last_error: string | null;
}

export interface AddTokenParams {
  provider: Exclude<ConnectorProvider, "github" | "s3">;
  label: string;
  token: string;
  scopes: string[];
  email?: string;
  siteUrl?: string;
}

export interface AddS3Params {
  label: string;
  endpoint: string;
  bucket: string;
  region: string;
  accessKey: string;
  secretKey: string;
}

export interface ConnectorsStore {
  accounts: ConnectorAccount[];
  loading: boolean;
  error: string | null;
  refresh: () => Promise<void>;
  addGithub: (label?: string) => Promise<ConnectorAccount>;
  addToken: (params: AddTokenParams) => Promise<ConnectorAccount>;
  addS3: (params: AddS3Params) => Promise<ConnectorAccount>;
  remove: (id: string) => Promise<void>;
  reverify: (id: string) => Promise<ConnectorAccount>;
  exportAudit: () => Promise<ConnectorAuditEntry[]>;
  /** Live progress of an in-flight/last `oauthConnect`, keyed by provider —
   * updated by `connector-oauth://status` events. */
  oauthStatus: Partial<Record<ConnectorProvider, ConnectorOAuthStatus>>;
  /** The loopback redirect URI to register with `provider`. Stable, needs no
   * network call and no saved credential, so the card shows it before
   * anything is pasted. */
  oauthRedirectUri: (provider: ConnectorOAuthProvider) => Promise<string>;
  /** Runs the full authorization-code (+ PKCE, where the provider supports
   * it) connect: opens the system browser, awaits the loopback redirect,
   * exchanges the code, and proves the account with one live read-only
   * identity call before it is saved. Resolves with the saved account. */
  oauthConnect: (params: OAuthConnectParams) => Promise<ConnectorAccount>;
  /** Cancels an in-flight `oauthConnect`. A no-op if none is running. */
  oauthCancel: (provider: ConnectorOAuthProvider) => Promise<void>;
}

// Module-scoped (not store state) since it's purely an internal sequencing
// counter, never rendered or persisted — same pattern as `rulesStore.ts`'s
// `latestRefreshRequest`.
let latestRefreshRequest = 0;

export const useConnectorsStore = create<ConnectorsStore>((set, get) => ({
  accounts: [],
  loading: false,
  error: null,

  refresh: async () => {
    // Two `refresh()` calls can be in flight at once — e.g. Reverify on one
    // row and Remove on another, each doing their own mutate-then-refresh.
    // Backend IPC gives no ordering guarantee between concurrent invokes, so
    // an earlier-*started* refresh can resolve after a later one. Without a
    // sequence guard, whichever resolves last always wins the `set()` below,
    // even if it started first and is carrying a now-stale snapshot. Only
    // the most-recently-*started* call is allowed to commit its result.
    const requestId = ++latestRefreshRequest;
    set({ loading: true, error: null });
    try {
      const accounts = await invoke<ConnectorAccount[]>("connectors_list");
      if (requestId !== latestRefreshRequest) return;
      set({ accounts, loading: false });
    } catch (err) {
      if (requestId !== latestRefreshRequest) return;
      set({ error: errorMessage(err), loading: false });
    }
  },

  // `label`/`token`/`accessKey`/`secretKey` below cross the invoke()
  // boundary only — never assigned into this store's own state. Only the
  // `ConnectorAccount` the backend returns (which structurally never
  // contains a secret) is ever kept, via `refresh()`.
  addGithub: async (label) => {
    const account = await invoke<ConnectorAccount>("connectors_add_github", { label: label ?? null });
    await get().refresh();
    return account;
  },

  addToken: async ({ provider, label, token, scopes, email, siteUrl }) => {
    const account = await invoke<ConnectorAccount>("connectors_add_token", {
      provider,
      label,
      token,
      scopes,
      email: email ?? null,
      siteUrl: siteUrl ?? null,
    });
    await get().refresh();
    return account;
  },

  addS3: async ({ label, endpoint, bucket, region, accessKey, secretKey }) => {
    const account = await invoke<ConnectorAccount>("connectors_add_s3", {
      label,
      endpoint,
      bucket,
      region,
      accessKey,
      secretKey,
    });
    await get().refresh();
    return account;
  },

  remove: async (id) => {
    await invoke("connectors_remove", { id });
    await get().refresh();
  },

  reverify: async (id) => {
    const account = await invoke<ConnectorAccount>("connectors_reverify", { id });
    await get().refresh();
    return account;
  },

  exportAudit: () => invoke<ConnectorAuditEntry[]>("connectors_export_audit"),

  oauthStatus: {},

  // The Rust commands use `rename_all = "snake_case"`, so these payload keys
  // are `client_id`/`client_secret`, not camelCase. `clientId`/`clientSecret`/
  // `host` cross the invoke boundary only — never assigned into store state.
  oauthRedirectUri: (provider) =>
    invoke<string>("connectors_oauth_redirect_uri", { provider }),

  oauthConnect: async ({ provider, label, host, clientId, clientSecret }) => {
    set((state) => ({
      oauthStatus: { ...state.oauthStatus, [provider]: { phase: "opening_browser", error: null } },
    }));
    const account = await invoke<ConnectorAccount>("connectors_oauth_connect", {
      provider,
      label,
      host: host ?? null,
      client_id: clientId ?? null,
      client_secret: clientSecret ?? null,
    });
    await get().refresh();
    return account;
  },

  oauthCancel: async (provider) => {
    await invoke("connectors_oauth_cancel", { provider });
  },
}));

// Tauri-shell only: in plain-browser dev `listen` itself throws. Same
// registration idiom as `mcpStore.ts`'s `mcp-oauth://status` handler.
if (isTauri()) {
  void listen<ConnectorOAuthStatusEvent>("connector-oauth://status", (event) => {
    const { provider, phase, error } = event.payload;
    useConnectorsStore.setState((state) => ({
      oauthStatus: { ...state.oauthStatus, [provider]: { phase, error } },
    }));
  }).catch((error) => {
    console.error("Failed to listen for connector-oauth://status events", error);
  });
}
