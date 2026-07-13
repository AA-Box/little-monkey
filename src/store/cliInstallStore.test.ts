import { beforeEach, describe, expect, it, vi } from "vitest";

const invokeMock = vi.fn();
vi.mock("@tauri-apps/api/core", () => ({ invoke: (...args: unknown[]) => invokeMock(...args), isTauri: () => true }));

import { useCliInstallStore, type CliInstallStatus } from "./cliInstallStore";

function makeStatus(overrides: Partial<CliInstallStatus> = {}): CliInstallStatus {
  return {
    enabled: true,
    bundled: true,
    installed: true,
    install_path: "/Users/x/.local/bin/monkey",
    on_path: true,
    error: null,
    ...overrides,
  };
}

beforeEach(() => {
  invokeMock.mockReset();
  useCliInstallStore.setState({
    status: { enabled: true, bundled: false, installed: false, install_path: null, on_path: false, error: null },
    loaded: false,
    updating: false,
  });
});

describe("cliInstallStore.refresh", () => {
  it("calls cli_install_status and stores the result", async () => {
    const status = makeStatus();
    invokeMock.mockResolvedValueOnce(status);

    await useCliInstallStore.getState().refresh();

    expect(invokeMock).toHaveBeenCalledWith("cli_install_status");
    expect(useCliInstallStore.getState().status).toEqual(status);
    expect(useCliInstallStore.getState().loaded).toBe(true);
  });
});

describe("cliInstallStore.setEnabled", () => {
  it("calls cli_install_set_enabled with the new value and stores the returned status", async () => {
    const status = makeStatus({ enabled: false, bundled: false, installed: false, install_path: null, on_path: false });
    invokeMock.mockResolvedValueOnce(status);

    await useCliInstallStore.getState().setEnabled(false);

    expect(invokeMock).toHaveBeenCalledWith("cli_install_set_enabled", { enabled: false });
    expect(useCliInstallStore.getState().status).toEqual(status);
  });

  it("sets updating true while in flight and false afterward, even on failure", async () => {
    let resolveInvoke: (value: CliInstallStatus) => void;
    invokeMock.mockReturnValueOnce(
      new Promise<CliInstallStatus>((resolve) => {
        resolveInvoke = resolve;
      }),
    );

    const promise = useCliInstallStore.getState().setEnabled(true);
    expect(useCliInstallStore.getState().updating).toBe(true);

    resolveInvoke!(makeStatus());
    await promise;
    expect(useCliInstallStore.getState().updating).toBe(false);
  });

  it("clears updating even when the backend call throws", async () => {
    invokeMock.mockRejectedValueOnce(new Error("permission denied"));

    await expect(useCliInstallStore.getState().setEnabled(false)).rejects.toThrow("permission denied");
    expect(useCliInstallStore.getState().updating).toBe(false);
  });
});
