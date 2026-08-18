import { create } from "zustand";
import {
  createM3OperationId,
  runtimeHubClient,
  sha256Text,
  type AgentConfigDriftReport,
  type AgentTool,
  type GeneratedAgentConfig,
  type BackendDescriptor,
  type ConformanceReport,
  type ConformanceSectionId,
  type ConversionReport,
  type ChatTemplateLabReport,
  type HardwareProfile,
  type HardwareSnapshot,
  type LanServerPolicy,
  type M3DiagnosticDispatchRequest,
  type M3ApiDispatchResponse,
  type M3DiagnosticCancelRequest,
  type M3CatalogSourceConfig,
  type M3CatalogMatch,
  type M3CleanupReport,
  type M3CompatibilityMatrixReport,
  type M3ComponentCatalogEntry,
  type M3ComponentUpdateCheck,
  type M3InstalledComponent,
  type M3InstalledModel,
  type M3HardwareCompatibilityReport,
  type M3HttpServerStatus,
  type M3LoadModelRequest,
  type M3LocalModelStalenessWarning,
  type M3RuntimeCapability,
  type M3RuntimeMetricsView,
  type M3RuntimeStatusView,
  type M3SchedulingInput,
  type M3SchedulingPlan,
  type M3SettingCapabilitiesView,
  type M3StorageStatus,
  type M3UnloadModelRequest,
  type ContextCacheView,
  type ContextFailureClassification,
  type ContextFailureInput,
  type EffectiveContextInput,
  type EffectiveContextResolution,
  type OffloadPlan,
  type OffloadPlanInput,
  type PairedToken,
  type PairingChallenge,
  type PairingRequest,
  type QuantTypeDescriptor,
  type RuntimeInventory,
  type RuntimeLogTail,
  type RuntimePrWatcherCheckResult,
  type RuntimePrWatcherState,
  type RuntimeTraceRecord,
  type ScopedToken,
  type SecurityAuditEvent,
  type SettingValue,
  type SupportBundle,
} from "../lib/runtimeHubClient";
import {
  extractModelIdFromRequestBody,
  extractSamplerStatsFromRequestBody,
  extractTokenTimingFromResponseBody,
} from "../lib/runtimeTelemetry";

export type RuntimeHubSection =
  | "overview"
  | "models"
  | "components"
  | "catalogs"
  | "runtimes"
  | "api"
  | "compatibility"
  | "lan"
  | "quantization"
  | "telemetry"
  | "benchmark"
  | "agents"
  | "upstream-watcher";

export interface RuntimeDetail {
  status?: M3RuntimeStatusView;
  inventory?: RuntimeInventory;
  logs?: RuntimeLogTail;
  metrics?: M3RuntimeMetricsView;
  config?: Record<string, SettingValue>;
  contextCache?: ContextCacheView;
  refreshedAt?: number;
}

export interface M3DownloadProgress {
  operationId: string;
  assetId: string;
  downloadedBytes: number;
  totalBytes: number;
  phase: "preparing" | "downloading" | "verifying" | "cancelling";
  startedAt: number;
}

interface RuntimeHubStoreState {
  section: RuntimeHubSection;
  hardware: HardwareSnapshot | null;
  profile: HardwareProfile | null;
  compatibilityReport: M3HardwareCompatibilityReport | null;
  storage: M3StorageStatus | null;
  installedModels: M3InstalledModel[];
  installedComponents: M3InstalledComponent[];
  componentRegistry: M3ComponentCatalogEntry[];
  componentUpdateChecks: M3ComponentUpdateCheck[];
  catalogSources: M3CatalogSourceConfig[];
  runtimes: M3RuntimeCapability[];
  runtimeDetails: Record<string, RuntimeDetail>;
  catalogQuery: string;
  catalogResults: M3CatalogMatch[];
  apiResult: M3ApiDispatchResponse | null;
  compatibilityMatrix: M3CompatibilityMatrixReport | null;
  conformanceReport: ConformanceReport | null;
  lanPolicy: LanServerPolicy | null;
  lanTokens: ScopedToken[];
  lanAudit: SecurityAuditEvent[];
  httpServerStatus: M3HttpServerStatus | null;
  pairingChallenge: PairingChallenge | null;
  pairedToken: PairedToken | null;
  agentGeneratedConfig: GeneratedAgentConfig | null;
  agentDriftReport: AgentConfigDriftReport | null;
  busy: Record<string, boolean>;
  errors: Record<string, string>;
  activeOperations: Record<string, string>;
  downloadProgress: Record<string, M3DownloadProgress>;
  cleanupReport: M3CleanupReport | null;
  schedulingPlan: M3SchedulingPlan | null;
  quantizationBackends: BackendDescriptor[];
  quantizationQuantTypes: QuantTypeDescriptor[];
  quantizationReports: ConversionReport[];
  /** Keyed by the raw `template` string a model declares (an empty string
   * stands in for "no template"/`null`) — not by `TemplateFamily`, since
   * family detection is the Rust command's job (`chat_template_lab.rs`'s
   * `TemplateFamily::detect`), not something the frontend re-implements. */
  chatTemplateLabReports: Record<string, ChatTemplateLabReport>;
  offloadPlans: Record<string, OffloadPlan>;
  /** The persisted Runtime PR Watcher report — `null` until
   * `refreshPrWatcher`/`checkPrWatcherNow` has loaded it at least once. */
  prWatcherState: RuntimePrWatcherState | null;
  /** The most recent `checkPrWatcherNow` result, kept separately from
   * `prWatcherState.relevantPrs` (which accumulates across every check) so
   * the UI can show "N new since last check" for just this run. */
  prWatcherLastResult: RuntimePrWatcherCheckResult | null;
  /** Model Retirement and Compatibility Warnings (ROADMAP.md Phase 8, item
   * 14): keyed by assetId. `null` means "checked, currently up to date";
   * absent means "not checked yet". */
  modelStalenessWarnings: Record<string, M3LocalModelStalenessWarning | null>;
  traces: RuntimeTraceRecord[];
  supportBundle: SupportBundle | null;
  /** Keyed by runtimeId: the Sampler/Batching/Speculative Decoding gating
   * result last resolved for that runtime (see `resolveSettingCapabilities`
   * below). Absent until first resolved; the UI falls back to the
   * runtime's raw, ungated `settings` list until then. */
  settingCapabilities: Record<string, M3SettingCapabilitiesView>;
  loaded: boolean;

