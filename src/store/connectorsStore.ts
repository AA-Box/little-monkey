import { create } from "zustand";
import { invoke } from "@tauri-apps/api/core";

/**
 * Mirrors the Rust `ConnectorProvider` enum (src-tauri/src/connectors.rs)
 * exactly — `#[serde(rename_all = "snake_case")]`, so each variant is its
 * lowercase string on the wire. `jira` covers Jira and Confluence (same
 * Atlassian API-token + email scheme); `s3` covers S3 and R2 (same
 * access-key/secret-key scheme).
 */
export type ConnectorProvider = "github" | "slack" | "notion" | "jira" | "s3";

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
      set({ error: err instanceof Error ? err.message : String(err), loading: false });
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
}));
