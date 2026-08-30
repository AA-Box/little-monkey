import type { ExtensionDetail } from "./executableExtensionsClient";
import type { ExtensionRegistryEntry } from "./extensionMarketplace";
import type { InstalledPackageState, PackageCatalogEntry } from "./ecosystemClient";

export type UnifiedCatalogKind = "package" | "wasm" | "mcp";

export interface UnifiedCatalogEntry {
  id: string;
  kind: UnifiedCatalogKind;
  name: string;
  publisher: string | null;
  version: string;
  description: string;
  capabilities: string[];
  compatibility: string;
  trust: string;
  permissions: string[];
  updateState: "available" | "installed" | "update_available" | "needs_setup" | "revoked";
  securityBoundary: string;
  sourceId: string;
  registryName: string | null;
  metadataComplete: boolean;
}

interface McpRequirementLike {
  requirement_id?: unknown;
  kind?: unknown;
  server_id?: unknown;
  remote_origin?: unknown;
  required_tools?: unknown;
  separate_install_approval_required?: unknown;
  separate_oauth_approval_required?: unknown;
}

interface CompatibilityLike {
  minimum_app_version?: unknown;
  maximum_app_version_exclusive?: unknown;
  platforms?: unknown;
  architectures?: unknown;
}

function stringList(value: unknown): string[] {
  return Array.isArray(value) ? value.filter((item): item is string => typeof item === "string") : [];
}

function compatibilityLabel(value: unknown): string {
  if (!value || typeof value !== "object") return "Compatibility verified by the owning installer";
  const compatibility = value as CompatibilityLike;
  const platforms = stringList(compatibility.platforms);
  const minimum = typeof compatibility.minimum_app_version === "string" ? compatibility.minimum_app_version : null;
  const maximum = typeof compatibility.maximum_app_version_exclusive === "string" ? compatibility.maximum_app_version_exclusive : null;
  const version = minimum ? `app >=${minimum}${maximum ? `, <${maximum}` : ""}` : "host-version checked";
  return `${version}${platforms.length ? ` · ${platforms.join(", ")}` : ""}`;
}

function packageCapabilities(entry: PackageCatalogEntry): string[] {
  const capabilities = new Set<string>([entry.manifest.kind]);
  for (const content of entry.manifest.content ?? []) capabilities.add(content.kind);
  if (entry.manifest.mcp_requirements?.length) capabilities.add("mcp");
  return [...capabilities];
}

function packageEntry(
  entry: PackageCatalogEntry,
  installedById: Map<string, InstalledPackageState>,
): UnifiedCatalogEntry {
  const installed = installedById.get(entry.manifest.package_id);
  const current = installed?.active_version;
  const updateState = current
    ? current === entry.manifest.version ? "installed" : "update_available"
    : "available";
  return {
    id: `package:${entry.manifest.package_id}@${entry.manifest.version}`,
    kind: "package",
    name: entry.manifest.display_name,
    publisher: entry.manifest.provenance.publisher,
    version: entry.manifest.version,
    description: entry.manifest.description,
    capabilities: packageCapabilities(entry),
    compatibility: compatibilityLabel((entry.manifest as Record<string, unknown>).compatibility),
    trust: entry.trust?.signed ? "Signed package" : "Unsigned/local package",
    permissions: (entry.manifest.permissions ?? []).map((permission) => `${permission.kind}: ${permission.scope}`),
    updateState,
    securityBoundary: "Declarative package — no executable payload",
    sourceId: entry.manifest.package_id,
    registryName: null,
    metadataComplete: true,
  };
}

function wasmEntry(
  entry: ExtensionRegistryEntry,
  installedById: Map<string, ExtensionDetail>,
): UnifiedCatalogEntry {
  const installed = installedById.get(entry.extension_id);
  const installedManifest = installed?.manifest;
  const isCurrent = installed?.active_version === entry.version;
  const publisher = installedManifest?.publisher ?? null;
  return {
    id: `wasm:${entry.registry_source_id}:${entry.extension_id}@${entry.version}`,
    kind: "wasm",
    name: installedManifest?.display_name ?? entry.extension_id,
    publisher,
    version: entry.version,
    description: installedManifest?.description
      ?? "Executable WebAssembly extension. Publisher, capabilities and requested permissions are resolved from the signed extension manifest by the native runtime before installation.",
    capabilities: installedManifest?.capabilities.map((capability) => capability.kind) ?? ["wasm extension"],
    compatibility: installedManifest
      ? compatibilityLabel(installedManifest.compatibility)
      : "Verified by native extension preview before install",
    trust: entry.revoked ? "Revoked by signed registry" : "M4 registry signed; runtime signature verified on review",
    permissions: installed?.permissions.map((permission) => `${permission.kind}: ${permission.scope}`)
      ?? ["Resolved from the signed extension manifest on review"],
    updateState: entry.revoked ? "revoked" : installed ? (isCurrent ? "installed" : "update_available") : "available",
    securityBoundary: "Sandboxed WASM component — explicit runtime permission grants",
    sourceId: entry.extension_id,
    registryName: entry.registry_display_name,
    metadataComplete: Boolean(installedManifest),
  };
}

