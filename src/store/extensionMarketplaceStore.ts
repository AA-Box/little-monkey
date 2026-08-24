import { invoke } from "@tauri-apps/api/core";
import { create } from "zustand";

import {
  ecosystemClient,
  type AdditionalRegistryRecord,
  type RegistrySnapshot,
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
  marketplaceRegistries,
  previewMarketplaceInstall,
  type ExtensionRegistryEntry,
  type MarketplaceInstallPreview,
  type MarketplaceRegistry,
} from "../lib/extensionMarketplace";
import {
  resolveMarketplaceCatalog,
  type MarketplaceCatalogConflict,
} from "../lib/marketplaceCatalog";

const STORAGE_KEY = "little-monkey-extension-update-policy-v1";
const MAX_REGISTRY_SNAPSHOT_CHARS = 2 * 1024 * 1024;

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

interface WebFetchResult {
  markdown: string;
  total_chars: number;
  truncated: boolean;
  content_type: string;
}

interface ExtensionMarketplaceState {
  registryRecords: AdditionalRegistryRecord[];
  registries: MarketplaceRegistry[];
  catalog: ExtensionRegistryEntry[];
  catalogConflicts: MarketplaceCatalogConflict[];
  expiredRegistrySourceIds: string[];
  installed: ExtensionDetail[];
  updates: ExtensionUpdateCandidate[];
  updatePolicy: ExtensionUpdatePolicy;
  pendingPreview: PendingMarketplacePreview | null;
  loading: boolean;
  error: string | null;
  notice: string | null;
  hydrate: () => Promise<void>;
  refreshAll: () => Promise<void>;
  runUpdateCycle: () => Promise<void>;
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
  if (granted.some((permission) => permission.binding_label !== null)) return null;
  return {
    approval_digest: preview.approval_digest,
    grants: granted.map((permission) => ({ permission_id: permission.permission_id, binding: null })),
    allow_unsigned: false,
    allow_untrusted: false,
    allow_high_risk: false,
  };
}

function metadataUpdates(catalog: ExtensionRegistryEntry[], registries: MarketplaceRegistry[], installed: ExtensionDetail[]): ExtensionUpdateCandidate[] {
  const output: ExtensionUpdateCandidate[] = [];
  for (const entry of catalog) {
    const current = installedForEntry(installed, entry);
    if (!current || compareSemver(entry.version, current.active_version) <= 0) continue;
    const registry = registryForEntry(registries, entry);
    if (!registry) continue;
    output.push({ installed: current, entry, registry, safe_auto_update: false, reasons: ["runtime trust, compatibility and permission diff are evaluated when this release is reviewed"] });
  }
  return output;
}

function diagnosticsNotice(conflicts: MarketplaceCatalogConflict[], expired: string[]): string | null {
  const messages: string[] = [];
  if (conflicts.length > 0) messages.push(`Blocked ${conflicts.length} executable release${conflicts.length === 1 ? "" : "s"} because verified registries disagree on immutable digests.`);
  if (expired.length > 0) messages.push(`Ignored ${expired.length} expired verified registry snapshot${expired.length === 1 ? "" : "s"}; refresh must succeed before those sources can authorize installs or updates.`);
  return messages.length > 0 ? messages.join(" ") : null;
}

async function refreshRegistryMetadata(records: AdditionalRegistryRecord[]): Promise<AdditionalRegistryRecord[]> {
  const refreshed: AdditionalRegistryRecord[] = [];
  for (const record of records) {
    try {
      const result = await invoke<WebFetchResult>("tool_web_fetch", {
        url: record.source.location,
        max_chars: MAX_REGISTRY_SNAPSHOT_CHARS,
        start_index: 0,
        turn_id: null,
        tool_call_id: `marketplace-registry:${crypto.randomUUID()}`,
      });
      if (result.truncated || result.total_chars > MAX_REGISTRY_SNAPSHOT_CHARS) throw new Error("registry snapshot exceeds the marketplace metadata limit");
      if (!/^application\/(?:json|[^;]+\+json)|^text\/plain/i.test(result.content_type || "")) throw new Error(`registry snapshot must be JSON/text, received ${result.content_type || "unknown content type"}`);
      const snapshot = JSON.parse(result.markdown) as RegistrySnapshot;
      refreshed.push(await ecosystemClient.verifyRegistrySource(record.source.source_id, snapshot));
    } catch (error) {
      refreshed.push({ ...record, last_verification_error: errorMessage(error) });
    }
  }
  return refreshed;
}

