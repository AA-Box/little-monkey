import { create } from "zustand";

import {
  ecosystemClient,
  type AdditionalRegistryRecord,
} from "../lib/ecosystemClient";
import {
  executableExtensionsClient,
  type ExtensionApproval,
  type ExtensionDetail,
  type ExtensionPreview,
  type PermissionGrant,
} from "../lib/executableExtensionsClient";
import {
  compareSemver,
  isSafeAutomaticUpdate,
  latestEntries,
  marketplaceRegistries,
  previewMarketplaceInstall,
  type ExtensionRegistryEntry,
  type MarketplaceInstallPreview,
  type MarketplaceRegistry,
} from "../lib/extensionMarketplace";

const STORAGE_KEY = "little-monkey-extension-update-policy-v1";

export type ExtensionUpdatePolicy = "off" | "notify" | "automatic_safe";
export type MarketplaceMutationMode = "install" | "update";

export interface PendingMarketplacePreview extends MarketplaceInstallPreview {
  mode: MarketplaceMutationMode;
}

export interface ExtensionUpdateCandidate {
  installed: ExtensionDetail;
  entry: ExtensionRegistryEntry;
  registry: MarketplaceRegistry;
  safe_auto_update: boolean;
  reasons: string[];
}

interface ExtensionMarketplaceState {
  registryRecords: AdditionalRegistryRecord[];
  registries: MarketplaceRegistry[];
  catalog: ExtensionRegistryEntry[];
  installed: ExtensionDetail[];
  updates: ExtensionUpdateCandidate[];
  updatePolicy: ExtensionUpdatePolicy;
  pendingPreview: PendingMarketplacePreview | null;
  loading: boolean;
  error: string | null;
  notice: string | null;
  hydrate: () => Promise<void>;
  refreshAll: () => Promise<void>;
  previewEntry: (entry: ExtensionRegistryEntry) => Promise<void>;
  clearPreview: () => void;
  applyPending: (grants: PermissionGrant[], acknowledgeHighRisk: boolean, acknowledgeUntrusted: boolean) => Promise<void>;
  setUpdatePolicy: (policy: ExtensionUpdatePolicy) => Promise<void>;
  applySafeUpdates: () => Promise<void>;
  clearMessage: () => void;
}

function loadPolicy(): ExtensionUpdatePolicy {
  try {
    const value = localStorage.getItem(STORAGE_KEY);
    return value === "off" || value === "automatic_safe" ? value : "notify";
  } catch {
    return "notify";
  }
}

function persistPolicy(policy: ExtensionUpdatePolicy): void {
  try { localStorage.setItem(STORAGE_KEY, policy); } catch { /* browser privacy mode */ }
}