  setSection: (section: RuntimeHubSection) => void;
  setCatalogQuery: (query: string) => void;
  clearError: (key: string) => void;
  dismissPairedToken: () => void;
  refresh: () => Promise<void>;
  refreshOverview: () => Promise<void>;
  refreshCompatibilityReport: () => Promise<void>;
  searchCatalog: (query?: string) => Promise<void>;
  downloadModel: (match: M3CatalogMatch) => Promise<void>;
  updateModel: (assetId: string, match: M3CatalogMatch) => Promise<void>;
  activateModelVersion: (assetId: string, versionKey: string) => Promise<void>;
  verifyProjector: (assetId: string, versionKey: string, candidatePath: string) => Promise<void>;
  pruneModelVersions: (assetId: string) => Promise<void>;
  deleteModel: (assetId: string) => Promise<void>;
  cleanupOrphans: () => Promise<void>;
  replaceCatalogSources: (sources: M3CatalogSourceConfig[]) => Promise<void>;
  refreshComponents: () => Promise<void>;
  installComponent: (entry: M3ComponentCatalogEntry) => Promise<void>;
  /** Fetches the trusted MLX catalog and installs its newest package when the
   * supported MLX runtime is present but has no working package. */
  ensureMlxRuntime: () => Promise<void>;
  /** Installs a signed MLX service package from a directory on this machine. */
  installMlxPackage: (packageDirectory: string) => Promise<void>;
  activateComponentVersion: (componentId: string, versionKey: string) => Promise<void>;
  replaceComponentRegistry: (entries: M3ComponentCatalogEntry[]) => Promise<void>;
  /** Folds a catalog's entries into the registry through the backend's merge, so
   *  an import adds to what this machine knows instead of replacing it. */
  mergeComponentRegistry: (entries: M3ComponentCatalogEntry[]) => Promise<void>;
  /** Fetches the catalog this project publishes and adopts it, atomically in the
   *  backend. Rejects when the catalog cannot be reached or is invalid, and in
   *  neither case has the registry been touched. */
  syncComponentCatalog: (url?: string) => Promise<void>;
  planSchedule: (input: M3SchedulingInput) => Promise<void>;
  fetchChatTemplateLabReport: (template: string | null) => Promise<void>;
  previewOffloadPlan: (runtimeId: string, input: OffloadPlanInput) => Promise<void>;
  resolveSettingCapabilities: (runtimeId: string, assetId: string | null) => Promise<void>;
  cancelOperation: (key: string) => Promise<boolean>;
  refreshRuntime: (runtimeId: string) => Promise<void>;
  checkModelStaleness: (assetId: string) => Promise<void>;
  resolveEffectiveContext: (input: EffectiveContextInput) => Promise<EffectiveContextResolution>;
  classifyContextFailure: (input: ContextFailureInput) => Promise<ContextFailureClassification | null>;
  loadModel: (request: M3LoadModelRequest) => Promise<void>;
  unloadModel: (request: M3UnloadModelRequest) => Promise<void>;
  setRuntimeConfig: (runtimeId: string, values: Record<string, SettingValue>) => Promise<void>;
  dispatchApi: (request: M3DiagnosticDispatchRequest) => Promise<void>;
  cancelInference: (request: M3DiagnosticCancelRequest) => Promise<boolean>;
  refreshCompatibilityMatrix: () => Promise<void>;
  runConformanceSuite: (
    baseUrl: string,
    token: string | null,
    sections: ConformanceSectionId[],
  ) => Promise<void>;
  clearConformanceReport: () => void;
  refreshLan: () => Promise<void>;
  validateLanPolicy: (policy: LanServerPolicy) => Promise<void>;
  configureLan: (policy: LanServerPolicy) => Promise<void>;
  disableLan: () => Promise<void>;
  beginPairing: (request: PairingRequest, remoteAddress?: string) => Promise<void>;
  completePairing: (challengeId: string, pairingCode: string, remoteAddress?: string) => Promise<void>;
  revokeToken: (tokenId: string, remoteAddress?: string) => Promise<void>;
  startHttpServer: () => Promise<void>;
  stopHttpServer: () => Promise<void>;
  storeTlsIdentity: (reference: string, certificatePem: string, privateKeyPem: string) => Promise<string>;
  refreshTraces: (runtimeId?: string | null) => Promise<void>;
  exportSupportBundle: () => Promise<SupportBundle>;
  clearSupportBundle: () => void;
  generateAgentConfig: (tool: AgentTool, modelId: string, authToken: string | null) => Promise<void>;
  clearAgentConfig: () => void;
  checkAgentConfigDrift: (tool: AgentTool, pastedConfig: string) => Promise<void>;
  clearAgentDriftReport: () => void;
  refreshQuantization: () => Promise<void>;
  convertPathQuantization: (sourcePath: string, quantChoice: string, allowRequantize: boolean) => Promise<void>;
  convertInstalledModelQuantization: (
    assetId: string,
    versionKey: string | null,
    quantChoice: string,
    allowRequantize: boolean,
  ) => Promise<void>;
  refreshPrWatcher: () => Promise<void>;
  checkPrWatcherNow: () => Promise<void>;
}

function errorMessage(error: unknown): string {
  if (error instanceof Error) return error.message;
  if (typeof error === "string") return error;
  try {
    return JSON.stringify(error);
  } catch {
    return "Unknown Runtime Hub error";
  }
}

function modelAssetId(match: M3CatalogMatch): string {
  return `${match.model.runtime}:${match.model.modelId}:${match.model.variantId}`;
}

const MLX_COMPONENT_ID = "mlx-runtime-apple-silicon";

