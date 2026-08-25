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

const DIGEST_A = "a".repeat(64);
const DIGEST_B = "b".repeat(64);

function verifiedRegistryRecord() {
  return {
    source: {
      source_id: "team",
      display_name: "Team registry",
      location: "https://registry.example/index.json",
      added_unix_ms: 1,
    },
    verified: {
      snapshot: {
        schema_version: 1,
        registry_id: "team-registry",
        sequence: 4,
        generated_unix_ms: 1,
        refresh_after_unix_ms: 2,
        expires_unix_ms: Date.now() + 60_000,
        packages: {
          "extension.com.example.echo": [
            { version: "1.1.0", bundle_sha256: DIGEST_A, manifest_sha256: DIGEST_B },
          ],
        },
        revocations: [],
        signature: { trust_root_id: "root", key_id: "key", algorithm: "ed25519", signature_hex: "11" },
      },
      verified_unix_ms: 3,
      snapshot_sha256: "c".repeat(64),
    },
    last_verification_error: null,
  };
}

function installedExtension() {
  return {
    active_version: "1.0.0",
    manifest: { extension_id: "com.example.echo", publisher: "Example" },
    trust: {
      state: "verified",
      reason: "ok",
      trust_root_id: "root",
      key_id: "key",
      manifest_sha256: DIGEST_A,
      component_sha256: DIGEST_A,
    },
  };
}

function safePreviewWithHostBinding() {
  return {
    source_path: "little-monkey-marketplace:v2:00000000-0000-0000-0000-000000000001",
    source_digest: DIGEST_A,
    manifest: { extension_id: "com.example.echo", version: "1.1.0", publisher: "Example" },
    trust: {
      state: "verified",
      reason: "ok",
      trust_root_id: "root",
      key_id: "key",
      manifest_sha256: DIGEST_B,
      component_sha256: DIGEST_A,
    },
    compatible: true,
    compatibility_reason: null,
    permissions: [
      {
        permission_id: "workspace.read",
        granted: true,
        binding_label: "/repo",
      },
    ],
    permission_diff: { added: [], removed: [], unchanged: [], expands_authority: false },
    approval_digest: DIGEST_A,
    requires_unsigned_approval: false,
    requires_untrusted_approval: false,
    requires_high_risk_approval: false,
    blockers: [],
  };
}

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

  it("automatic-safe pauses for manual review when an existing grant has a host-only binding", async () => {
    const record = verifiedRegistryRecord();
    const installed = installedExtension();
    const preview = safePreviewWithHostBinding();
    mocks.listRegistrySources.mockResolvedValue([record]);
    mocks.listExtensions.mockResolvedValue([installed]);
    mocks.previewMarketplaceInstall.mockImplementation(async (registry, entry) => ({
      registry,
      entry,
      source_path: preview.source_path,
      runtime_preview: preview,
    }));
    mocks.previewUpdate.mockResolvedValue(preview);

    useExtensionMarketplaceStore.setState({ updatePolicy: "automatic_safe" });
    await useExtensionMarketplaceStore.getState().runUpdateCycle();

    expect(mocks.previewMarketplaceInstall).toHaveBeenCalledTimes(1);
    expect(mocks.previewUpdate).toHaveBeenCalledTimes(1);
    expect(mocks.update).not.toHaveBeenCalled();
    expect(useExtensionMarketplaceStore.getState().updates[0]?.reasons.join(" ")).toMatch(/host-bound permission/);
  });
});
