import { describe, expect, it } from "vitest";

import type { AdditionalRegistryRecord } from "./ecosystemClient";
import type { ExtensionDetail, ExtensionPreview } from "./executableExtensionsClient";
import {
  compareSemver,
  extensionArtifactUrl,
  extensionEntriesFromRegistries,
  isSafeAutomaticUpdate,
  latestEntries,
  marketplaceRegistries,
  validateLmxEnvelope,
  type ExtensionRegistryEntry,
  type LmxEnvelope,
} from "./extensionMarketplace";
import { resolveMarketplaceCatalog } from "./marketplaceCatalog";

const DIGEST_A = "a".repeat(64);
const DIGEST_B = "b".repeat(64);

function registryRecord(overrides: Partial<AdditionalRegistryRecord> = {}): AdditionalRegistryRecord {
  const record: AdditionalRegistryRecord = {
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
          "ordinary.skill": [{ version: "9.9.9", bundle_sha256: DIGEST_A, manifest_sha256: DIGEST_B }],
          "extension.com.example.echo": [
            { version: "1.0.0", bundle_sha256: DIGEST_A, manifest_sha256: DIGEST_B },
            { version: "1.2.0", bundle_sha256: DIGEST_B, manifest_sha256: DIGEST_A },
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
  return { ...record, ...overrides };
}

describe("extension marketplace M4 bridge", () => {
  it("only treats the reserved extension namespace as executable catalog entries", () => {
    const registries = marketplaceRegistries([registryRecord()]);
    const entries = extensionEntriesFromRegistries(registries);
    expect(entries.map((entry) => `${entry.extension_id}@${entry.version}`)).toEqual([
      "com.example.echo@1.2.0",
      "com.example.echo@1.0.0",
    ]);
    expect(latestEntries(registries).map((entry) => entry.version)).toEqual(["1.2.0"]);
  });

  it("derives immutable artifact paths beside the existing static registry", () => {
    expect(extensionArtifactUrl("https://registry.example/catalog/index.json", "com.example.echo", "1.2.0"))
      .toBe("https://registry.example/catalog/extensions/com.example.echo/1.2.0.lmx");
  });

  it("compares canonical semantic versions numerically", () => {
    expect(compareSemver("1.10.0", "1.9.9")).toBeGreaterThan(0);
    expect(compareSemver("2.0.0", "2.0.0")).toBe(0);
  });

  it("rejects traversal and case-colliding .lmx paths", () => {
    const envelope = {
      schema_version: 1,
      manifest: { extension_id: "com.example.echo", version: "1.0.0", component: { path: "component.wasm" } },
      files_base64: { "../component.wasm": "AA==" },
    } as unknown as LmxEnvelope;
    expect(() => validateLmxEnvelope(envelope)).toThrow(/Unsafe \.lmx path/);

    const colliding = {
      schema_version: 1,
      manifest: { extension_id: "com.example.echo", version: "1.0.0", component: { path: "component.wasm" } },
      files_base64: { "component.wasm": "AA==", "Component.wasm": "AA==" },
    } as unknown as LmxEnvelope;
    expect(() => validateLmxEnvelope(colliding)).toThrow(/colliding/);
  });

  it("fails closed when verified registries disagree on immutable bytes for the newest version", () => {
    const first = registryRecord();
    const second = registryRecord({
      source: {
        source_id: "other",
        display_name: "Other registry",
        location: "https://other.example/index.json",
        added_unix_ms: 1,
      },
      verified: {
        ...registryRecord().verified!,
        snapshot_sha256: "d".repeat(64),
        snapshot: {
          ...registryRecord().verified!.snapshot,
          registry_id: "other-registry",
          packages: {
            "extension.com.example.echo": [
              { version: "1.2.0", bundle_sha256: DIGEST_A, manifest_sha256: DIGEST_A },
            ],
          },
        },
      },
    });
    const resolved = resolveMarketplaceCatalog(marketplaceRegistries([first, second]));
    expect(resolved.entries).toEqual([]);
    expect(resolved.conflicts).toHaveLength(1);
    expect(resolved.conflicts[0]).toMatchObject({
      extension_id: "com.example.echo",
      version: "1.2.0",
      source_ids: ["other", "team"],
    });
  });

  it("never authorizes executable entries from an expired signed snapshot", () => {
    const expired = registryRecord({
      verified: {
        ...registryRecord().verified!,
        snapshot: {
          ...registryRecord().verified!.snapshot,
          expires_unix_ms: Date.now() - 1,
        },
      },
    });
    const resolved = resolveMarketplaceCatalog(marketplaceRegistries([expired]));
    expect(resolved.entries).toEqual([]);
    expect(resolved.expired_source_ids).toEqual(["team"]);
  });
});

describe("automatic extension updates", () => {
  const entry: ExtensionRegistryEntry = {
    registry_source_id: "team",
    registry_display_name: "Team",
    registry_snapshot_sha256: "c".repeat(64),
    package_id: "extension.com.example.echo",
    extension_id: "com.example.echo",
    version: "1.1.0",
    package_url: "https://registry.example/extensions/com.example.echo/1.1.0.lmx",
    package_sha256: DIGEST_A,
    manifest_sha256: DIGEST_B,
    revoked: false,
    revocation_reason: null,
  };

  function preview(overrides: Partial<ExtensionPreview> = {}): ExtensionPreview {
    return {
      source_path: "/tmp/source",
      source_digest: DIGEST_A,
      manifest: { publisher: "Example" } as ExtensionPreview["manifest"],
      trust: { state: "verified", reason: "ok", trust_root_id: "root", key_id: "key", manifest_sha256: DIGEST_B, component_sha256: DIGEST_A },
      compatible: true,
      compatibility_reason: null,
      permissions: [],
      permission_diff: { added: [], removed: [], unchanged: [], expands_authority: false },
      approval_digest: DIGEST_A,
      requires_unsigned_approval: false,
      requires_untrusted_approval: false,
      requires_high_risk_approval: false,
      blockers: [],
      ...overrides,
    };
  }

  const installed = {
    active_version: "1.0.0",
    manifest: { publisher: "Example" },
    trust: { state: "verified", reason: "ok", trust_root_id: "root", key_id: "key", manifest_sha256: DIGEST_A, component_sha256: DIGEST_A },
  } as ExtensionDetail;

  it("accepts only a newer compatible release on the same verified signing lineage", () => {
    expect(isSafeAutomaticUpdate(preview(), installed, entry)).toEqual({ safe: true, reasons: [] });
  });

  it("refuses publisher/key changes and permission expansion", () => {
    const result = isSafeAutomaticUpdate(preview({
      manifest: { publisher: "Other" } as ExtensionPreview["manifest"],
      trust: { state: "verified", reason: "ok", trust_root_id: "root", key_id: "different", manifest_sha256: DIGEST_B, component_sha256: DIGEST_A },
      permission_diff: { added: [], removed: [], unchanged: [], expands_authority: true },
    }), installed, entry);
    expect(result.safe).toBe(false);
    expect(result.reasons.join(" ")).toMatch(/publisher changed/);
    expect(result.reasons.join(" ")).toMatch(/signing lineage changed/);
    expect(result.reasons.join(" ")).toMatch(/permissions expand authority/);
  });
});
