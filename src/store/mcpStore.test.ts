import { beforeEach, describe, expect, it, vi } from "vitest";

const invokeMock = vi.fn();
// `vi.mock` factories are hoisted above every other statement in the file,
// so the handler `listen` receives (captured at mcpStore.ts's module-eval
// time, during the `import` below) must be stashed via `vi.hoisted` rather
// than a plain outer-scope variable — a normal `let`/`var` closed over by
// the factory is a *different* binding than the one this file's test bodies
// read later, since Vitest's hoisting transform isolates them.
const statusHandlerRef = vi.hoisted(() => ({ current: null as ((event: { payload: unknown }) => void) | null }));
const oauthStatusHandlerRef = vi.hoisted(() => ({ current: null as ((event: { payload: unknown }) => void) | null }));
vi.mock("@tauri-apps/api/core", () => ({ invoke: (...args: unknown[]) => invokeMock(...args), isTauri: () => true }));
vi.mock("@tauri-apps/api/event", () => ({
  listen: (name: string, handler: (event: { payload: unknown }) => void) => {
    if (name === "mcp-oauth://status") {
      oauthStatusHandlerRef.current = handler;
    } else {
      statusHandlerRef.current = handler;
    }
    return Promise.resolve(() => {});
  },
}));

import { useMcpStore, type McpServerInfo } from "./mcpStore";

function makeInfo(overrides: Partial<McpServerInfo> = {}): McpServerInfo {
  return {
    id: "srv",
    label: "Test Server",
    transport: { type: "stdio", command: "echo", args: [], env: {} },
    enabled: true,
    toolAllowlist: null,
    timeoutSecs: null,
    status: "disconnected",
    error: null,
    tools: [],
    instructions: null,
    hasHttpToken: false,
    hasOauth: false,
    ...overrides,
  };
}

beforeEach(() => {
  invokeMock.mockReset();
  useMcpStore.setState({ servers: [], oauthStatus: {} });
});

describe("mcpStore.refresh", () => {
  it("calls mcp_list_servers and stores the result", async () => {
    const info = makeInfo();
    invokeMock.mockResolvedValueOnce([info]);

    await useMcpStore.getState().refresh();

    expect(invokeMock).toHaveBeenCalledWith("mcp_list_servers");
    expect(useMcpStore.getState().servers).toEqual([info]);
  });
});

