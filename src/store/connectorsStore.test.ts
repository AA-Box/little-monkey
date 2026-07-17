import { beforeEach, describe, expect, it, vi } from "vitest";

const invokeMock = vi.fn();
vi.mock("@tauri-apps/api/core", () => ({
  invoke: (...args: unknown[]) => invokeMock(...args),
}));

import { useConnectorsStore, type ConnectorAccount } from "./connectorsStore";

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
  useConnectorsStore.setState({ accounts: [], loading: false, error: null });
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
});
