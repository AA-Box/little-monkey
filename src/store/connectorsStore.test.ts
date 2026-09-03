import { beforeEach, describe, expect, it, vi } from "vitest";

const invokeMock = vi.fn();
vi.mock("@tauri-apps/api/core", () => ({
  invoke: (...args: unknown[]) => invokeMock(...args),
  // The store registers its `connector-oauth://status` listener behind this,
  // same as `mcpStore.ts` does for its own events.
  isTauri: () => true,
}));

// `vi.hoisted` because `vi.mock`'s factory is hoisted above module-scope
// consts — the same reason mcpStore's own event test does this.
const eventHandlers = vi.hoisted(() => new Map<string, (event: { payload: unknown }) => void>());
vi.mock("@tauri-apps/api/event", () => ({
  listen: (name: string, handler: (event: { payload: unknown }) => void) => {
    eventHandlers.set(name, handler);
    return Promise.resolve(() => {});
  },
}));

import {
  useConnectorsStore,
  type ConnectorAccount,
  type ConnectorOAuthStatus,
} from "./connectorsStore";

const account: ConnectorAccount = {
  id: "acct-1",
  provider: "slack",
  label: "Team Slack",
  scopes: ["channels:read"],
  credential_ref: "connector:slack:acct-1",
  identity: "botty @ acme",
  created_at: 1000,
  last_verified_at: 1000,
  last_error: null,
  connection: null,
};

beforeEach(() => {
  invokeMock.mockReset();
  useConnectorsStore.setState({ accounts: [], loading: false, error: null, oauthStatus: {} });
});