describe("mcpStore CRUD actions", () => {
  it("addServer invokes mcp_add_server then refreshes", async () => {
    const entry = {
      id: "srv",
      label: "Test",
      transport: { type: "stdio" as const, command: "echo", args: [], env: {} },
      enabled: true,
      tool_allowlist: null,
      timeout_secs: null,
    };
    invokeMock.mockResolvedValueOnce(entry).mockResolvedValueOnce([makeInfo()]);

    await useMcpStore.getState().addServer(entry);

    expect(invokeMock).toHaveBeenNthCalledWith(1, "mcp_add_server", { entry });
    expect(invokeMock).toHaveBeenNthCalledWith(2, "mcp_list_servers");
    expect(useMcpStore.getState().servers).toHaveLength(1);
  });

  it("removeServer invokes mcp_remove_server with server_id then refreshes", async () => {
    invokeMock.mockResolvedValueOnce(undefined).mockResolvedValueOnce([]);

    await useMcpStore.getState().removeServer("srv");

    expect(invokeMock).toHaveBeenNthCalledWith(1, "mcp_remove_server", { server_id: "srv" });
  });

  it("setEnabled invokes mcp_set_enabled with server_id and enabled then refreshes", async () => {
    invokeMock.mockResolvedValueOnce(makeInfo()).mockResolvedValueOnce([]);

    await useMcpStore.getState().setEnabled("srv", false);

    expect(invokeMock).toHaveBeenNthCalledWith(1, "mcp_set_enabled", { server_id: "srv", enabled: false });
  });

  it("connect invokes mcp_connect with server_id then refreshes", async () => {
    invokeMock.mockResolvedValueOnce(makeInfo({ status: "connected" })).mockResolvedValueOnce([]);

    await useMcpStore.getState().connect("srv");

    expect(invokeMock).toHaveBeenNthCalledWith(1, "mcp_connect", { server_id: "srv" });
  });

  it("disconnect invokes mcp_disconnect with server_id then refreshes", async () => {
    invokeMock.mockResolvedValueOnce(undefined).mockResolvedValueOnce([]);

    await useMcpStore.getState().disconnect("srv");

    expect(invokeMock).toHaveBeenNthCalledWith(1, "mcp_disconnect", { server_id: "srv" });
  });

  it("setHttpToken invokes mcp_set_http_token with server_id and token then refreshes", async () => {
    invokeMock.mockResolvedValueOnce(undefined).mockResolvedValueOnce([makeInfo({ hasHttpToken: true })]);

    await useMcpStore.getState().setHttpToken("srv", "secret-token");

    expect(invokeMock).toHaveBeenNthCalledWith(1, "mcp_set_http_token", { server_id: "srv", token: "secret-token" });
    expect(invokeMock).toHaveBeenNthCalledWith(2, "mcp_list_servers");
    expect(useMcpStore.getState().servers[0].hasHttpToken).toBe(true);
  });

  it("removeHttpToken invokes mcp_remove_http_token with server_id then refreshes", async () => {
    invokeMock.mockResolvedValueOnce(undefined).mockResolvedValueOnce([makeInfo({ hasHttpToken: false })]);

    await useMcpStore.getState().removeHttpToken("srv");

    expect(invokeMock).toHaveBeenNthCalledWith(1, "mcp_remove_http_token", { server_id: "srv" });
    expect(invokeMock).toHaveBeenNthCalledWith(2, "mcp_list_servers");
  });
});

describe("mcp://status event handling", () => {
  it("patches status/error in place for a non-connected transition", () => {
    useMcpStore.setState({ servers: [makeInfo({ status: "disconnected" })] });

    statusHandlerRef.current?.({ payload: { serverId: "srv", status: "connecting", error: null, toolCount: null } });

    expect(useMcpStore.getState().servers[0].status).toBe("connecting");
  });

  it("clears cached tools/instructions on a disconnected transition", () => {
    useMcpStore.setState({
      servers: [makeInfo({ status: "connected", tools: [{ name: "greet", description: null, inputSchema: {} }], instructions: "hi" })],
    });

    statusHandlerRef.current?.({ payload: { serverId: "srv", status: "disconnected", error: null, toolCount: null } });

    const server = useMcpStore.getState().servers[0];
    expect(server.tools).toEqual([]);
    expect(server.instructions).toBeNull();
  });

  it("records the error message on an error transition", () => {
    useMcpStore.setState({ servers: [makeInfo({ status: "connecting" })] });

    statusHandlerRef.current?.({ payload: { serverId: "srv", status: "error", error: "spawn failed", toolCount: null } });

    expect(useMcpStore.getState().servers[0].status).toBe("error");
    expect(useMcpStore.getState().servers[0].error).toBe("spawn failed");
  });

  it("triggers a full refresh on a connected transition (to pick up the cached tool list)", async () => {
    useMcpStore.setState({ servers: [makeInfo({ status: "connecting" })] });
    invokeMock.mockResolvedValueOnce([makeInfo({ status: "connected", tools: [{ name: "greet", description: null, inputSchema: {} }] })]);

    statusHandlerRef.current?.({ payload: { serverId: "srv", status: "connected", error: null, toolCount: 1 } });
    // The handler fires refresh() without awaiting it — flush microtasks.
    await Promise.resolve();
    await Promise.resolve();

    expect(invokeMock).toHaveBeenCalledWith("mcp_list_servers");
    expect(useMcpStore.getState().servers[0].tools).toHaveLength(1);
  });

  it("ignores an event for a server id not present in the current list", () => {
    useMcpStore.setState({ servers: [makeInfo({ id: "srv" })] });

    statusHandlerRef.current?.({ payload: { serverId: "ghost", status: "error", error: "boom", toolCount: null } });

    expect(useMcpStore.getState().servers).toHaveLength(1);
    expect(useMcpStore.getState().servers[0].id).toBe("srv");
  });
});

