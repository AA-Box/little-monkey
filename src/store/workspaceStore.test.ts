import { beforeEach, describe, expect, it, vi } from "vitest";

const invokeMock = vi.fn();
vi.mock("@tauri-apps/api/core", () => ({ invoke: (...args: unknown[]) => invokeMock(...args), isTauri: () => true }));

import { primaryRoot, useWorkspaceStore, type WorkspaceRootInfo } from "./workspaceStore";

function root(overrides: Partial<WorkspaceRootInfo> = {}): WorkspaceRootInfo {
  return {
    id: "/work/app",
    path: "/work/app",
    label: "app",
    is_primary: true,
    ...overrides,
  };
}

beforeEach(() => {
  invokeMock.mockReset();
  useWorkspaceStore.setState({ roots: [], recent: [], rootsVersion: 0 });
});

describe("workspaceStore.restoreRoots", () => {
  it("reattaches the folders that were open at last quit", async () => {
    const restored = [
      root(),
      root({ id: "/work/lib", path: "/work/lib", label: "lib", is_primary: false }),
    ];
    invokeMock.mockResolvedValueOnce(restored);

    await useWorkspaceStore.getState().restoreRoots();

    expect(invokeMock).toHaveBeenCalledWith("restore_workspace_roots");
    expect(useWorkspaceStore.getState().roots).toEqual(restored);
    expect(primaryRoot(useWorkspaceStore.getState().roots)?.path).toBe("/work/app");
  });

  it("bumps rootsVersion so the file tree and git status load against the restored root", async () => {
    invokeMock.mockResolvedValueOnce([root()]);

    await useWorkspaceStore.getState().restoreRoots();

    expect(useWorkspaceStore.getState().rootsVersion).toBe(1);
  });

  it("leaves the app in its no-workspace state when nothing is restorable", async () => {
    invokeMock.mockResolvedValueOnce([]);

    await useWorkspaceStore.getState().restoreRoots();

    expect(useWorkspaceStore.getState().roots).toEqual([]);
    expect(primaryRoot(useWorkspaceStore.getState().roots)).toBeNull();
  });
});