async function evaluateAutomaticCandidate(candidate: ExtensionUpdateCandidate): Promise<{
  downloaded: MarketplaceInstallPreview;
  runtimePreview: ExtensionPreview;
  approval: ExtensionApproval | null;
  safe: boolean;
  reasons: string[];
}> {
  const downloaded = await previewMarketplaceInstall(candidate.registry, candidate.entry);
  const runtimePreview = await executableExtensionsClient.previewUpdate(downloaded.source_path);
  const safety = isSafeAutomaticUpdate(runtimePreview, candidate.installed, candidate.entry);
  const approval = automaticApproval(runtimePreview);
  const reasons = [...safety.reasons];
  if (approval === null) reasons.push("existing host-bound permission must be reviewed manually");
  return { downloaded, runtimePreview, approval, safe: safety.safe && approval !== null, reasons };
}

function snapshotState(records: AdditionalRegistryRecord[], installed: ExtensionDetail[], policy: ExtensionUpdatePolicy) {
  const registries = marketplaceRegistries(records);
  const resolved = resolveMarketplaceCatalog(registries);
  const updates = policy === "off" ? [] : metadataUpdates(resolved.entries, registries, installed);
  return {
    registryRecords: records,
    registries,
    catalog: resolved.entries,
    catalogConflicts: resolved.conflicts,
    expiredRegistrySourceIds: resolved.expired_source_ids,
    installed,
    updates,
    notice: diagnosticsNotice(resolved.conflicts, resolved.expired_source_ids),
  };
}

