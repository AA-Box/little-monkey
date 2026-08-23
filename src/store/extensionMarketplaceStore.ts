import { create } from "zustand";

import {
  executableExtensionsClient,
  type ExtensionApproval,
  type ExtensionDetail,
  type PermissionGrant,
} from "../lib/executableExtensionsClient";
import {
  fetchVerifiedRegistry,
  isSafeAutomaticUpdate,
  latestEntries,
  previewMarketplaceInstall,
  type ExtensionRegistryEntry,
  type ExtensionRegistrySource,
  type MarketplaceInstallPreview,
  type VerifiedExtensionRegistry,
} from "../lib/extensionMarketplace";

const STORAGE_KEY = "little-monkey-extension-marketplace-v1";

export type ExtensionUpdatePolicy = "off" | "notify" | "automatic_safe";

interface PersistedMarketplaceState {
  sources: ExtensionRegistrySource[];
  update_policy: ExtensionUpdatePolicy;
}

interface ExtensionUpdateCandidate {
  installed: ExtensionDetail;
  entry: ExtensionRegistryEntry;
  registry: VerifiedExtensionRegistry;
  safe_auto_update: boolean;
  reasons: string[];
}

interface ExtensionMarketplaceState {
  sources: ExtensionRegistrySource[];
  registries: VerifiedExtensionRegistry[];
  catalog: ExtensionRegistryEntry[];
  installed: ExtensionDetail[];
  updates: ExtensionUpdateCandidate[];
  updatePolicy: ExtensionUpdatePolicy;
  pendingPreview: MarketplaceInstallPreview | null;
  loading: boolean;
  error: string | null;
  notice: string | null;
  hydrate: () => Promise<void>;
  addSource: (input: Omit<ExtensionRegistrySource, "added_at_ms" | "last_sequence" | "last_snapshot_sha256">) => Promise<void>;
  removeSource: (sourceId: string) => void;
  refreshSource: (sourceId: string) => Promise<void>;
  refreshAll: () => Promise<void>;
  refreshInstalled: () => Promise<void>;
  previewInstall: (entry: ExtensionRegistryEntry) => Promise<void>;
  clearPreview: () => void;
  installPending: (grants: PermissionGrant[], acknowledgeHighRisk: boolean, acknowledgeUntrusted: boolean) => Promise<void>;
  setUpdatePolicy: (policy: ExtensionUpdatePolicy) => Promise<void>;
  applySafeUpdates: () => Promise<void>;
}

function loadPersisted(): PersistedMarketplaceState {
  try {
    const raw = localStorage.getItem(STORAGE_KEY);
    if (!raw) return { sources: [], update_policy: "notify" };
    const parsed = JSON.parse(raw) as Partial<PersistedMarketplaceState>;
    return {
      sources: Array.isArray(parsed.sources) ? parsed.sources : [],
      update_policy: parsed.update_policy === "off" || parsed.update_policy === "automatic_safe" ? parsed.update_policy : "notify",
    };
  } catch {
    return { sources: [], update_policy: "notify" };
  }
}

function persist(sources: ExtensionRegistrySource[], updatePolicy: ExtensionUpdatePolicy): void {
  localStorage.setItem(STORAGE_KEY, JSON.stringify({ sources, update_policy: updatePolicy } satisfies PersistedMarketplaceState));
}