function errorMessage(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

function registryForEntry(registries: MarketplaceRegistry[], entry: ExtensionRegistryEntry): MarketplaceRegistry | null {
  return registries.find((registry) =>
    registry.record.source.source_id === entry.registry_source_id
    && registry.record.verified?.snapshot_sha256 === entry.registry_snapshot_sha256
  ) ?? null;
}

function installedForEntry(installed: ExtensionDetail[], entry: ExtensionRegistryEntry): ExtensionDetail | null {
  return installed.find((extension) => extension.manifest.extension_id === entry.extension_id) ?? null;
}

function automaticApproval(preview: ExtensionPreview): ExtensionApproval | null {
  const granted = preview.permissions.filter((permission) => permission.granted);
  // binding_label is intentionally only a display label. The runtime does not
  // expose the canonical host binding, so silently reconstructing a workspace
  // grant would either narrow it incorrectly or grant a different path. Any
  // bound permission therefore remains a manual review/update.
  if (granted.some((permission) => permission.binding_label !== null)) return null;
  return {
    approval_digest: preview.approval_digest,
    grants: granted.map((permission) => ({ permission_id: permission.permission_id, binding: null })),
    allow_unsigned: false,
    allow_untrusted: false,
    allow_high_risk: false,
  };
}

async function previewUpdateCandidate(
  registry: MarketplaceRegistry,
  entry: ExtensionRegistryEntry,
  installed: ExtensionDetail,
): Promise<ExtensionUpdateCandidate> {
  try {
    const downloaded = await previewMarketplaceInstall(registry, entry);
    const runtimePreview = await executableExtensionsClient.previewUpdate(downloaded.source_path);
    const safety = isSafeAutomaticUpdate(runtimePreview, installed, entry);
    const reasons = [...safety.reasons];
    if (automaticApproval(runtimePreview) === null) reasons.push("existing host-bound permission must be reviewed manually");
    return {
      installed,
      entry,
      registry,
      safe_auto_update: safety.safe && automaticApproval(runtimePreview) !== null,
      reasons,
    };
  } catch (error) {
    return { installed, entry, registry, safe_auto_update: false, reasons: [errorMessage(error)] };
  }
}

async function computeUpdates(
  registries: MarketplaceRegistry[],
  installed: ExtensionDetail[],
): Promise<ExtensionUpdateCandidate[]> {
  const output: ExtensionUpdateCandidate[] = [];
  for (const entry of latestEntries(registries)) {
    const current = installedForEntry(installed, entry);
    if (!current || compareSemver(entry.version, current.active_version) <= 0) continue;
    const registry = registryForEntry(registries, entry);
    if (!registry) continue;
    output.push(await previewUpdateCandidate(registry, entry, current));
  }
  return output;
}

export const useExtensionMarketplaceStore = create<ExtensionMarketplaceState>((set, get) => ({
  registryRecords: [],
  registries: [],
  catalog: [],
  installed: [],
  updates: [],
  updatePolicy: "notify",
  pendingPreview: null,
  loading: false,
  error: null,
  notice: null,

  hydrate: async () => {
    set({ updatePolicy: loadPolicy() });
    await get().refreshAll();
  },

  refreshAll: async () => {
    set({ loading: true, error: null, notice: null });
    try {
      const [registryRecords, installed] = await Promise.all([
        ecosystemClient.listRegistrySources(),
        executableExtensionsClient.list(),
      ]);
      const registries = marketplaceRegistries(registryRecords);
      const catalog = latestEntries(registries);
      const updates = get().updatePolicy === "off" ? [] : await computeUpdates(registries, installed);
      set({ registryRecords, registries, catalog, installed, updates, loading: false });
      if (get().updatePolicy === "automatic_safe") await get().applySafeUpdates();
    } catch (error) {
      set({ loading: false, error: errorMessage(error) });
    }
  },

  previewEntry: async (entry) => {
    const registry = registryForEntry(get().registries, entry);
    if (!registry) throw new Error("The signed M4 registry snapshot for this release is no longer current; refresh first.");
    set({ loading: true, error: null, notice: null, pendingPreview: null });
    try {
      const downloaded = await previewMarketplaceInstall(registry, entry);
      const installed = installedForEntry(get().installed, entry);
      const mode: MarketplaceMutationMode = installed ? "update" : "install";
      const runtime_preview = installed
        ? await executableExtensionsClient.previewUpdate(downloaded.source_path)
        : downloaded.runtime_preview;
      set({ pendingPreview: { ...downloaded, runtime_preview, mode }, loading: false });
    } catch (error) {
      set({ loading: false, error: errorMessage(error) });
      throw error;
    }
  },

  clearPreview: () => set({ pendingPreview: null }),

  applyPending: async (grants, acknowledgeHighRisk, acknowledgeUntrusted) => {
    const pending = get().pendingPreview;
    if (!pending) throw new Error("No marketplace change is awaiting approval.");
    const preview = pending.runtime_preview;
    if (preview.requires_unsigned_approval) {
      throw new Error("Network-delivered unsigned executable extensions are refused by the marketplace.");
    }
    if (preview.requires_high_risk_approval && !acknowledgeHighRisk) {
      throw new Error("Review and acknowledge the requested high-risk permissions first.");
    }
    if (preview.requires_untrusted_approval && !acknowledgeUntrusted) {
      throw new Error("The extension publisher is not trusted by the executable runtime; explicit acknowledgement is required.");
    }
    const approval: ExtensionApproval = {
      approval_digest: preview.approval_digest,
      grants,
      allow_unsigned: false,
      allow_untrusted: acknowledgeUntrusted,
      allow_high_risk: acknowledgeHighRisk,
    };
    set({ loading: true, error: null, notice: null });
    try {
      const result = pending.mode === "update"
        ? await executableExtensionsClient.update(pending.source_path, approval)
        : await executableExtensionsClient.install(pending.source_path, approval);
      set({
        pendingPreview: null,
        loading: false,
        notice: `${pending.mode === "update" ? "Updated" : "Installed"} ${result.manifest.display_name} to ${result.active_version}.`,
      });
      await get().refreshAll();
    } catch (error) {
      set({ loading: false, error: errorMessage(error) });
      throw error;
    }
  },

  setUpdatePolicy: async (policy) => {
    persistPolicy(policy);
    set({ updatePolicy: policy });
    await get().refreshAll();
  },

  applySafeUpdates: async () => {
    const failures: string[] = [];
    let applied = 0;
    for (const candidate of get().updates) {
      if (!candidate.safe_auto_update) continue;
      try {
        // Re-download and re-preview immediately before mutation. A registry
        // refresh, runtime trust change, permission diff, or digest mismatch can
        // therefore turn an earlier safe result into a refusal rather than a race.
        const downloaded = await previewMarketplaceInstall(candidate.registry, candidate.entry);
        const runtimePreview = await executableExtensionsClient.previewUpdate(downloaded.source_path);
        const safety = isSafeAutomaticUpdate(runtimePreview, candidate.installed, candidate.entry);
        const approval = automaticApproval(runtimePreview);
        if (!safety.safe || !approval) continue;
        await executableExtensionsClient.update(downloaded.source_path, approval);
        applied += 1;
      } catch (error) {
        failures.push(`${candidate.entry.extension_id}: ${errorMessage(error)}`);
      }
    }
    if (applied > 0 || failures.length > 0) {
      set({
        notice: applied > 0 ? `Applied ${applied} safe executable extension update${applied === 1 ? "" : "s"}.` : get().notice,
        error: failures.length > 0 ? failures.join("\n") : get().error,
      });
    }
    // Avoid recursion: update state directly instead of calling refreshAll(),
    // which would call applySafeUpdates again while the policy is automatic.
    try {
      const installed = await executableExtensionsClient.list();
      const updates = await computeUpdates(get().registries, installed);
      set({ installed, updates });
    } catch (error) {
      set({ error: errorMessage(error) });
    }
  },

  clearMessage: () => set({ error: null, notice: null }),
}));
