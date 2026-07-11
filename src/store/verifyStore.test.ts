import { beforeEach, describe, expect, it, vi } from "vitest";

const invokeMock = vi.fn();
vi.mock("@tauri-apps/api/core", () => ({ invoke: (...args: unknown[]) => invokeMock(...args) }));

import { useVerifyStore, type VerifyCommand, type VerifyConfig } from "./verifyStore";

function makeCommand(overrides: Partial<VerifyCommand> = {}): VerifyCommand {
  return {
    id: "cmd-1",
    label: "Lint",
    command: "pnpm lint",
    kind: "lint",
    enabled: true,
    ...overrides,
  };
}

beforeEach(() => {
  invokeMock.mockReset();
  useVerifyStore.setState({ config: { commands: [] } });
});

describe("verifyStore.refresh", () => {
  it("calls verify_get_config and caches the result", async () => {
    const config: VerifyConfig = { commands: [makeCommand()] };
    invokeMock.mockResolvedValueOnce(config);

    await useVerifyStore.getState().refresh();

    expect(invokeMock).toHaveBeenCalledWith("verify_get_config", {});
    expect(useVerifyStore.getState().config).toEqual(config);
  });

  it("degrades to an empty config when the backend call fails (e.g. no workspace open)", async () => {
    useVerifyStore.setState({ config: { commands: [makeCommand()] } });
    invokeMock.mockRejectedValueOnce(new Error("No workspace folder is open"));

    await useVerifyStore.getState().refresh();

    expect(useVerifyStore.getState().config).toEqual({ commands: [] });
  });
});

describe("verifyStore.addCommand", () => {
  it("appends a fresh disabled command, persists via verify_set_config, then refreshes", async () => {
    invokeMock.mockResolvedValueOnce(undefined); // verify_set_config
    invokeMock.mockResolvedValueOnce({ commands: [{ ...makeCommand(), label: "", command: "", enabled: false }] }); // verify_get_config

    await useVerifyStore.getState().addCommand();

    expect(invokeMock).toHaveBeenNthCalledWith(1, "verify_set_config", {
      config: { commands: [expect.objectContaining({ label: "", command: "", enabled: false })] },
    });
    expect(invokeMock).toHaveBeenNthCalledWith(2, "verify_get_config", {});
    expect(useVerifyStore.getState().config.commands).toHaveLength(1);
  });

  it("gives each added command a distinct id", async () => {
    invokeMock.mockResolvedValue(undefined);
    const calls: VerifyConfig[] = [];
    invokeMock.mockImplementation((cmd: string, args: unknown) => {
      if (cmd === "verify_set_config") {
        const config = (args as { config: VerifyConfig }).config;
        calls.push(config);
        return Promise.resolve(undefined);
      }
      return Promise.resolve(calls[calls.length - 1] ?? { commands: [] });
    });

    await useVerifyStore.getState().addCommand();
    await useVerifyStore.getState().addCommand();

    const ids = calls[1].commands.map((c) => c.id);
    expect(new Set(ids).size).toBe(ids.length);
  });
});

describe("verifyStore.updateCommand", () => {
  it("merges the patch into the matching command only", async () => {
    useVerifyStore.setState({ config: { commands: [makeCommand({ id: "a" }), makeCommand({ id: "b", label: "Test" })] } });
    invokeMock.mockResolvedValueOnce(undefined);
    invokeMock.mockResolvedValueOnce({ commands: [] });

    await useVerifyStore.getState().updateCommand("a", { command: "pnpm lint --fix" });

    const sentConfig = invokeMock.mock.calls[0][1].config as VerifyConfig;
    expect(sentConfig.commands.find((c) => c.id === "a")?.command).toBe("pnpm lint --fix");
    expect(sentConfig.commands.find((c) => c.id === "b")?.command).toBe("pnpm lint");
  });
});

describe("verifyStore.removeCommand", () => {
  it("drops only the targeted command", async () => {
    useVerifyStore.setState({ config: { commands: [makeCommand({ id: "a" }), makeCommand({ id: "b" })] } });
    invokeMock.mockResolvedValueOnce(undefined);
    invokeMock.mockResolvedValueOnce({ commands: [] });

    await useVerifyStore.getState().removeCommand("a");

    const sentConfig = invokeMock.mock.calls[0][1].config as VerifyConfig;
    expect(sentConfig.commands.map((c) => c.id)).toEqual(["b"]);
  });
});

describe("verifyStore.toggleCommand", () => {
  it("flips enabled on the targeted command only", async () => {
    useVerifyStore.setState({
      config: { commands: [makeCommand({ id: "a", enabled: true }), makeCommand({ id: "b", enabled: false })] },
    });
    invokeMock.mockResolvedValueOnce(undefined);
    invokeMock.mockResolvedValueOnce({ commands: [] });

    await useVerifyStore.getState().toggleCommand("a");

    const sentConfig = invokeMock.mock.calls[0][1].config as VerifyConfig;
    expect(sentConfig.commands.find((c) => c.id === "a")?.enabled).toBe(false);
    expect(sentConfig.commands.find((c) => c.id === "b")?.enabled).toBe(false);
  });
});