function mcpEntries(entry: PackageCatalogEntry): UnifiedCatalogEntry[] {
  const rawRequirements = (entry.manifest.mcp_requirements ?? []) as McpRequirementLike[];
  return rawRequirements.flatMap((requirement) => {
    const requirementId = typeof requirement.requirement_id === "string" ? requirement.requirement_id : null;
    if (!requirementId) return [];
    const serverId = typeof requirement.server_id === "string" ? requirement.server_id : null;
    const origin = typeof requirement.remote_origin === "string" ? requirement.remote_origin : null;
    const requiredTools = stringList(requirement.required_tools);
    const kind = typeof requirement.kind === "string" ? requirement.kind : "mcp";
    const permissions = [
      requirement.separate_install_approval_required === true ? "Separate MCP setup approval required" : null,
      requirement.separate_oauth_approval_required === true ? "Separate OAuth approval required" : null,
    ].filter((item): item is string => item !== null);
    return [{
      id: `mcp:${entry.manifest.package_id}:${requirementId}`,
      kind: "mcp" as const,
      name: serverId ?? requirementId,
      publisher: entry.manifest.provenance.publisher,
      version: entry.manifest.version,
      description: origin
        ? `Remote MCP integration at ${origin}`
        : `MCP integration required by ${entry.manifest.display_name}`,
      capabilities: requiredTools.length ? requiredTools : [kind],
      compatibility: compatibilityLabel((entry.manifest as Record<string, unknown>).compatibility),
      trust: entry.trust?.signed ? "Requirement declared by signed package" : "Package requirement; MCP authority remains separate",
      permissions: permissions.length ? permissions : ["MCP setup remains a separate user action"],
      updateState: "needs_setup" as const,
      securityBoundary: "External MCP process/server — not executed in the WASM sandbox",
      sourceId: requirementId,
      registryName: null,
      metadataComplete: true,
    }];
  });
}

export interface UnifiedCatalogInput {
  packages: PackageCatalogEntry[];
  installedPackages: InstalledPackageState[];
  extensions: ExtensionRegistryEntry[];
  installedExtensions: ExtensionDetail[];
}

/**
 * Normalizes all ecosystem discovery surfaces without merging their authority.
 * The returned entries share a browse/search shape, while `kind` remains the
 * mandatory dispatch key into the existing package, WASM, or MCP security path.
 */
export function buildUnifiedCatalog(input: UnifiedCatalogInput): UnifiedCatalogEntry[] {
  const installedPackages = new Map(input.installedPackages.map((item) => [item.package_id, item]));
  const installedExtensions = new Map(input.installedExtensions.map((item) => [item.manifest.extension_id, item]));
  const output: UnifiedCatalogEntry[] = [];
  for (const entry of input.packages) {
    // `extension.` is the reserved immutable-artifact namespace. Those records
    // are represented by the executable-extension catalog, never as declarative packages.
    if (!entry.manifest.package_id.startsWith("extension.")) output.push(packageEntry(entry, installedPackages));
    output.push(...mcpEntries(entry));
  }
  for (const entry of input.extensions) output.push(wasmEntry(entry, installedExtensions));
  return output.sort((left, right) => left.name.localeCompare(right.name) || left.kind.localeCompare(right.kind));
}

export function filterUnifiedCatalog(
  entries: UnifiedCatalogEntry[],
  query: string,
  kind: UnifiedCatalogKind | "all",
): UnifiedCatalogEntry[] {
  const needle = query.trim().toLocaleLowerCase();
  return entries.filter((entry) => {
    if (kind !== "all" && entry.kind !== kind) return false;
    if (!needle) return true;
    return [
      entry.name,
      entry.publisher ?? "",
      entry.version,
      entry.description,
      entry.capabilities.join(" "),
      entry.permissions.join(" "),
      entry.registryName ?? "",
      entry.sourceId,
    ].join(" ").toLocaleLowerCase().includes(needle);
  });
}
