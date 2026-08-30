import {
  compareSemver,
  extensionEntriesFromRegistries,
  type ExtensionRegistryEntry,
  type MarketplaceRegistry,
} from "./extensionMarketplace";

export interface MarketplaceCatalogConflict {
  extension_id: string;
  version: string;
  source_ids: string[];
  reason: string;
}

export interface ResolvedMarketplaceCatalog {
  entries: ExtensionRegistryEntry[];
  conflicts: MarketplaceCatalogConflict[];
  expired_source_ids: string[];
}

function usableRegistries(registries: MarketplaceRegistry[], nowMs: number): {
  usable: MarketplaceRegistry[];
  expired: string[];
} {
  const usable: MarketplaceRegistry[] = [];
  const expired: string[] = [];
  for (const registry of registries) {
    if (registry.snapshot.expires_unix_ms <= nowMs) {
      expired.push(registry.record.source.source_id);
      continue;
    }
    usable.push(registry);
  }
  return { usable, expired: expired.sort() };
}

/**
 * Resolves the executable catalog fail-closed across every currently verified
 * M4 registry. The highest version wins only when all sources advertising that
 * exact identity agree on both immutable package and manifest digests.
 *
 * A disagreement never falls back to an older version: doing so would hide a
 * signed supply-chain conflict from the user. Identical mirrors are deduped
 * deterministically by source id.
 */
export function resolveMarketplaceCatalog(
  registries: MarketplaceRegistry[],
  nowMs = Date.now(),
): ResolvedMarketplaceCatalog {
  const { usable, expired } = usableRegistries(registries, nowMs);
  const byExtension = new Map<string, ExtensionRegistryEntry[]>();
  for (const entry of extensionEntriesFromRegistries(usable)) {
    if (entry.revoked) continue;
    byExtension.set(entry.extension_id, [...(byExtension.get(entry.extension_id) ?? []), entry]);
  }

  const entries: ExtensionRegistryEntry[] = [];
  const conflicts: MarketplaceCatalogConflict[] = [];
  for (const [extensionId, candidates] of byExtension) {
    const versions = [...new Set(candidates.map((entry) => entry.version))]
      .sort((left, right) => compareSemver(right, left));
    const latestVersion = versions[0];
    if (!latestVersion) continue;
    const latest = candidates.filter((entry) => entry.version === latestVersion);
    const identities = new Set(latest.map((entry) => `${entry.package_sha256}:${entry.manifest_sha256}`));
    if (identities.size > 1) {
      conflicts.push({
        extension_id: extensionId,
        version: latestVersion,
        source_ids: [...new Set(latest.map((entry) => entry.registry_source_id))].sort(),
        reason: "verified registries disagree on immutable package/manifest digests for the latest release",
      });
      continue;
    }
    entries.push([...latest].sort((left, right) => left.registry_source_id.localeCompare(right.registry_source_id))[0]);
  }

  entries.sort((left, right) => left.extension_id.localeCompare(right.extension_id));
  conflicts.sort((left, right) => left.extension_id.localeCompare(right.extension_id));
  return { entries, conflicts, expired_source_ids: expired };
}
