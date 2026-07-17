// Pure, dependency-free discovery logic for EcosystemDiscover.tsx: search,
// kind/publisher/trust filters, team-collection ordering, install-count and
// vulnerability-severity summaries, and the pre-install confirmation gate.
// Kept separate from the component so it can be unit tested directly —
// vitest here runs under a plain Node environment with no DOM, so any logic
// that must be exercised in a test has to live outside React rendering.
import type {
  InstalledPackageState,
  PackageCatalogEntry,
  VulnerabilityNotice,
  VulnerabilitySeverity,
} from "./ecosystemClient";

export type TrustFilter = "any" | "signed" | "unsigned";

export interface DiscoverFilters {
  query: string;
  kind: string;
  publisher: string;
  trust: TrustFilter;
}

export const DEFAULT_DISCOVER_FILTERS: DiscoverFilters = {
  query: "",
  kind: "any",
  publisher: "any",
  trust: "any",
};

function matchesQuery(entry: PackageCatalogEntry, normalizedQuery: string): boolean {
  if (!normalizedQuery) return true;
  const { manifest } = entry;
  return [manifest.display_name, manifest.package_id, manifest.description, manifest.kind, manifest.provenance.publisher]
    .some((value) => value.toLowerCase().includes(normalizedQuery));
}

export function filterCatalogEntries(
  entries: PackageCatalogEntry[],
  filters: DiscoverFilters,
): PackageCatalogEntry[] {
  const normalizedQuery = filters.query.trim().toLowerCase();
  return entries.filter((entry) => {
    if (filters.kind !== "any" && entry.manifest.kind !== filters.kind) return false;
    if (filters.publisher !== "any" && entry.manifest.provenance.publisher !== filters.publisher) return false;
    if (filters.trust === "signed" && !entry.trust?.signed) return false;
    if (filters.trust === "unsigned" && entry.trust?.signed) return false;
    return matchesQuery(entry, normalizedQuery);
  });
}

export function distinctPublishers(entries: PackageCatalogEntry[]): string[] {
  return [...new Set(entries.map((entry) => entry.manifest.provenance.publisher))].sort((left, right) =>
    left.localeCompare(right));
}

export function distinctKinds(entries: PackageCatalogEntry[]): string[] {
  return [...new Set(entries.map((entry) => entry.manifest.kind))].sort((left, right) => left.localeCompare(right));
}

/** Team-approved collections first, then everything else, each internally
 * sorted by package id so the layout stays stable across re-renders. */
export function splitTeamCollectionsFirst(
  entries: PackageCatalogEntry[],
  installedById: Map<string, InstalledPackageState>,
): { teamCollections: PackageCatalogEntry[]; rest: PackageCatalogEntry[] } {
  const teamCollections: PackageCatalogEntry[] = [];
  const rest: PackageCatalogEntry[] = [];
  for (const entry of entries) {
    const installed = installedById.get(entry.manifest.package_id);
    if (entry.manifest.kind === "collection" && installed?.team_approved && !installed.tombstoned) {
      teamCollections.push(entry);
    } else {
      rest.push(entry);
    }
  }
  const byPackageId = (left: PackageCatalogEntry, right: PackageCatalogEntry) =>
    left.manifest.package_id.localeCompare(right.manifest.package_id);
  return { teamCollections: teamCollections.sort(byPackageId), rest: rest.sort(byPackageId) };
}

/** Local-only install count for one catalog entry, or null when this device
 * has never installed it. There is no hosted install telemetry in this app. */
export function localInstallCountFor(
  entry: PackageCatalogEntry,
  installedById: Map<string, InstalledPackageState>,
): number | null {
  return installedById.get(entry.manifest.package_id)?.local_install_count ?? null;
}

const SEVERITY_RANK: Record<VulnerabilitySeverity, number> = {
  low: 0,
  medium: 1,
  high: 2,
  critical: 3,
};

/** Highest-severity manifest-declared notice for an entry, or null when it
 * declares none. Manifest-declared only — there is no live CVE feed. */
export function worstVulnerabilitySeverity(notices: VulnerabilityNotice[] | undefined): VulnerabilitySeverity | null {
  if (!notices || notices.length === 0) return null;
  return notices.reduce<VulnerabilitySeverity>(
    (worst, notice) => (SEVERITY_RANK[notice.severity] > SEVERITY_RANK[worst] ? notice.severity : worst),
    notices[0].severity,
  );
}

/** Gates the pre-install confirmation dialog's Approve button: the install
 * call must never fire before a preview has been loaded for the exact
 * package/version being approved AND the user has explicitly ticked the
 * "I reviewed this" checkbox — either condition failing keeps it disabled. */
export function canApproveInstall(hasPreview: boolean, reviewedByUser: boolean): boolean {
  return hasPreview && reviewedByUser;
}
