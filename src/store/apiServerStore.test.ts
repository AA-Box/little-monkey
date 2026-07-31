import { beforeEach, describe, expect, it, vi } from "vitest";

const invokeMock = vi.fn();
// See `mcpStore.test.ts`'s comment on why the `listen` handler must be
// stashed via `vi.hoisted` rather than a plain outer-scope variable — a
// normal `let`/`var` closed over by a hoisted `vi.mock` factory is a
// *different* binding than the one this file's test bodies read later.
const statusHandlerRef = vi.hoisted(() => ({ current: null as ((event: { payload: unknown }) => void) | null }));
vi.mock("@tauri-apps/api/core", () => ({ invoke: (...args: unknown[]) => invokeMock(...args), isTauri: () => true }));
vi.mock("@tauri-apps/api/event", () => ({
  listen: (_name: string, handler: (event: { payload: unknown }) => void) => {
    statusHandlerRef.current = handler;
    return Promise.resolve(() => {});
  },
}));

import {
  useApiServerStore,
  DEFAULT_API_SERVER_STATUS,
  DEFAULT_API_SERVER_CONFIG,
  type ApiServerConfig,
  type ApiServerStatus,
  type Backend,
  type Scope,
  type TokenEntry,
} from "./apiServerStore";

function makeStatus(overrides: Partial<ApiServerStatus> = {}): ApiServerStatus {
  return { ...DEFAULT_API_SERVER_STATUS, ...overrides };
}

function makeConfig(overrides: Partial<ApiServerConfig> = {}): ApiServerConfig {
  return { ...DEFAULT_API_SERVER_CONFIG, ...overrides };
}

function makeToken(overrides: Partial<TokenEntry> = {}): TokenEntry {
  return {
    id: "tok-1",
    label: "My IDE",
    scopes: ["chat", "models"],
    backends: ["local"],
    created_at: 1700000000000,
    last_used_at: null,
    expires_at: null,
    ...overrides,
  };
}

beforeEach(() => {
  invokeMock.mockReset();
  useApiServerStore.setState({
    status: DEFAULT_API_SERVER_STATUS,
    config: DEFAULT_API_SERVER_CONFIG,
    tokens: [],
    loaded: false,
    mintedToken: null,
  });
});

describe("apiServerStore.refresh", () => {
  it("fetches status, config, and tokens together", async () => {
    const status = makeStatus({ status: "running", port: 4321 });
    const config = makeConfig({ port: 4321, require_token: true });
    const tokens = [makeToken()];
    invokeMock.mockResolvedValueOnce(status).mockResolvedValueOnce(config).mockResolvedValueOnce(tokens);

    await useApiServerStore.getState().refresh();

    expect(invokeMock).toHaveBeenNthCalledWith(1, "api_server_status");
    expect(invokeMock).toHaveBeenNthCalledWith(2, "api_server_get_config");
    expect(invokeMock).toHaveBeenNthCalledWith(3, "api_server_list_tokens");
    expect(useApiServerStore.getState().status).toEqual(status);
    expect(useApiServerStore.getState().config).toEqual(config);
    expect(useApiServerStore.getState().tokens).toEqual(tokens);
    expect(useApiServerStore.getState().loaded).toBe(true);
  });
});

describe("apiServerStore.start/stop", () => {
  it("start invokes api_server_start with no args and stores the returned status", async () => {
    const status = makeStatus({ status: "running", port: 5555 });
    invokeMock.mockResolvedValueOnce(status);

    await useApiServerStore.getState().start();

    expect(invokeMock).toHaveBeenCalledWith("api_server_start");
    expect(useApiServerStore.getState().status).toEqual(status);
  });

  it("stop invokes api_server_stop and stores the returned status", async () => {
    const status = makeStatus({ status: "stopped" });
    invokeMock.mockResolvedValueOnce(status);

    await useApiServerStore.getState().stop();

    expect(invokeMock).toHaveBeenCalledWith("api_server_stop");
    expect(useApiServerStore.getState().status).toEqual(status);
  });

  it("propagates a rejected start (e.g. a port-bind conflict) without silently swallowing it", async () => {
    invokeMock.mockRejectedValueOnce(new Error("Failed to bind 127.0.0.1:1234 — address in use"));

    await expect(useApiServerStore.getState().start()).rejects.toThrow("address in use");
  });
});

