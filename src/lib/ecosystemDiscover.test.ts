import { describe, expect, it } from "vitest";
import {
  canApproveInstall,
  DEFAULT_DISCOVER_FILTERS,
  distinctKinds,
  distinctPublishers,
  filterCatalogEntries,
  localInstallCountFor,
  splitTeamCollectionsFirst,
  worstVulnerabilitySeverity,
} from "./ecosystemDiscover";
import type { InstalledPackageState, PackageCatalogEntry } from "./ecosystemClient";

function entry(overrides: Partial<PackageCatalogEntry["manifest"]> & { package_id: string }): PackageCatalogEntry {
  return {
    manifest: {
      schema_version: 1,
      package_id: overrides.package_id,
      version: "1.0.0",
      kind: overrides.kind ?? "skill",
      display_name: overrides.display_name ?? overrides.package_id,
      description: overrides.description ?? "A test package.",
      content: [],
      permissions: [],
      mcp_requirements: [],
      provenance: overrides.provenance ?? { publisher: "Acme", source: {}, source_revision: "rev", build_reproducible: true },
      vulnerability_notices: overrides.vulnerability_notices,
    } as PackageCatalogEntry["manifest"],
    bundle_sha256: "a".repeat(64),
    trust: overrides.package_id.includes("unsigned") ? { signed: false, trust_root_id: null, key_id: null, registry_snapshot_sha256: null, revocation: {} } : { signed: true, trust_root_id: "root", key_id: "key", registry_snapshot_sha256: "sha", revocation: {} },
    available: true,
    validation_error: null,
  };
}

function installed(packageId: string, overrides: Partial<InstalledPackageState> = {}): InstalledPackageState {
  return {
    schema_version: 1,
    sequence: 1,
    package_id: packageId,
    active_version: "1.0.0",
    versions: {},
    activation_history: ["1.0.0"],
    pinned_version: null,
    enabled: true,
    revoked: false,
    tombstoned: false,
    approved_permissions: [],
    local_install_count: 1,
    team_approved: false,
    ...overrides,
  };
}

describe("filterCatalogEntries", () => {
  const entries = [
    entry({ package_id: "com.acme.skill.review", kind: "skill", display_name: "Review", provenance: { publisher: "Acme", source: {}, source_revision: "r", build_reproducible: true } }),
    entry({ package_id: "com.other.connector.slack", kind: "connector", display_name: "Slack", provenance: { publisher: "Other", source: {}, source_revision: "r", build_reproducible: true } }),
    entry({ package_id: "com.other.unsigned.tool", kind: "skill", display_name: "Unsigned tool", provenance: { publisher: "Other", source: {}, source_revision: "r", build_reproducible: true } }),
  ];

  it("returns everything when no filters are set", () => {
    expect(filterCatalogEntries(entries, DEFAULT_DISCOVER_FILTERS)).toHaveLength(3);
  });

  it("matches search query against name, id, description and publisher", () => {
    expect(filterCatalogEntries(entries, { ...DEFAULT_DISCOVER_FILTERS, query: "slack" })).toEqual([entries[1]]);
    expect(filterCatalogEntries(entries, { ...DEFAULT_DISCOVER_FILTERS, query: "ACME" })).toEqual([entries[0]]);
  });

  it("filters by kind", () => {
    expect(filterCatalogEntries(entries, { ...DEFAULT_DISCOVER_FILTERS, kind: "connector" })).toEqual([entries[1]]);
  });

  it("filters by publisher", () => {
    const result = filterCatalogEntries(entries, { ...DEFAULT_DISCOVER_FILTERS, publisher: "Other" });
    expect(result.map((item) => item.manifest.package_id)).toEqual(["com.other.connector.slack", "com.other.unsigned.tool"]);
  });

  it("filters by trust status", () => {
    expect(filterCatalogEntries(entries, { ...DEFAULT_DISCOVER_FILTERS, trust: "unsigned" }))
      .toEqual([entries[2]]);
    expect(filterCatalogEntries(entries, { ...DEFAULT_DISCOVER_FILTERS, trust: "signed" }))
      .toEqual([entries[0], entries[1]]);
  });

  it("combines multiple filters", () => {
    const result = filterCatalogEntries(entries, { query: "tool", kind: "skill", publisher: "Other", trust: "unsigned" });
    expect(result).toEqual([entries[2]]);
  });
});