function errorMessage(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

function registryForEntry(registries: VerifiedExtensionRegistry[], entry: ExtensionRegistryEntry): VerifiedExtensionRegistry | null {
  return registries.find((registry) => registry.snapshot.entries.some((candidate) => candidate.extension_id === entry.extension_id && candidate.version === entry.version)) ?? null;
}

async function computeUpdates(
  registries: VerifiedExtensionRegistry[],
  installed: ExtensionDetail[],
): Promise<ExtensionUpdateCandidate[]> {
  const latest = latestEntries(registries);
  const output: ExtensionUpdateCandidate[] = [];
  for (const current of installed) {
    const entry = latest.find((candidate) => candidate.extension_id === current.manifest.extension_id);
    if (!entry || entry.version === current.active_version) continue;
    const registry = registryForEntry(registries, entry);
    if (!registry) continue;
    try {
      const marketplace = await previewMarketplaceInstall(registry, entry);
      const runtimePreview = await executableExtensionsClient.previewUpdate(marketplace.source_path);
      const safe = isSafeAutomaticUpdate(runtimePreview, current, entry);
      output.push({ installed: current, entry, registry, safe_auto_update: safe.safe, reasons: safe.reasons });
    } catch (error) {
      output.push({ installed: current, entry, registry, safe_auto_update: false, reasons: [errorMessage(error)] });
    }
  }
  return output;
}

function approvalForInstalledUpdate(preview: MarketplaceInstallPreview, installed: ExtensionDetail): ExtensionApproval {
  const grants: PermissionGrant[] = preview.runtime_preview.permissions
    .filter((permission) => permission.granted)
    .map((permission) => ({ permission_id: permission.permission_id, binding: permission.binding_label }));
  return {
    approval_digest: preview.runtime_preview.approval_digest,
    grants,
    allow_unsigned: false,
    allow_untrusted: false,
    allow_high_risk: false,
  };
}

export const useExtensionMarketplaceStore = create<ExtensionMarketplaceState>((set, get) => ({
  sources: [],
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
    const persisted = loadPersisted();
    set({ sources: persisted.sources, updatePolicy: persisted.update_policy });
    await get().refreshAll();
  },

  addSource: async (input) => {
    if (!input.source_id.trim() || get().sources.some((source) => source.source_id === input.source_id.trim())) {
      throw new Error("Registry source id must be unique and non-empty.");
    }
    const source: ExtensionRegistrySource = {
      ...input,
      source_id: input.source_id.trim(),
      display_name: input.display_name.trim() || input.source_id.trim(),
      index_url: input.index_url.trim(),
      public_key_base64: input.public_key_base64.trim(),
      key_id: input.key_id.trim(),
      added_at_ms: Date.now(),
      last_sequence: 0,
      last_snapshot_sha256: null,
    };
    set({ loading: true, error: null, notice: null });
    try {
      const registry = await fetchVerifiedRegistry(source);
      const trusted = { ...source, last_sequence: registry.snapshot.sequence, last_snapshot_sha256: registry.snapshot_sha256 };
      const sources = [...get().sources, trusted];
      const registries = [...get().registries.filter((item) => item.source.source_id !== trusted.source_id), { ...registry, source: trusted }];
      persist(sources, get().updatePolicy);
      set({ sources, registries, catalog: latestEntries(registries), loading: false, notice: `Verified ${trusted.display_name}.` });
      await get().refreshInstalled();
    } catch (error) {
      set({ loading: false, error: errorMessage(error) });
      throw error;
    }
  },

  removeSource: (sourceId) => {
    const sources = get().sources.filter((source) => source.source_id !== sourceId);
    const registries = get().registries.filter((registry) => registry.source.source_id !== sourceId);
    persist(sources, get().updatePolicy);
    set({ sources, registries, catalog: latestEntries(registries), updates: get().updates.filter((update) => update.registry.source.source_id !== sourceId) });
  },

  refreshSource: async (sourceId) => {
    const source = get().sources.find((candidate) => candidate.source_id === sourceId);
    if (!source) throw new Error(`Unknown registry ${sourceId}.`);
    set({ loading: true, error: null, notice: null });
    try {
      const registry = await fetchVerifiedRegistry(source);
      const trusted = { ...source, last_sequence: registry.snapshot.sequence, last_snapshot_sha256: registry.snapshot_sha256 };
      const sources = get().sources.map((candidate) => candidate.source_id === sourceId ? trusted : candidate);
      const registries = [...get().registries.filter((item) => item.source.source_id !== sourceId), { ...registry, source: trusted }];
      persist(sources, get().updatePolicy);
      set({ sources, registries, catalog: latestEntries(registries), loading: false, notice: `Verified ${trusted.display_name}.` });
      await get().refreshInstalled();
    } catch (error) {
      set({ loading: false, error: errorMessage(error) });
      throw error;
    }
  },

  refreshAll: async () => {
    set({ loading: true, error: null, notice: null });
    const registries: VerifiedExtensionRegistry[] = [];
    const nextSources: ExtensionRegistrySource[] = [];
    const errors: string[] = [];
    for (const source of get().sources) {
      if (!source.enabled) { nextSources.push(source); continue; }
      try {
        const registry = await fetchVerifiedRegistry(source);
        const trusted = { ...source, last_sequence: registry.snapshot.sequence, last_snapshot_sha256: registry.snapshot_sha256 };
        registries.push({ ...registry, source: trusted });
        nextSources.push(trusted);
      } catch (error) {
        nextSources.push(source);
        errors.push(`${source.display_name}: ${errorMessage(error)}`);
      }
    }
    persist(nextSources, get().updatePolicy);
    set({ sources: nextSources, registries, catalog: latestEntries(registries), loading: false, error: errors.length ? errors.join("\n") : null });
    await get().refreshInstalled();
    if (get().updatePolicy === "automatic_safe") await get().applySafeUpdates();
  },

  refreshInstalled: async () => {
    try {
      const installed = await executableExtensionsClient.list();
      const updates = await computeUpdates(get().registries, installed);
      set({ installed, updates });
    } catch (error) {
      set({ error: errorMessage(error) });
    }
  },

  previewInstall: async (entry) => {
    const registry = registryForEntry(get().registries, entry);
    if (!registry) throw new Error("The registry that supplied this entry is not currently verified.");
    set({ loading: true, error: null, notice: null, pendingPreview: null });
    try {
      const pendingPreview = await previewMarketplaceInstall(registry, entry);
      set({ pendingPreview, loading: false });
    } catch (error) {
      set({ loading: false, error: errorMessage(error) });
      throw error;
    }
  },

  clearPreview: () => set({ pendingPreview: null }),

  installPending: async (grants, acknowledgeHighRisk, acknowledgeUntrusted) => {
    const pending = get().pendingPreview;
    if (!pending) throw new Error("No extension install is awaiting approval.");
    if (pending.runtime_preview.requires_unsigned_approval) throw new Error("Network-delivered unsigned executable extensions are never installable from the marketplace.");
    if (pending.runtime_preview.requires_high_risk_approval && !acknowledgeHighRisk) throw new Error("Review and acknowledge the requested high-risk permissions first.");
    if (pending.runtime_preview.requires_untrusted_approval && !acknowledgeUntrusted) throw new Error("The runtime publisher is not in the local trust store; explicit acknowledgement is required.");
    const approval: ExtensionApproval = {
      approval_digest: pending.runtime_preview.approval_digest,
      grants,
      allow_unsigned: false,
      allow_untrusted: acknowledgeUntrusted,
      allow_high_risk: acknowledgeHighRisk,
    };
    set({ loading: true, error: null, notice: null });
    try {
      const installed = await executableExtensionsClient.install(pending.source_path, approval);
      set({ pendingPreview: null, loading: false, notice: `Installed ${installed.manifest.display_name} ${installed.active_version}.` });
      await get().refreshInstalled();
    } catch (error) {
      set({ loading: false, error: errorMessage(error) });
      throw error;
    }
  },

  setUpdatePolicy: async (policy) => {
    persist(get().sources, policy);
    set({ updatePolicy: policy });
    if (policy === "automatic_safe") await get().applySafeUpdates();
  },

  applySafeUpdates: async () => {
    for (const candidate of get().updates) {
      if (!candidate.safe_auto_update) continue;
      try {
        const marketplace = await previewMarketplaceInstall(candidate.registry, candidate.entry);
        const runtimePreview = await executableExtensionsClient.previewUpdate(marketplace.source_path);
        const safety = isSafeAutomaticUpdate(runtimePreview, candidate.installed, candidate.entry);
        if (!safety.safe) continue; // re-check immediately before mutation
        const approval = approvalForInstalledUpdate({ ...marketplace, runtime_preview: runtimePreview }, candidate.installed);
        await executableExtensionsClient.update(marketplace.source_path, approval);
      } catch (error) {
        set({ error: `Safe update for ${candidate.entry.display_name} failed: ${errorMessage(error)}` });
      }
    }
    await get().refreshInstalled();
  },
}));
