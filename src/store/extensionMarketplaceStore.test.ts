import { beforeEach, describe, expect, it, vi } from "vitest";

const mocks = vi.hoisted(() => ({
  invoke: vi.fn(),
  listRegistrySources: vi.fn(),
  listExtensions: vi.fn(),
  previewMarketplaceInstall: vi.fn(),
  previewUpdate: vi.fn(),
  update: vi.fn(),
  install: vi.fn(),
  discover: vi.fn(),
}));

vi.mock("@tauri-apps/api/core", () => ({ invoke: mocks.invoke }));
vi.mock("../lib/ecosystemClient", () => ({
  ecosystemClient: { listRegistrySources: mocks.listRegistrySources },
}));
vi.mock("../lib/executableExtensionsClient", () => ({
  executableExtensionsClient: {
    list: mocks.listExtensions,
    previewUpdate: mocks.previewUpdate,
    update: mocks.update,
    install: mocks.install,
    discover: mocks.discover,
  },
}));
vi.mock("../lib/extensionMarketplace", async (importOriginal) => {
  const actual = await importOriginal<typeof import("../lib/extensionMarketplace")>();
  return { ...actual, previewMarketplaceInstall: mocks.previewMarketplaceInstall };
});

import { useExtensionMarketplaceStore } from "./extensionMarketplaceStore";

describe("extension marketplace update policy", () => {
  beforeEach(() => {
    for (const mock of Object.values(mocks)) mock.mockReset();
    mocks.invoke.mockResolvedValue([]);
    mocks.listRegistrySources.mockResolvedValue([]);
    mocks.listExtensions.mockResolvedValue([]);
    useExtensionMarketplaceStore.setState({
      registryRecords: [],
      registries: [],
      catalog: [],
      catalogConflicts: [],
      expiredRegistrySourceIds: [],
      installed: [],
      updates: [],
      pendingPreview: null,
      loading: false,
      error: null,
      notice: null,
      updatePolicy: "notify",
    });
  });

  it("off performs no recurring registry refresh and never stages executable bytes", async () => {
    useExtensionMarketplaceStore.setState({ updatePolicy: "off" });
    await useExtensionMarketplaceStore.getState().runUpdateCycle();

    expect(mocks.invoke).not.toHaveBeenCalled();
    expect(mocks.previewMarketplaceInstall).not.toHaveBeenCalled();
    expect(mocks.update).not.toHaveBeenCalled();
    expect(mocks.listRegistrySources).toHaveBeenCalledTimes(1);
    expect(mocks.listExtensions).toHaveBeenCalledTimes(1);
  });

  it("notify refreshes native signed metadata but never downloads an executable", async () => {
    useExtensionMarketplaceStore.setState({ updatePolicy: "notify" });
    await useExtensionMarketplaceStore.getState().runUpdateCycle();

    expect(mocks.invoke).toHaveBeenCalledWith("extensions_list", { refreshMarketplace: true });
    expect(mocks.previewMarketplaceInstall).not.toHaveBeenCalled();
    expect(mocks.previewUpdate).not.toHaveBeenCalled();
    expect(mocks.update).not.toHaveBeenCalled();
  });
});