describe("distinctKinds / distinctPublishers", () => {
  const entries = [
    entry({ package_id: "a", kind: "skill", provenance: { publisher: "Zed", source: {}, source_revision: "r", build_reproducible: true } }),
    entry({ package_id: "b", kind: "connector", provenance: { publisher: "Acme", source: {}, source_revision: "r", build_reproducible: true } }),
  ];

  it("returns sorted, de-duplicated kinds and publishers", () => {
    expect(distinctKinds(entries)).toEqual(["connector", "skill"]);
    expect(distinctPublishers(entries)).toEqual(["Acme", "Zed"]);
  });
});

describe("splitTeamCollectionsFirst", () => {
  it("puts team-approved, non-tombstoned collections first", () => {
    const collectionApproved = entry({ package_id: "com.acme.collection.approved", kind: "collection" });
    const collectionUnapproved = entry({ package_id: "com.acme.collection.unapproved", kind: "collection" });
    const skill = entry({ package_id: "com.acme.skill.review", kind: "skill" });
    const installedById = new Map([
      [collectionApproved.manifest.package_id, installed(collectionApproved.manifest.package_id, { team_approved: true })],
      [collectionUnapproved.manifest.package_id, installed(collectionUnapproved.manifest.package_id, { team_approved: false })],
    ]);
    const { teamCollections, rest } = splitTeamCollectionsFirst(
      [skill, collectionUnapproved, collectionApproved],
      installedById,
    );
    expect(teamCollections).toEqual([collectionApproved]);
    expect(rest.map((item) => item.manifest.package_id)).toEqual([
      "com.acme.collection.unapproved",
      "com.acme.skill.review",
    ]);
  });

  it("never treats a tombstoned (uninstalled) collection as a team collection", () => {
    const collection = entry({ package_id: "com.acme.collection.gone", kind: "collection" });
    const installedById = new Map([
      [collection.manifest.package_id, installed(collection.manifest.package_id, { team_approved: true, tombstoned: true })],
    ]);
    const { teamCollections, rest } = splitTeamCollectionsFirst([collection], installedById);
    expect(teamCollections).toEqual([]);
    expect(rest).toEqual([collection]);
  });
});

describe("localInstallCountFor", () => {
  it("returns null for a never-installed package and the local count otherwise", () => {
    const packageEntry = entry({ package_id: "com.acme.skill.review" });
    expect(localInstallCountFor(packageEntry, new Map())).toBeNull();
    const installedById = new Map([[packageEntry.manifest.package_id, installed(packageEntry.manifest.package_id, { local_install_count: 4 })]]);
    expect(localInstallCountFor(packageEntry, installedById)).toBe(4);
  });
});

describe("worstVulnerabilitySeverity", () => {
  it("returns null when there are no notices", () => {
    expect(worstVulnerabilitySeverity(undefined)).toBeNull();
    expect(worstVulnerabilitySeverity([])).toBeNull();
  });

  it("returns the highest-ranked declared severity", () => {
    expect(worstVulnerabilitySeverity([
      { notice_id: "a", severity: "low", summary: "s", affected_versions: ["1.0.0"], advisory_url: null },
      { notice_id: "b", severity: "critical", summary: "s", affected_versions: ["1.0.0"], advisory_url: null },
      { notice_id: "c", severity: "medium", summary: "s", affected_versions: ["1.0.0"], advisory_url: null },
    ])).toBe("critical");
  });
});

describe("canApproveInstall", () => {
  it("requires both a loaded preview and an explicit review acknowledgement", () => {
    expect(canApproveInstall(false, false)).toBe(false);
    expect(canApproveInstall(true, false)).toBe(false);
    expect(canApproveInstall(false, true)).toBe(false);
    expect(canApproveInstall(true, true)).toBe(true);
  });
});