describe("connectorsStore", () => {
  it("loads the connector catalog", async () => {
    invokeMock.mockResolvedValueOnce([account]);
    await useConnectorsStore.getState().refresh();
    expect(invokeMock).toHaveBeenCalledWith("connectors_list");
    expect(useConnectorsStore.getState().accounts).toEqual([account]);
  });

  it("adds a Slack token only through the command boundary, never landing in store state", async () => {
    invokeMock.mockResolvedValueOnce(account).mockResolvedValueOnce([account]);

    await useConnectorsStore.getState().addToken({
      provider: "slack",
      label: "Team Slack",
      token: "xoxb-super-secret-value",
      scopes: ["channels:read"],
    });

    expect(invokeMock).toHaveBeenNthCalledWith(1, "connectors_add_token", {
      provider: "slack",
      label: "Team Slack",
      token: "xoxb-super-secret-value",
      scopes: ["channels:read"],
      email: null,
      siteUrl: null,
    });
    expect(invokeMock).toHaveBeenNthCalledWith(2, "connectors_list");
    expect(JSON.stringify(useConnectorsStore.getState().accounts)).not.toContain("xoxb-super-secret-value");
  });

  it("adds an S3 connector only through the command boundary", async () => {
    const s3Account: ConnectorAccount = {
      ...account,
      id: "acct-2",
      provider: "s3",
      connection: { endpoint: "https://s3.example", bucket: "my-bucket", region: "us-east-1", access_key: "AKIA" },
    };
    invokeMock.mockResolvedValueOnce(s3Account).mockResolvedValueOnce([s3Account]);

    await useConnectorsStore.getState().addS3({
      label: "Backups",
      endpoint: "https://s3.example",
      bucket: "my-bucket",
      region: "us-east-1",
      accessKey: "AKIA",
      secretKey: "super-secret-key-value",
    });

    expect(invokeMock).toHaveBeenNthCalledWith(1, "connectors_add_s3", {
      label: "Backups",
      endpoint: "https://s3.example",
      bucket: "my-bucket",
      region: "us-east-1",
      accessKey: "AKIA",
      secretKey: "super-secret-key-value",
    });
    expect(JSON.stringify(useConnectorsStore.getState().accounts)).not.toContain("super-secret-key-value");
  });

  it("connects GitHub with no token argument at all", async () => {
    const githubAccount: ConnectorAccount = { ...account, id: "acct-3", provider: "github", credential_ref: null, connection: null };
    invokeMock.mockResolvedValueOnce(githubAccount).mockResolvedValueOnce([githubAccount]);

    await useConnectorsStore.getState().addGithub("My GitHub");

    expect(invokeMock).toHaveBeenNthCalledWith(1, "connectors_add_github", { label: "My GitHub" });
  });

  it("removes a connector then reloads the catalog", async () => {
    invokeMock.mockResolvedValueOnce(undefined).mockResolvedValueOnce([]);
    await useConnectorsStore.getState().remove("acct-1");
    expect(invokeMock).toHaveBeenNthCalledWith(1, "connectors_remove", { id: "acct-1" });
    expect(invokeMock).toHaveBeenNthCalledWith(2, "connectors_list");
  });

  it("reverifies a connector then reloads the catalog", async () => {
    invokeMock.mockResolvedValueOnce(account).mockResolvedValueOnce([account]);
    await useConnectorsStore.getState().reverify("acct-1");
    expect(invokeMock).toHaveBeenNthCalledWith(1, "connectors_reverify", { id: "acct-1" });
  });

  it("exports a redacted audit report without touching store state", async () => {
    const audit = [{ id: "acct-1", provider: "slack" as const, label: "Team Slack", scopes: ["channels:read"], created_at: 1000, last_verified_at: 1000, last_error: null }];
    invokeMock.mockResolvedValueOnce(audit);
    const result = await useConnectorsStore.getState().exportAudit();
    expect(invokeMock).toHaveBeenCalledWith("connectors_export_audit");
    expect(result).toEqual(audit);
  });

  it("surfaces a refresh error without throwing", async () => {
    invokeMock.mockRejectedValueOnce(new Error("boom"));
    await useConnectorsStore.getState().refresh();
    expect(useConnectorsStore.getState().error).toBe("boom");
    expect(useConnectorsStore.getState().loading).toBe(false);
  });

  it("drops a stale refresh that resolves after a newer one, instead of overwriting fresher state", async () => {
    // Mirrors two concurrent refreshes racing (e.g. Reverify on one row and
    // Remove on another, each doing their own mutate-then-refresh): the
    // first call started earlier but its `connectors_list` read resolves
    // *later* than the second, newer call's — IPC gives no ordering
    // guarantee. The earlier-started call must not clobber the later one's
    // result once it finally resolves.
    const staleAccount: ConnectorAccount = { ...account, id: "stale" };
    const freshAccount: ConnectorAccount = { ...account, id: "fresh" };

    let resolveStaleList!: (v: ConnectorAccount[]) => void;
    let callCount = 0;

    invokeMock.mockImplementation((cmd: string) => {
      callCount += 1;
      const isFirstCall = callCount === 1;
      if (cmd === "connectors_list") {
        return isFirstCall
          ? new Promise<ConnectorAccount[]>((resolve) => {
              resolveStaleList = resolve;
            })
          : Promise.resolve([freshAccount]);
      }
      return Promise.reject(new Error(`unexpected invoke: ${cmd}`));
    });

    const stalePromise = useConnectorsStore.getState().refresh(); // starts first, hangs
    await useConnectorsStore.getState().refresh(); // starts second, resolves immediately

    expect(useConnectorsStore.getState().accounts).toEqual([freshAccount]);

    resolveStaleList([staleAccount]);
    await stalePromise;

    expect(useConnectorsStore.getState().accounts).toEqual([freshAccount]);
  });

  it("sends the snake_case payload connectors_oauth_connect expects, and keeps the client secret out of store state", async () => {
    const gitlabAccount: ConnectorAccount = {
      ...account,
      id: "acct-gl",
      provider: "gitlab",
      credential_ref: "connector-oauth:acct-gl",
      connection: { host: "gitlab.example.com" },
    };
    invokeMock.mockResolvedValueOnce(gitlabAccount).mockResolvedValueOnce([gitlabAccount]);

    await useConnectorsStore.getState().oauthConnect({
      provider: "gitlab",
      label: "Work GitLab",
      host: "gitlab.example.com",
      clientId: "client-abc",
      clientSecret: "shhh-secret",
    });

    expect(invokeMock).toHaveBeenNthCalledWith(1, "connectors_oauth_connect", {
      provider: "gitlab",
      label: "Work GitLab",
      host: "gitlab.example.com",
      client_id: "client-abc",
      client_secret: "shhh-secret",
    });
    expect(invokeMock).toHaveBeenNthCalledWith(2, "connectors_list");
    const state = JSON.stringify(useConnectorsStore.getState());
    expect(state).not.toContain("shhh-secret");
    expect(state).not.toContain("client-abc");
  });

  it("normalises an omitted host and client secret to null", async () => {
    invokeMock.mockResolvedValueOnce(account).mockResolvedValueOnce([account]);
    await useConnectorsStore.getState().oauthConnect({ provider: "linear", label: "Linear" });
    expect(invokeMock).toHaveBeenNthCalledWith(1, "connectors_oauth_connect", {
      provider: "linear",
      label: "Linear",
      host: null,
      client_id: null,
      client_secret: null,
    });
  });

  it("passes the provider through to connectors_oauth_redirect_uri and connectors_oauth_cancel", async () => {
    invokeMock.mockResolvedValueOnce("http://127.0.0.1:52001/");
    await expect(useConnectorsStore.getState().oauthRedirectUri("asana")).resolves.toBe(
      "http://127.0.0.1:52001/",
    );
    expect(invokeMock).toHaveBeenNthCalledWith(1, "connectors_oauth_redirect_uri", { provider: "asana" });

    invokeMock.mockResolvedValueOnce(undefined);
    await useConnectorsStore.getState().oauthCancel("asana");
    expect(invokeMock).toHaveBeenNthCalledWith(2, "connectors_oauth_cancel", { provider: "asana" });
  });

  it("updates oauthStatus keyed by provider from connector-oauth://status events", () => {
    const handler = eventHandlers.get("connector-oauth://status");
    expect(handler).toBeDefined();

    handler!({ payload: { provider: "dropbox", phase: "verifying", error: null } });
    expect(useConnectorsStore.getState().oauthStatus.dropbox).toEqual<ConnectorOAuthStatus>({
      phase: "verifying",
      error: null,
    });

    handler!({ payload: { provider: "dropbox", phase: "connected", error: null } });
    expect(useConnectorsStore.getState().oauthStatus.dropbox?.phase).toBe("connected");
    // Another provider's card is untouched.
    expect(useConnectorsStore.getState().oauthStatus.linear).toBeUndefined();
  });
});
