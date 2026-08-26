import { describe, expect, it } from "vitest";
import type { ExtensionDetail } from "./executableExtensionsClient";
import type { ExtensionRegistryEntry } from "./extensionMarketplace";
import type { InstalledPackageState, PackageCatalogEntry } from "./ecosystemClient";
import { buildUnifiedCatalog, filterUnifiedCatalog } from "./unifiedEcosystemCatalog";

function packageFixture(): PackageCatalogEntry {
  return {
    manifest: {
      schema_version: 1,
      package_id: "dev.example.connector",
      version: "1.0.0",
      kind: "connector",
      display_name: "Example Connector",
      description: "Declarative connector package",
      content: [],
      permissions: [{ permission_id: "net", kind: "network", scope: "https://api.example.com", reason: "API" }],
      mcp_requirements: [{
        requirement_id: "example-mcp",
        kind: "remote_http",
        server_id: "example",
        remote_origin: "https://mcp.example.com",
        required_tools: ["search"],
        separate_install_approval_required: true,
        separate_oauth_approval_required: true,
      }],
      provenance: {
        publisher: "Example Publisher",
        source: { curated_registry: { registry_id: "test" } },
        source_revision: "1",
        build_reproducible: true,
      },
      compatibility: {
        minimum_app_version: "1.5.0",
        maximum_app_version_exclusive: null,
        platforms: ["macos", "linux", "windows"],
        architectures: ["aarch64", "x86_64"],
      },
    },
    bundle_sha256: "a".repeat(64),
    trust: {
      signed: true,
      trust_root_id: "root",
      key_id: "key",
      registry_snapshot_sha256: "b".repeat(64),
      revocation: {},
    },
    available: true,
    validation_error: null,
  } as unknown as PackageCatalogEntry;
}

function extensionFixture(): ExtensionRegistryEntry {
  return {
    registry_source_id: "test",
    registry_display_name: "Test registry",
    registry_snapshot_sha256: "c".repeat(64),
    package_id: "extension.dev.example.tool",
    extension_id: "dev.example.tool",
    version: "1.0.0",
    package_url: "https://example.com/extensions/dev.example.tool/1.0.0.lmx",
    package_sha256: "d".repeat(64),
    manifest_sha256: "e".repeat(64),
    revoked: false,
    revocation_reason: null,
  };
}

function installedExtensionFixture(): ExtensionDetail {
  return {
    manifest: {
      schema_version: 1,
      extension_id: "dev.example.tool",
      version: "0.9.0",
      display_name: "Example Tool",
      description: "Installed tool",
      host_api: { minimum: "1.0.0", maximum_exclusive: "2.0.0" },
      component: { path: "component.wasm", sha256: "f".repeat(64) },
      capabilities: [{ capability_id: "echo", kind: "tool", display_name: "Echo", description: "Echo", input_schema: {} }],
      permissions: [],
      config_schema: [],
      secret_slots: [],
      dependencies: [],
      compatibility: { minimum_app_version: "1.5.0", maximum_app_version_exclusive: null, platforms: ["macos"], architectures: ["aarch64"] },
      publisher: "WASM Publisher",
      provenance: { publisher: "WASM Publisher", source: { curated_registry: { registry_id: "test" } }, source_revision: "0.9.0", build_reproducible: true },
      signature: null,
      checksums: { "component.wasm": "f".repeat(64) },
    },
    trust: { state: "verified", reason: "test", trust_root_id: "root", key_id: "key", manifest_sha256: "f".repeat(64), component_sha256: "f".repeat(64) },
    installed_source: { curated_registry: { registry_id: "test" } },
    compatible: true,
    compatibility_reason: null,
    permissions: [],
    secret_slots: [],
    config: {},
    health: { state: "healthy", validated: true, enabled: true, running: true, consecutive_failures: 0, trap_count: 0, undeclared_attempts: 0, last_error: null, last_invocation_at_ms: null },
    active_version: "0.9.0",
    previous_version: null,
    available_versions: ["0.9.0"],
    update_available: false,
    allowed_actions: [],
    blockers: [],
  };
}

describe("unified ecosystem catalog", () => {
  it("normalizes declarative packages, WASM extensions and MCP integrations into one browse list", () => {
    const entries = buildUnifiedCatalog({
      packages: [packageFixture()],
      installedPackages: [] as InstalledPackageState[],
      extensions: [extensionFixture()],
      installedExtensions: [installedExtensionFixture()],
    });

    expect(entries.map((entry) => entry.kind).sort()).toEqual(["mcp", "package", "wasm"]);
    const wasm = entries.find((entry) => entry.kind === "wasm")!;
    expect(wasm.publisher).toBe("WASM Publisher");
    expect(wasm.updateState).toBe("update_available");
    expect(wasm.securityBoundary).toContain("WASM");

    const mcp = entries.find((entry) => entry.kind === "mcp")!;
    expect(mcp.capabilities).toContain("search");
    expect(mcp.permissions).toContain("Separate OAuth approval required");
    expect(mcp.securityBoundary).toContain("External MCP");
  });

  it("searches across type-specific normalized metadata", () => {
    const entries = buildUnifiedCatalog({
      packages: [packageFixture()],
      installedPackages: [],
      extensions: [extensionFixture()],
      installedExtensions: [installedExtensionFixture()],
    });
    expect(filterUnifiedCatalog(entries, "OAuth", "all").map((entry) => entry.kind)).toEqual(["mcp"]);
    expect(filterUnifiedCatalog(entries, "WASM Publisher", "wasm")).toHaveLength(1);
    expect(filterUnifiedCatalog(entries, "", "package")).toHaveLength(1);
  });

  it("never presents reserved extension registry rows as declarative packages", () => {
    const reserved = packageFixture();
    reserved.manifest.package_id = "extension.dev.example.tool";
    const entries = buildUnifiedCatalog({
      packages: [reserved],
      installedPackages: [],
      extensions: [extensionFixture()],
      installedExtensions: [],
    });
    expect(entries.filter((entry) => entry.kind === "package")).toHaveLength(0);
    expect(entries.filter((entry) => entry.kind === "wasm")).toHaveLength(1);
  });
});