describe("mcpStore OAuth actions", () => {
  it("oauthConnect seeds a discovering phase then invokes mcp_oauth_connect with server_id and client_id", async () => {
    invokeMock.mockResolvedValueOnce(undefined);

    const promise = useMcpStore.getState().oauthConnect("srv", "byo-client-id");
    expect(useMcpStore.getState().oauthStatus.srv).toEqual({ phase: "discovering", error: null });
    await promise;

    expect(invokeMock).toHaveBeenCalledWith("mcp_oauth_connect", { server_id: "srv", client_id: "byo-client-id" });
  });

  it("oauthConnect passes client_id as null when the caller didn't supply one", async () => {
    invokeMock.mockResolvedValueOnce(undefined);

    await useMcpStore.getState().oauthConnect("srv");

    expect(invokeMock).toHaveBeenCalledWith("mcp_oauth_connect", { server_id: "srv", client_id: null });
  });

  it("oauthCancel invokes mcp_oauth_cancel with server_id", async () => {
    invokeMock.mockResolvedValueOnce(undefined);

    await useMcpStore.getState().oauthCancel("srv");

    expect(invokeMock).toHaveBeenCalledWith("mcp_oauth_cancel", { server_id: "srv" });
  });

  it("oauthDisconnect invokes mcp_oauth_disconnect, clears local oauth status, then refreshes", async () => {
    useMcpStore.setState({ oauthStatus: { srv: { phase: "connected", error: null } } });
    invokeMock.mockResolvedValueOnce(undefined).mockResolvedValueOnce([makeInfo({ hasOauth: false })]);

    await useMcpStore.getState().oauthDisconnect("srv");

    expect(invokeMock).toHaveBeenNthCalledWith(1, "mcp_oauth_disconnect", { server_id: "srv" });
    expect(invokeMock).toHaveBeenNthCalledWith(2, "mcp_list_servers");
    expect(useMcpStore.getState().oauthStatus.srv).toBeUndefined();
  });
});

describe("mcp-oauth://status event handling", () => {
  it("records phase/error transitions per server id", () => {
    oauthStatusHandlerRef.current?.({ payload: { serverId: "srv", phase: "opening_browser", error: null } });
    expect(useMcpStore.getState().oauthStatus.srv).toEqual({ phase: "opening_browser", error: null });

    oauthStatusHandlerRef.current?.({ payload: { serverId: "srv", phase: "error", error: "denied" } });
    expect(useMcpStore.getState().oauthStatus.srv).toEqual({ phase: "error", error: "denied" });
  });

  it("triggers a full refresh on a connected transition (to pick up hasOauth)", async () => {
    invokeMock.mockResolvedValueOnce([makeInfo({ hasOauth: true })]);

    oauthStatusHandlerRef.current?.({ payload: { serverId: "srv", phase: "connected", error: null } });
    await Promise.resolve();
    await Promise.resolve();

    expect(invokeMock).toHaveBeenCalledWith("mcp_list_servers");
    expect(useMcpStore.getState().servers[0]?.hasOauth).toBe(true);
  });

  it("keeps each server's oauth status independent", () => {
    oauthStatusHandlerRef.current?.({ payload: { serverId: "a", phase: "waiting_for_browser", error: null } });
    oauthStatusHandlerRef.current?.({ payload: { serverId: "b", phase: "needs_client_id", error: "no dcr" } });

    expect(useMcpStore.getState().oauthStatus.a).toEqual({ phase: "waiting_for_browser", error: null });
    expect(useMcpStore.getState().oauthStatus.b).toEqual({ phase: "needs_client_id", error: "no dcr" });
  });
});