describe("apiServerStore.setConfig", () => {
  it("persists the config and stores exactly what the backend echoes back", async () => {
    const updated = makeConfig({ port: 9999, expose_providers: true });
    invokeMock.mockResolvedValueOnce(updated);

    await useApiServerStore.getState().setConfig(updated);

    expect(invokeMock).toHaveBeenCalledWith("api_server_set_config", { config: updated });
    expect(useApiServerStore.getState().config).toEqual(updated);
  });
});

describe("apiServerStore.createToken/revokeToken", () => {
  // Split so secret scanners don't flag the fixture as a real token.
  const FAKE_TOKEN = ["lmk-", "abcdef0123456789abcdef0123456789"].join("");

  it("createToken stores the minted plaintext once and appends the entry to the token list", async () => {
    const entry = makeToken({ id: "tok-new", label: "New key" });
    invokeMock.mockResolvedValueOnce({ token: FAKE_TOKEN, entry });

    await useApiServerStore.getState().createToken("New key", ["chat"], ["local"]);

    expect(invokeMock).toHaveBeenCalledWith("api_server_create_token", {
      label: "New key",
      scopes: ["chat"],
      backends: ["local"],
      expiresAt: null,
    });
    expect(useApiServerStore.getState().mintedToken).toEqual({
      token: FAKE_TOKEN,
      entry,
    });
    expect(useApiServerStore.getState().tokens).toEqual([entry]);
  });

  it("revokeToken removes the entry from the local list on success", async () => {
    useApiServerStore.setState({ tokens: [makeToken({ id: "a" }), makeToken({ id: "b" })] });
    invokeMock.mockResolvedValueOnce(undefined);

    await useApiServerStore.getState().revokeToken("a");

    expect(invokeMock).toHaveBeenCalledWith("api_server_revoke_token", { id: "a" });
    expect(useApiServerStore.getState().tokens.map((t) => t.id)).toEqual(["b"]);
  });

  it("createToken forwards an explicit expiresAt through to the backend", async () => {
    const entry = makeToken({ id: "tok-expiring", expires_at: 1700000900000 });
    invokeMock.mockResolvedValueOnce({ token: FAKE_TOKEN, entry });

    await useApiServerStore.getState().createToken("Expiring key", ["chat"], ["local"], 1700000900000);

    expect(invokeMock).toHaveBeenCalledWith("api_server_create_token", {
      label: "Expiring key",
      scopes: ["chat"],
      backends: ["local"],
      expiresAt: 1700000900000,
    });
  });
});

describe("apiServerStore.exportAudit", () => {
  it("fetches the redacted audit log without touching any other state", async () => {
    const audit = [
      {
        id: "tok-1",
        label: "My IDE",
        scopes: ["chat"] as Scope[],
        backends: ["local"] as Backend[],
        created_at: 1,
        last_used_at: null,
        revoked_at: null,
        expires_at: null,
      },
    ];
    invokeMock.mockResolvedValueOnce(audit);

    const result = await useApiServerStore.getState().exportAudit();

    expect(invokeMock).toHaveBeenCalledWith("api_server_export_audit");
    expect(result).toEqual(audit);
  });
});

describe("apiServerStore.dismissMintedToken", () => {
  it("clears mintedToken without touching anything else", () => {
    useApiServerStore.setState({ mintedToken: { token: "lmk-x", entry: makeToken() } });
    useApiServerStore.getState().dismissMintedToken();
    expect(useApiServerStore.getState().mintedToken).toBeNull();
  });
});

describe("apiserver://status event subscription", () => {
  it("updates status live when the event fires", () => {
    const pushed = makeStatus({ status: "error", last_error: "boom" });
    expect(statusHandlerRef.current).not.toBeNull();
    statusHandlerRef.current?.({ payload: pushed });
    expect(useApiServerStore.getState().status).toEqual(pushed);
  });

  // Phase 4 addition: `last_request_at` is the other half of the "request
  // counter/last-request display" parity item — `request_count` itself was
  // already wired end to end since phase 1. Pins down that it flows through
  // the same live event subscription, not just the initial `refresh()`.
  it("carries last_request_at through to the store", () => {
    const pushed = makeStatus({ status: "running", request_count: 3, last_request_at: 1700000000000 });
    statusHandlerRef.current?.({ payload: pushed });
    expect(useApiServerStore.getState().status.last_request_at).toBe(1700000000000);
  });
});