export const useExtensionMarketplaceStore = create<ExtensionMarketplaceState>((set, get) => ({
  registryRecords: [], registries: [], catalog: [], catalogConflicts: [], expiredRegistrySourceIds: [], installed: [], updates: [],
  updatePolicy: "notify", pendingPreview: null, loading: false, error: null, notice: null,

  hydrate: async () => {
    set({ updatePolicy: loadPolicy() });
    await get().refreshAll();
  },

  refreshAll: async () => {
    set({ loading: true, error: null });
    try {
      const [registryRecords, installed] = await Promise.all([ecosystemClient.listRegistrySources(), executableExtensionsClient.list()]);
      set({ ...snapshotState(registryRecords, installed, get().updatePolicy), loading: false });
    } catch (error) { set({ loading: false, error: errorMessage(error) }); }
  },

  runUpdateCycle: async () => {
    const policy = get().updatePolicy;
    if (policy === "off") { await get().refreshAll(); return; }
    set({ loading: true, error: null });
    try {
      const current = await ecosystemClient.listRegistrySources();
      const registryRecords = await refreshRegistryMetadata(current);
      const installed = await executableExtensionsClient.list();
      set({ ...snapshotState(registryRecords, installed, policy), loading: false });
      if (policy === "automatic_safe") await get().applySafeUpdates();
    } catch (error) { set({ loading: false, error: errorMessage(error) }); }
  },

  previewEntry: async (entry) => {
    const registry = registryForEntry(get().registries, entry);
    if (!registry) throw new Error("The signed M4 registry snapshot for this release is no longer current; refresh first.");
    if (registry.snapshot.expires_unix_ms <= Date.now()) throw new Error("The signed registry snapshot has expired; refresh it before installing.");
    set({ loading: true, error: null, notice: null, pendingPreview: null });
    try {
      const downloaded = await previewMarketplaceInstall(registry, entry);
      const installed = installedForEntry(get().installed, entry);
      const mode: MarketplaceMutationMode = installed ? "update" : "install";
      const runtime_preview = installed ? await executableExtensionsClient.previewUpdate(downloaded.source_path) : downloaded.runtime_preview;
      set({ pendingPreview: { ...downloaded, runtime_preview, mode }, loading: false });
    } catch (error) { set({ loading: false, error: errorMessage(error) }); throw error; }
  },

  clearPreview: () => set({ pendingPreview: null }),

  applyPending: async (grants, acknowledgeHighRisk, acknowledgeUntrusted) => {
    const pending = get().pendingPreview;
    if (!pending) throw new Error("No marketplace change is awaiting approval.");
    const currentRegistry = registryForEntry(get().registries, pending.entry);
    if (!currentRegistry || currentRegistry.snapshot.expires_unix_ms <= Date.now()) throw new Error("The signed registry authority for this preview is no longer current; refresh and review again.");
    const preview = pending.runtime_preview;
    if (preview.requires_unsigned_approval) throw new Error("Network-delivered unsigned executable extensions are refused by the marketplace.");
    if (preview.requires_high_risk_approval && !acknowledgeHighRisk) throw new Error("Review and acknowledge the requested high-risk permissions first.");
    if (preview.requires_untrusted_approval && !acknowledgeUntrusted) throw new Error("The extension publisher is not trusted by the executable runtime; explicit acknowledgement is required.");
    const approval: ExtensionApproval = { approval_digest: preview.approval_digest, grants, allow_unsigned: false, allow_untrusted: acknowledgeUntrusted, allow_high_risk: acknowledgeHighRisk };
    set({ loading: true, error: null, notice: null });
    try {
      const finalPreview = pending.mode === "update" ? await executableExtensionsClient.previewUpdate(pending.source_path) : await executableExtensionsClient.discover(pending.source_path);
      if (finalPreview.approval_digest !== preview.approval_digest) throw new Error("Executable runtime approval state changed; review the release again.");
      const result = pending.mode === "update" ? await executableExtensionsClient.update(pending.source_path, approval) : await executableExtensionsClient.install(pending.source_path, approval);
      set({ pendingPreview: null, loading: false, notice: `${pending.mode === "update" ? "Updated" : "Installed"} ${result.manifest.display_name} to ${result.active_version}.` });
      await get().refreshAll();
    } catch (error) { set({ loading: false, error: errorMessage(error) }); throw error; }
  },

  setUpdatePolicy: async (policy) => {
    persistPolicy(policy);
    set({ updatePolicy: policy });
    await get().runUpdateCycle();
  },

  applySafeUpdates: async () => {
    const failures: string[] = [];
    const reviewRequired: string[] = [];
    let applied = 0;
    for (const candidate of get().updates) {
      try {
        const currentRegistry = registryForEntry(get().registries, candidate.entry);
        if (!currentRegistry || currentRegistry.snapshot.expires_unix_ms <= Date.now()) { reviewRequired.push(`${candidate.entry.extension_id}: signed registry snapshot is no longer current`); continue; }
        const evaluated = await evaluateAutomaticCandidate(candidate);
        if (!evaluated.safe || !evaluated.approval) { reviewRequired.push(`${candidate.entry.extension_id}: ${evaluated.reasons.join("; ") || "manual review required"}`); continue; }
        const finalPreview = await executableExtensionsClient.previewUpdate(evaluated.downloaded.source_path);
        const finalSafety = isSafeAutomaticUpdate(finalPreview, candidate.installed, candidate.entry);
        const finalApproval = automaticApproval(finalPreview);
        if (!finalSafety.safe || !finalApproval || finalApproval.approval_digest !== evaluated.approval.approval_digest) { reviewRequired.push(`${candidate.entry.extension_id}: runtime trust/permission state changed before update`); continue; }
        await executableExtensionsClient.update(evaluated.downloaded.source_path, finalApproval);
        applied += 1;
      } catch (error) { failures.push(`${candidate.entry.extension_id}: ${errorMessage(error)}`); }
    }
    if (applied > 0 || failures.length > 0 || reviewRequired.length > 0) {
      set({ notice: applied > 0 ? `Applied ${applied} safe executable extension update${applied === 1 ? "" : "s"}.` : get().notice, error: failures.length > 0 ? failures.join("\n") : get().error });
    }
    try {
      const installed = await executableExtensionsClient.list();
      const updates = metadataUpdates(get().catalog, get().registries, installed).map((candidate) => {
        const detail = reviewRequired.find((reason) => reason.startsWith(`${candidate.entry.extension_id}: `));
        return detail ? { ...candidate, reasons: [detail.slice(candidate.entry.extension_id.length + 2)] } : candidate;
      });
      set({ installed, updates });
    } catch (error) { set({ error: errorMessage(error) }); }
  },

  clearMessage: () => set({ error: null, notice: null }),
}));
