import { beforeEach, describe, expect, it, vi } from "vitest";

const invokeMock = vi.fn();
// See `mcpStore.test.ts`'s comment on why the `listen` handler must be
// stashed via `vi.hoisted` rather than a plain outer-scope variable — a
// normal `let`/`var` closed over by a hoisted `vi.mock` factory is a
// *different* binding than the one this file's test bodies read later.
const statusHandlerRef = vi.hoisted(() => ({ current: null as ((event: { payload: unknown }) => void) | null }));
vi.mock("@tauri-apps/api/core", () => ({ invoke: (...args: unknown[]) => invokeMock(...args) }));
vi.mock("@tauri-apps/api/event", () => ({
  listen: (_name: string, handler: (event: { payload: unknown }) => void) => {
    statusHandlerRef.current = handler;
    return Promise.resolve(() => {});
  },
}));

import { useApiServerStore, DEFAULT_API_SERVER_STATUS, type ApiServerStatus } from "./apiServerStore";

function makeStatus(overrides: Partial<ApiServerStatus> = {}): ApiServerStatus {
  return { ...DEFAULT_API_SERVER_STATUS, ...overrides };
}

beforeEach(() => {
  invokeMock.mockReset();
  useApiServerStore.setState({
    status: DEFAULT_API_SERVER_STATUS,
    loaded: false,
    portInput: DEFAULT_API_SERVER_STATUS.port,
  });
});

describe("apiServerStore.refresh", () => {
  it("calls api_server_status and stores the result, syncing portInput", async () => {
    const status = makeStatus({ status: "running", port: 4321, token: "lmk-abc" });
    invokeMock.mockResolvedValueOnce(status);

    await useApiServerStore.getState().refresh();

    expect(invokeMock).toHaveBeenCalledWith("api_server_status");
    expect(useApiServerStore.getState().status).toEqual(status);
    expect(useApiServerStore.getState().loaded).toBe(true);
    expect(useApiServerStore.getState().portInput).toBe(4321);
  });
});

describe("apiServerStore.start/stop", () => {
  it("start invokes api_server_start with the given port and stores the returned status", async () => {
    const status = makeStatus({ status: "running", port: 5555, token: "lmk-xyz" });
    invokeMock.mockResolvedValueOnce(status);

    await useApiServerStore.getState().start(5555);

    expect(invokeMock).toHaveBeenCalledWith("api_server_start", { port: 5555 });
    expect(useApiServerStore.getState().status).toEqual(status);
  });

  it("stop invokes api_server_stop and stores the returned status", async () => {
    const status = makeStatus({ status: "stopped", token: null });
    invokeMock.mockResolvedValueOnce(status);

    await useApiServerStore.getState().stop();

    expect(invokeMock).toHaveBeenCalledWith("api_server_stop");
    expect(useApiServerStore.getState().status).toEqual(status);
  });

  it("propagates a rejected start (e.g. a port-bind conflict) without silently swallowing it", async () => {
    invokeMock.mockRejectedValueOnce(new Error("Failed to bind 127.0.0.1:1234 — address in use"));

    await expect(useApiServerStore.getState().start(1234)).rejects.toThrow("address in use");
  });
});

describe("apiServerStore.setPortInput", () => {
  it("updates portInput without touching status", () => {
    useApiServerStore.getState().setPortInput(9999);
    expect(useApiServerStore.getState().portInput).toBe(9999);
    expect(useApiServerStore.getState().status).toEqual(DEFAULT_API_SERVER_STATUS);
  });
});

describe("apiserver://status event subscription", () => {
  it("updates status live when the event fires", () => {
    const pushed = makeStatus({ status: "error", last_error: "boom" });
    expect(statusHandlerRef.current).not.toBeNull();
    statusHandlerRef.current?.({ payload: pushed });
    expect(useApiServerStore.getState().status).toEqual(pushed);
  });
});