function latestMlxComponent(entries: M3ComponentCatalogEntry[]): M3ComponentCatalogEntry | undefined {
  return entries
    .filter((entry) => entry.componentId === MLX_COMPONENT_ID && entry.kind === "mlx_runtime")
    .sort((left, right) => right.publishedAtMs - left.publishedAtMs)[0];
}

function omitKey<T>(record: Record<string, T>, key: string): Record<string, T> {
  const next = { ...record };
  delete next[key];
  return next;
}

export const useRuntimeHubStore = create<RuntimeHubStoreState>((set, get) => {
  const begin = (key: string, operationId?: string) => {
    set((state) => ({
      busy: { ...state.busy, [key]: true },
      errors: omitKey(state.errors, key),
      activeOperations: operationId
        ? { ...state.activeOperations, [key]: operationId }
        : state.activeOperations,
    }));
  };

  const fail = (key: string, error: unknown) => {
    set((state) => ({ errors: { ...state.errors, [key]: errorMessage(error) } }));
  };

  const finish = (key: string) => {
    set((state) => ({
      busy: omitKey(state.busy, key),
      activeOperations: omitKey(state.activeOperations, key),
    }));
  };

  const refreshModelState = async () => {
    const [storage, installedModels] = await Promise.all([
      runtimeHubClient.storageStatus(),
      runtimeHubClient.installedModels(),
    ]);
    set({ storage, installedModels });
  };

  const runDownload = async (key: string, match: M3CatalogMatch, assetId?: string) => {
    const operationId = createM3OperationId(assetId ? "model-update" : "model-download");
    const progressKey = assetId ?? modelAssetId(match);
    begin(key, operationId);
    set((state) => ({
      downloadProgress: {
        ...state.downloadProgress,
        [progressKey]: {
          operationId,
          assetId: progressKey,
          downloadedBytes: Math.min(state.storage?.pendingDownloadBytes ?? 0, match.model.sizeBytes),
          totalBytes: match.model.sizeBytes,
          phase: "preparing",
          startedAt: Date.now(),
        },
      },
    }));

    let polling = false;
    const timer = globalThis.setInterval(() => {
      if (polling) return;
      polling = true;
      void runtimeHubClient
        .storageStatus()
        .then((storage) => {
          set((state) => {
            const current = state.downloadProgress[progressKey];
            if (!current) return { storage };
            return {
              storage,
              downloadProgress: {
                ...state.downloadProgress,
                [progressKey]: {
                  ...current,
                  phase: "downloading",
                  downloadedBytes: Math.min(storage.pendingDownloadBytes, current.totalBytes),
                },
              },
            };
          });
        })
        .catch(() => {})
        .finally(() => {
          polling = false;
        });
    }, 400);

    try {
      const acceptedLicenseSha256 = await sha256Text(match.model.license.rawDeclaration);
      const request = { model: match.model, acceptedLicenseSha256 };
      if (assetId) {
        await runtimeHubClient.modelUpdate({ operationId, timeoutMs: null, assetId, request });
      } else {
        await runtimeHubClient.modelDownload({ operationId, timeoutMs: null, request });
      }
      set((state) => {
        const current = state.downloadProgress[progressKey];
        return current
          ? {
              downloadProgress: {
                ...state.downloadProgress,
                [progressKey]: { ...current, downloadedBytes: current.totalBytes, phase: "verifying" },
              },
            }
          : {};
      });
      await refreshModelState();
    } catch (error) {
      fail(key, error);
      throw error;
    } finally {
      globalThis.clearInterval(timer);
      set((state) => ({ downloadProgress: omitKey(state.downloadProgress, progressKey) }));
      finish(key);
    }
  };

  return {
    section: "overview",
    hardware: null,
    profile: null,
    compatibilityReport: null,
    storage: null,
    installedModels: [],
    installedComponents: [],
    componentRegistry: [],
    componentUpdateChecks: [],
    catalogSources: [],
    runtimes: [],
    runtimeDetails: {},
    catalogQuery: "",
    catalogResults: [],
    apiResult: null,
    compatibilityMatrix: null,
    conformanceReport: null,
    lanPolicy: null,
    lanTokens: [],
    lanAudit: [],
    httpServerStatus: null,
    pairingChallenge: null,
    pairedToken: null,
    agentGeneratedConfig: null,
    agentDriftReport: null,
    busy: {},
    errors: {},
    activeOperations: {},
    downloadProgress: {},
    cleanupReport: null,
    schedulingPlan: null,
    quantizationBackends: [],
    quantizationQuantTypes: [],
    quantizationReports: [],
    chatTemplateLabReports: {},
    offloadPlans: {},
    prWatcherState: null,
    prWatcherLastResult: null,
    modelStalenessWarnings: {},
    traces: [],
    supportBundle: null,
    settingCapabilities: {},
    loaded: false,

    setSection: (section) => set({ section }),
    setCatalogQuery: (catalogQuery) => set({ catalogQuery }),
    clearError: (key) => set((state) => ({ errors: omitKey(state.errors, key) })),
    dismissPairedToken: () => set({ pairedToken: null }),
    clearAgentConfig: () => set({ agentGeneratedConfig: null }),
    clearAgentDriftReport: () => set({ agentDriftReport: null }),

    refresh: async () => {
      await Promise.all([get().refreshOverview(), get().refreshLan(), get().refreshComponents()]);
      // MLX is a managed runtime dependency, not a user-selected folder. On a
      // supported host, adopt the published catalog and install the verified
      // package as part of the normal Runtime Hub refresh. Network/package
      // failures remain visible on the MLX card without blocking the overview.
      await get().ensureMlxRuntime().catch(() => {});
    },

    refreshOverview: async () => {
      const key = "overview";
      const operationId = createM3OperationId("runtime-factory-refresh");
      begin(key, operationId);
      try {
        const runtimes = await runtimeHubClient.refreshRuntimes({ operationId, timeoutMs: 30_000 });
        const [hardware, profile, storage, installedModels, catalogSources] = await Promise.all([
          runtimeHubClient.hardwareSnapshot(),
          runtimeHubClient.hardwareProfile(),
          runtimeHubClient.storageStatus(),
          runtimeHubClient.installedModels(),
          runtimeHubClient.catalogSources(),
          ]);
        set({ hardware, profile, storage, installedModels, catalogSources, runtimes, loaded: true });
      } catch (error) {
        fail(key, error);
        throw error;
      } finally {
        finish(key);
      }
      // Kept out of the Promise.all above and never rethrown: the Hardware
      // Compatibility Matrix / Driver Doctor report is diagnostic, additive
      // information, so a failure here must never block the rest of the
      // Runtime Hub (hardware/profile/storage/models/runtimes) from loading.
      await get().refreshCompatibilityReport();
    },

    refreshCompatibilityReport: async () => {
      const key = "compatibility";
      begin(key);
      try {
        const compatibilityReport = await runtimeHubClient.hardwareCompatibilityReport();
        set({ compatibilityReport });
      } catch (error) {
        fail(key, error);
      } finally {
        finish(key);
      }
    },

    searchCatalog: async (requestedQuery) => {
      const key = "catalog";
      const operationId = createM3OperationId("catalog-search");
      const query = requestedQuery ?? get().catalogQuery;
      begin(key, operationId);
      try {
        const catalogResults = await runtimeHubClient.catalogSearch({
          operationId,
          timeoutMs: 30_000,
          query: query.trim(),
          limit: 50,
        });
        set({ catalogQuery: query, catalogResults });
      } catch (error) {
        fail(key, error);
        throw error;
      } finally {
        finish(key);
      }
    },

    downloadModel: async (match) => runDownload(`download:${modelAssetId(match)}`, match),
    updateModel: async (assetId, match) => runDownload(`update:${assetId}`, match, assetId),

    activateModelVersion: async (assetId, versionKey) => {
      const key = `activate-version:${assetId}`;
      const operationId = createM3OperationId("model-rollback");
      begin(key, operationId);
      try {
        await runtimeHubClient.modelActivateVersion({
          operationId,
          timeoutMs: 30_000,
          request: { assetId, versionKey },
        });
        await refreshModelState();
      } catch (error) {
        fail(key, error);
        throw error;
      } finally {
        finish(key);
      }
    },

    verifyProjector: async (assetId, versionKey, candidatePath) => {
      const key = `verify-projector:${assetId}`;
      const operationId = createM3OperationId("verify-projector");
      begin(key, operationId);
      try {
        await runtimeHubClient.verifyProjector({
          operationId,
          timeoutMs: 30_000,
          request: { assetId, versionKey, candidatePath },
        });
        await refreshModelState();
      } catch (error) {
        fail(key, error);
        throw error;
      } finally {
        finish(key);
      }
    },

    pruneModelVersions: async (assetId) => {
      const key = `prune:${assetId}`;
      const operationId = createM3OperationId("model-prune");
      begin(key, operationId);
      try {
        await runtimeHubClient.modelPruneVersions({
          operationId,
          timeoutMs: 30_000,
          request: { assetId, confirmation: `PRUNE ${assetId}` },
        });
        await refreshModelState();
      } catch (error) {
        fail(key, error);
        throw error;
      } finally {
        finish(key);
      }
    },

    deleteModel: async (assetId) => {
      const key = `delete:${assetId}`;
      const operationId = createM3OperationId("model-delete");
      begin(key, operationId);
      try {
        await runtimeHubClient.modelDelete({
          operationId,
          timeoutMs: 30_000,
          request: { assetId, confirmation: `DELETE ${assetId}` },
        });
        await refreshModelState();
      } catch (error) {
        fail(key, error);
        throw error;
      } finally {
        finish(key);
      }
    },

    cleanupOrphans: async () => {
      const key = "cleanup-orphans";
      const operationId = createM3OperationId("cleanup-orphans");
      begin(key, operationId);
      try {
        const cleanupReport = await runtimeHubClient.cleanupOrphans({
          operationId,
          timeoutMs: 30_000,
          confirmation: "CLEAN ORPHANS",
        });
        set({ cleanupReport });
        await refreshModelState();
      } catch (error) {
        fail(key, error);
        throw error;
      } finally {
        finish(key);
      }
    },

    replaceCatalogSources: async (sources) => {
      const key = "catalog-sources";
      begin(key);
      try {
        const catalogSources = await runtimeHubClient.catalogReplaceSources(sources);
        set({ catalogSources, catalogResults: [] });
      } catch (error) {
        fail(key, error);
        throw error;
      } finally {
        finish(key);
      }
    },

    refreshComponents: async () => {
      const key = "components";
      begin(key);
      try {
        const [installedComponents, componentRegistry] = await Promise.all([
          runtimeHubClient.componentInstalled(),
          runtimeHubClient.componentListRegistry({
            operationId: createM3OperationId("component-registry-list"),
            timeoutMs: 30_000,
          }),
        ]);
        const componentUpdateChecks = installedComponents.length
          ? await runtimeHubClient.componentCheckUpdates({
              operationId: createM3OperationId("component-check-updates"),
              timeoutMs: 30_000,
            })
          : [];
        set({ installedComponents, componentRegistry, componentUpdateChecks });
      } catch (error) {
        // Soft-fails like `refreshLan`: a component-hub hiccup should not
        // block the rest of the Runtime Hub overview from loading.
        fail(key, error);
      } finally {
        finish(key);
      }
    },

    installComponent: async (entry) => {
      const key = `component-install:${entry.componentId}`;
      const operationId = createM3OperationId("component-install");
      begin(key, operationId);
      try {
        await runtimeHubClient.componentInstall({
          operationId,
          timeoutMs: null,
          request: { entry },
        });
        if (entry.kind === "mlx_runtime") {
          // Runtime capabilities include whether the verified package exists;
          // refresh that snapshot as well as the live card status so Load model
          // becomes available without a second manual refresh.
          await get().refreshOverview();
          await get().refreshRuntime("mlx");
        }
        await get().refreshComponents();
      } catch (error) {
        fail(key, error);
        throw error;
      } finally {
        finish(key);
      }
    },

    ensureMlxRuntime: async () => {
      const key = "mlx-auto-install";
      if (get().busy[key]) return;
      const platform = get().hardware?.platform;
      const mlxRuntime = get().runtimes.find((runtime) => runtime.descriptor.kind === "mlx");
      if (
        !platform ||
        platform.os !== "macos" ||
        !["aarch64", "arm64"].includes(platform.arch) ||
        !mlxRuntime
      ) {
        return;
      }

      begin(key);
      try {
        const installed = get().installedComponents.find(
          (component) => component.componentId === MLX_COMPONENT_ID,
        );
        const active = installed?.versions.find((version) => version.active);
        if (active && mlxRuntime.canInfer) {
          return;
        }
        if (active) {
          // The component index can outlive a damaged or manually removed
          // active MLX tree. Rehydrate it from the already digest-verified
          // component artifact before downloading anything new.
          await runtimeHubClient.mlxInstallComponent(MLX_COMPONENT_ID);
          await get().refreshRuntime("mlx");
          return;
        }

        const knownEntry = latestMlxComponent(get().componentRegistry);
        let entry = knownEntry;
        try {
          await get().syncComponentCatalog();
          entry = latestMlxComponent(get().componentRegistry) ?? knownEntry;
        } catch (error) {
          if (!knownEntry) throw error;
        }
        if (!entry) {
          throw new Error("The published MLX runtime is not available in the component catalog.");
        }

        await get().installComponent(entry);
      } catch (error) {
        fail(key, error);
        throw error;
      } finally {
        finish(key);
      }
    },

    installMlxPackage: async (packageDirectory) => {
      const key = "mlx-install";
      begin(key);
      try {
        await runtimeHubClient.mlxInstall(packageDirectory);
        // The MLX driver reports its state from `verify_active()` on the next
        // status read, so refreshing the runtime is what turns the card from
        // Not Installed into a version.
        await get().refreshRuntime("mlx");
      } catch (error) {
        fail(key, error);
        throw error;
      } finally {
        finish(key);
      }
    },

    activateComponentVersion: async (componentId, versionKey) => {
      const key = `component-activate:${componentId}`;
      const operationId = createM3OperationId("component-activate");
      begin(key, operationId);
      try {
        await runtimeHubClient.componentActivateVersion({
          operationId,
          timeoutMs: 30_000,
          request: { componentId, versionKey },
        });
        await get().refreshComponents();
      } catch (error) {
        fail(key, error);
        throw error;
      } finally {
        finish(key);
      }
    },

    replaceComponentRegistry: async (entries) => {
      const key = "component-registry";
      begin(key);
      try {
        const componentRegistry = await runtimeHubClient.componentReplaceRegistryEntries(entries);
        set({ componentRegistry });
      } catch (error) {
        fail(key, error);
        throw error;
      } finally {
        finish(key);
      }
    },

    mergeComponentRegistry: async (entries) => {
      const key = "component-registry";
      begin(key);
      try {
        const componentRegistry = await runtimeHubClient.componentMergeRegistryEntries(entries);
        set({ componentRegistry });
      } catch (error) {
        fail(key, error);
        throw error;
      } finally {
        finish(key);
      }
    },

    syncComponentCatalog: async (url) => {
      const key = "component-catalog";
      const operationId = createM3OperationId("component-catalog");
      begin(key, operationId);
      try {
        // The registry the backend returns is the merged one, so the panel does
        // not re-read and cannot render a state the file never had.
        const componentRegistry = await runtimeHubClient.componentSyncCatalog({
          operationId,
          timeoutMs: 30_000,
          ...(url ? { url } : {}),
        });
        set({ componentRegistry });
      } catch (error) {
        fail(key, error);
        throw error;
      } finally {
        finish(key);
      }
    },

    planSchedule: async (input) => {
      const key = "schedule-plan";
      begin(key);
      try {
        const schedulingPlan = await runtimeHubClient.schedulePlan(input);
        set({ schedulingPlan });
      } catch (error) {
        set({ schedulingPlan: null });
        fail(key, error);
        throw error;
      } finally {
        finish(key);
      }
    },

    fetchChatTemplateLabReport: async (template) => {
      const cacheKey = template ?? "";
      const key = `chat-template-lab:${cacheKey}`;
      begin(key);
      try {
        const report = await runtimeHubClient.chatTemplateLabReport(template);
        set((state) => ({
          chatTemplateLabReports: { ...state.chatTemplateLabReports, [cacheKey]: report },
        }));
      } catch (error) {
        fail(key, error);
        throw error;
      } finally {
        finish(key);
      }
    },

    previewOffloadPlan: async (runtimeId, input) => {
      const key = `offload-plan:${runtimeId}`;
      begin(key);
      try {
        const plan = await runtimeHubClient.offloadPlan(input);
        set((state) => ({ offloadPlans: { ...state.offloadPlans, [runtimeId]: plan } }));
      } catch (error) {
        set((state) => ({ offloadPlans: omitKey(state.offloadPlans, runtimeId) }));
        fail(key, error);
        throw error;
      } finally {
        finish(key);
      }
    },

    checkModelStaleness: async (assetId) => {
      // Model Retirement and Compatibility Warnings (ROADMAP.md Phase 8,
      // item 14): diagnostic, additive information like the Hardware
      // Compatibility Matrix report above — a staleness-check hiccup must
      // never block the "Load model" flow itself, so failures are captured
      // for display but never rethrown.
      const key = `model-staleness:${assetId}`;
      begin(key);
      try {
        const operationId = createM3OperationId("model-staleness-check");
        const warning = await runtimeHubClient.modelStalenessCheck({ operationId, timeoutMs: 30_000, assetId });
        set((state) => ({ modelStalenessWarnings: { ...state.modelStalenessWarnings, [assetId]: warning } }));
      } catch (error) {
        fail(key, error);
      } finally {
        finish(key);
      }
    },

    resolveSettingCapabilities: async (runtimeId, assetId) => {
      const key = `settings-gate:${runtimeId}`;
      begin(key);
      try {
        const resolved = await runtimeHubClient.resolveSettingCapabilities({ runtimeId, assetId });
        set((state) => ({ settingCapabilities: { ...state.settingCapabilities, [runtimeId]: resolved } }));
      } catch (error) {
        set((state) => ({ settingCapabilities: omitKey(state.settingCapabilities, runtimeId) }));
        fail(key, error);
        throw error;
      } finally {
        finish(key);
      }
    },

    cancelOperation: async (key) => {
      const operationId = get().activeOperations[key];
      if (!operationId) return false;
      set((state) => {
        const entry = Object.entries(state.downloadProgress).find(([, progress]) => progress.operationId === operationId);
        if (!entry) return {};
        return {
          downloadProgress: {
            ...state.downloadProgress,
            [entry[0]]: { ...entry[1], phase: "cancelling" },
          },
        };
      });
      try {
        return await runtimeHubClient.cancelOperation(operationId);
      } catch (error) {
        fail(key, error);
        throw error;
      }
    },

    refreshRuntime: async (runtimeId) => {
      const key = `runtime:${runtimeId}`;
      const capability = get().runtimes.find((runtime) => runtime.descriptor.runtimeId === runtimeId);
      begin(key);
      try {
        const statusOperation = createM3OperationId("runtime-status");
        const inventoryOperation = createM3OperationId("runtime-inventory");
        const [statusResult, inventoryResult, logsResult, metricsResult, configResult, contextCacheResult] =
          await Promise.allSettled([
            runtimeHubClient.runtimeStatus({ operationId: statusOperation, timeoutMs: 15_000, runtimeId }),
            runtimeHubClient.runtimeInventory({ operationId: inventoryOperation, timeoutMs: 20_000, runtimeId }),
            capability?.canLogs
              ? runtimeHubClient.runtimeLogs({
                  operationId: createM3OperationId("runtime-logs"),
                  timeoutMs: 10_000,
                  runtimeId,
                  maxBytes: 128 * 1024,
                })
              : Promise.resolve(undefined),
            capability?.canMetrics
              ? runtimeHubClient.runtimeMetrics({
                  operationId: createM3OperationId("runtime-metrics"),
                  timeoutMs: 10_000,
                  runtimeId,
                })
              : Promise.resolve(undefined),
            runtimeHubClient.runtimeConfig(runtimeId),
            runtimeHubClient.contextCacheState({
              operationId: createM3OperationId("context-cache-state"),
              timeoutMs: 10_000,
              runtimeId,
            }),
          ]);
        if (statusResult.status === "rejected") throw statusResult.reason;
        if (inventoryResult.status === "rejected") throw inventoryResult.reason;
        if (configResult.status === "rejected") throw configResult.reason;
        // Status, inventory, and config are the runtime snapshot. Logs,
        // metrics, and context/cache state are additive diagnostics: a stopped
        // managed service may legitimately reject those calls, and that must
        // not hide the installed/stopped state from the card.
        const contextCache = contextCacheResult.status === "fulfilled" ? contextCacheResult.value : undefined;
        set((state) => ({
          runtimeDetails: {
            ...state.runtimeDetails,
            [runtimeId]: {
              ...state.runtimeDetails[runtimeId],
              status: statusResult.value,
              inventory: inventoryResult.value,
              logs: logsResult.status === "fulfilled" ? logsResult.value : state.runtimeDetails[runtimeId]?.logs,
              metrics: metricsResult.status === "fulfilled" ? metricsResult.value : state.runtimeDetails[runtimeId]?.metrics,
              config: configResult.value ?? undefined,
              contextCache,
              refreshedAt: Date.now(),
            },
          },
        }));
      } catch (error) {
        fail(key, error);
        throw error;
      } finally {
        finish(key);
      }
    },

    // Pure, read-only helpers with no shared state to track: like
    // `previewOffloadPlan`'s underlying `m3_offload_plan` call, these never
    // touch a runtime process, so callers can await the resolved value
    // directly instead of reading it back out of the store.
    resolveEffectiveContext: (input) => runtimeHubClient.contextEffectiveSize(input),
    classifyContextFailure: (input) => runtimeHubClient.classifyContextFailure(input),

    loadModel: async (request) => {
      const key = `load:${request.runtimeId}`;
      const operationId = createM3OperationId("runtime-load");
      begin(key, operationId);
      const startedAtMs = Date.now();
      // Reuses whatever offload plan `previewOffloadPlan` already computed
      // for this runtime (the Runtime Hub's load flow always previews one
      // before offering the load action) — this trace never computes its
      // own placement/memory numbers, only reports the planner's.
      const offloadPlan = get().offloadPlans[request.runtimeId] ?? null;
      const recordTrace = (loadError: unknown) => {
        // Wrapped in `Promise.resolve` (rather than chaining `.catch`
        // directly off the client call) so this stays safe even if the
        // client returns a non-promise, which test mocks default to.
        void Promise.resolve(
          runtimeHubClient.telemetryRecordLoad({
            runtimeId: request.runtimeId,
            modelId: request.assetId,
            startedAtMs,
            readyAtMs: Date.now(),
            offloadPlan,
            errorMessage: loadError === undefined ? null : errorMessage(loadError),
          }),
        ).catch(() => {
          // Telemetry capture is best-effort and must never surface as a
          // load failure to the user.
        });
      };
      try {
        await runtimeHubClient.runtimeLoadModel({ operationId, timeoutMs: 120_000, request });
        await get().refreshRuntime(request.runtimeId);
        recordTrace(undefined);
      } catch (error) {
        recordTrace(error);
        fail(key, error);
        throw error;
      } finally {
        finish(key);
      }
    },

    unloadModel: async (request) => {
      const key = `unload:${request.runtimeId}:${request.modelId}`;
      const operationId = createM3OperationId("runtime-unload");
      begin(key, operationId);
      try {
        await runtimeHubClient.runtimeUnloadModel({ operationId, timeoutMs: 30_000, request });
        await get().refreshRuntime(request.runtimeId);
      } catch (error) {
        fail(key, error);
        throw error;
      } finally {
        finish(key);
      }
    },

    setRuntimeConfig: async (runtimeId, values) => {
      const key = `config:${runtimeId}`;
      begin(key);
      try {
        const config = await runtimeHubClient.runtimeSetConfig({ runtimeId, values });
        set((state) => ({
          runtimeDetails: {
            ...state.runtimeDetails,
            [runtimeId]: { ...state.runtimeDetails[runtimeId], config },
          },
        }));
      } catch (error) {
        fail(key, error);
        throw error;
      } finally {
        finish(key);
      }
    },

    dispatchApi: async (request) => {
      const key = "api";
      const operationId = createM3OperationId("api-dispatch");
      begin(key, operationId);
      const startedAtMs = Date.now();
      // Only ever plucks a fixed allowlist of numeric sampler/usage keys —
      // see `extractSamplerStatsFromRequestBody`/`extractTokenTimingFromResponseBody`
      // in `lib/runtimeTelemetry.ts`. This request body is a free-form
      // diagnostics payload the user can type anything into, so this is the
      // one place in the Runtime Hub that must not assume the body is
      // free of prompt content.
      const sampler = extractSamplerStatsFromRequestBody(request.body);
      const modelId = extractModelIdFromRequestBody(request.body) ?? request.requestId;
      const recordTrace = (responseBody: unknown, dispatchError: unknown) => {
        void Promise.resolve(
          runtimeHubClient.telemetryRecordRequest({
            runtimeId: request.runtimeId,
            modelId,
            startedAtMs,
            endedAtMs: Date.now(),
            sampler,
            tokens: extractTokenTimingFromResponseBody(responseBody),
            errorMessage: dispatchError === undefined ? null : errorMessage(dispatchError),
          }),
        ).catch(() => {
          // Best-effort, same as `loadModel`'s trace capture.
        });
      };
      try {
        const apiResult = await runtimeHubClient.apiDispatch({ operationId, timeoutMs: 120_000, request });
        set({ apiResult });
        recordTrace(apiResult.body, undefined);
      } catch (error) {
        recordTrace(null, error);
        fail(key, error);
        throw error;
      } finally {
        finish(key);
      }
    },

    cancelInference: async (request) => {
      const key = "api-cancel";
      const operationId = createM3OperationId("api-cancel");
      begin(key, operationId);
      try {
        return await runtimeHubClient.apiCancelInference({ operationId, timeoutMs: 10_000, request });
      } catch (error) {
        fail(key, error);
        throw error;
      } finally {
        finish(key);
      }
    },

    refreshCompatibilityMatrix: async () => {
      const key = "compatibility-matrix";
      begin(key);
      try {
        const compatibilityMatrix = await runtimeHubClient.compatibilityMatrix();
        set({ compatibilityMatrix });
      } catch (error) {
        fail(key, error);
      } finally {
        finish(key);
      }
    },

    runConformanceSuite: async (baseUrl, token, sections) => {
      const key = "conformance-suite";
      begin(key);
      try {
        const conformanceReport = await runtimeHubClient.runConformanceSuite(baseUrl, token, sections);
        set({ conformanceReport });
      } catch (error) {
        fail(key, error);
      } finally {
        finish(key);
      }
    },

    clearConformanceReport: () => set({ conformanceReport: null }),

    refreshLan: async () => {
      const key = "lan-refresh";
      begin(key);
      try {
        const [lanPolicy, httpServerStatus] = await Promise.all([
          runtimeHubClient.lanPolicy(),
          runtimeHubClient.httpServerStatus(),
        ]);
        const [lanTokens, lanAudit] = lanPolicy
          ? await Promise.all([runtimeHubClient.lanTokens(), runtimeHubClient.lanAuditEvents()])
          : [[], []];
        set({ lanPolicy, lanTokens, lanAudit, httpServerStatus });
      } catch (error) {
        fail(key, error);
      } finally {
        finish(key);
      }
    },

    validateLanPolicy: async (policy) => {
      const key = "lan-policy";
      begin(key);
      try {
        await runtimeHubClient.lanValidatePolicy(policy);
      } catch (error) {
        fail(key, error);
        throw error;
      } finally {
        finish(key);
      }
    },

    configureLan: async (policy) => {
      const key = "lan-policy";
      begin(key);
      try {
        await runtimeHubClient.lanValidatePolicy(policy);
        const lanPolicy = await runtimeHubClient.lanConfigure(policy);
        set({ lanPolicy });
        // `m3_lan_configure` is a backend transaction: policy persistence,
        // listener reconciliation, and rollback on bind failure complete
        // before it returns. A second start call would reopen the race that
        // transaction exists to remove.
        await get().refreshLan();
      } catch (error) {
        try {
          const httpServerStatus = await runtimeHubClient.httpServerStatus();
          set({ httpServerStatus });
        } catch {
          // Preserve the original configure/start error if status inspection fails.
        }
        fail(key, error);
        throw error;
      } finally {
        finish(key);
      }
    },

    disableLan: async () => {
      const key = "lan-disable";
      begin(key);
      try {
        // Disable + listener reconciliation is likewise one backend
        // transaction. Publish the durable policy result immediately, then
        // refresh the status projection from the unified server.
        await runtimeHubClient.lanDisable("DISABLE LAN API");
        set({ lanPolicy: null, lanTokens: [], pairingChallenge: null, pairedToken: null });
        const httpServerStatus = await runtimeHubClient.httpServerStatus();
        set({ httpServerStatus });
      } catch (error) {
        fail(key, error);
        throw error;
      } finally {
        finish(key);
      }
    },

    beginPairing: async (request, remoteAddress = "127.0.0.1") => {
      const key = "lan-pairing";
      begin(key);
      try {
        const pairingChallenge = await runtimeHubClient.lanBeginPairing(request, Date.now(), remoteAddress);
        set({ pairingChallenge, pairedToken: null });
        await get().refreshLan();
      } catch (error) {
        fail(key, error);
        throw error;
      } finally {
        finish(key);
      }
    },

    completePairing: async (challengeId, pairingCode, remoteAddress = "127.0.0.1") => {
      const key = "lan-pairing";
      begin(key);
      try {
        const pairedToken = await runtimeHubClient.lanCompletePairing(
          challengeId,
          pairingCode,
          Date.now(),
          remoteAddress,
        );
        set({ pairedToken, pairingChallenge: null });
        await get().refreshLan();
      } catch (error) {
        fail(key, error);
        throw error;
      } finally {
        finish(key);
      }
    },

    revokeToken: async (tokenId, remoteAddress = "127.0.0.1") => {
      const key = `lan-revoke:${tokenId}`;
      begin(key);
      try {
        await runtimeHubClient.lanRevokeToken(tokenId, Date.now(), remoteAddress);
        await get().refreshLan();
      } catch (error) {
        fail(key, error);
        throw error;
      } finally {
        finish(key);
      }
    },

    startHttpServer: async () => {
      const key = "http-server";
      begin(key);
      try {
        const httpServerStatus = await runtimeHubClient.httpServerStart();
        set({ httpServerStatus });
      } catch (error) {
        try {
          set({ httpServerStatus: await runtimeHubClient.httpServerStatus() });
        } catch {
          // Preserve the start failure when status inspection is also unavailable.
        }
        fail(key, error);
        throw error;
      } finally {
        finish(key);
      }
    },

    stopHttpServer: async () => {
      const key = "http-server";
      begin(key);
      try {
        const httpServerStatus = await runtimeHubClient.httpServerStop();
        set({ httpServerStatus });
      } catch (error) {
        fail(key, error);
        throw error;
      } finally {
        finish(key);
      }
    },

    storeTlsIdentity: async (reference, certificatePem, privateKeyPem) => {
      const key = "tls-identity";
      begin(key);
      try {
        return await runtimeHubClient.httpServerStoreTlsIdentity(reference, certificatePem, privateKeyPem);
      } catch (error) {
        fail(key, error);
        throw error;
      } finally {
        finish(key);
      }
    },

    refreshTraces: async (runtimeId = null) => {
      const key = "telemetry-traces";
      begin(key);
      try {
        const traces = await runtimeHubClient.telemetryRecentTraces(runtimeId, 100);
        set({ traces });
      } catch (error) {
        fail(key, error);
        throw error;
      } finally {
        finish(key);
      }
    },

    exportSupportBundle: async () => {
      const key = "telemetry-support-bundle";
      const operationId = createM3OperationId("telemetry-support-bundle");
      begin(key, operationId);
      try {
        const supportBundle = await runtimeHubClient.telemetrySupportBundle({ operationId, timeoutMs: 30_000 });
        set({ supportBundle });
        return supportBundle;
      } catch (error) {
        fail(key, error);
        throw error;
      } finally {
        finish(key);
      }
    },

    clearSupportBundle: () => set({ supportBundle: null }),

    generateAgentConfig: async (tool, modelId, authToken) => {
      const key = "agent-launcher-generate";
      begin(key);
      try {
        const agentGeneratedConfig = await runtimeHubClient.agentLauncherGenerateConfig(
          tool,
          modelId,
          authToken,
        );
        set({ agentGeneratedConfig });
      } catch (error) {
        fail(key, error);
        throw error;
      } finally {
        finish(key);
      }
    },
    refreshQuantization: async () => {
      const key = "quantization-refresh";
      begin(key);
      try {
        const [quantizationBackends, quantizationQuantTypes] = await Promise.all([
          runtimeHubClient.quantizationBackends(),
          runtimeHubClient.quantizationQuantTypes(),
        ]);
        set({ quantizationBackends, quantizationQuantTypes });
      } catch (error) {
        fail(key, error);
        throw error;
      } finally {
        finish(key);
      }
    },

    checkAgentConfigDrift: async (tool, pastedConfig) => {
      const key = "agent-launcher-drift";
      begin(key);
      try {
        const agentDriftReport = await runtimeHubClient.agentLauncherCheckDrift(tool, pastedConfig);
        set({ agentDriftReport });
      } catch (error) {
        fail(key, error);
        throw error;
      } finally {
        finish(key);
      }
    },

    convertPathQuantization: async (sourcePath, quantChoice, allowRequantize) => {
      const key = "quantization-convert";
      begin(key);
      try {
        const report = await runtimeHubClient.quantizationConvertPath({ sourcePath, quantChoice, allowRequantize });
        set((state) => ({ quantizationReports: [report, ...state.quantizationReports] }));
      } catch (error) {
        fail(key, error);
        throw error;
      } finally {
        finish(key);
      }
    },

    convertInstalledModelQuantization: async (assetId, versionKey, quantChoice, allowRequantize) => {
      const key = "quantization-convert";
      begin(key);
      try {
        const report = await runtimeHubClient.quantizationConvertInstalledModel({
          assetId,
          versionKey,
          quantChoice,
          allowRequantize,
        });
        set((state) => ({ quantizationReports: [report, ...state.quantizationReports] }));
      } catch (error) {
        fail(key, error);
        throw error;
      } finally {
        finish(key);
      }
    },

    refreshPrWatcher: async () => {
      const key = "pr-watcher-refresh";
      begin(key);
      try {
        const prWatcherState = await runtimeHubClient.runtimePrWatcherState();
        set({ prWatcherState });
      } catch (error) {
        fail(key, error);
        throw error;
      } finally {
        finish(key);
      }
    },

    checkPrWatcherNow: async () => {
      const key = "pr-watcher-check";
      begin(key);
      try {
        const result = await runtimeHubClient.runtimePrWatcherCheckNow();
        set({ prWatcherState: result.state, prWatcherLastResult: result });
      } catch (error) {
        fail(key, error);
        throw error;
      } finally {
        finish(key);
      }
    },
  };
});

export default useRuntimeHubStore;
