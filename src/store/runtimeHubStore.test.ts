import { beforeEach, describe, expect, it, vi } from "vitest";

const mocks = vi.hoisted(() => ({
  counter: 0,
  client: {
    hardwareSnapshot: vi.fn(),
    hardwareProfile: vi.fn(),
    hardwareCompatibilityReport: vi.fn(),
    storageStatus: vi.fn(),
    installedModels: vi.fn(),
    catalogSources: vi.fn(),
    catalogReplaceSources: vi.fn(),
    runtimes: vi.fn(),
    refreshRuntimes: vi.fn(),
    schedulePlan: vi.fn(),
    catalogSearch: vi.fn(),
    modelDownload: vi.fn(),
    modelUpdate: vi.fn(),
    modelActivateVersion: vi.fn(),
    modelPruneVersions: vi.fn(),
    modelDelete: vi.fn(),
    cleanupOrphans: vi.fn(),
    cancelOperation: vi.fn(),
    runtimeStatus: vi.fn(),
    runtimeInventory: vi.fn(),
    runtimeLoadModel: vi.fn(),
    runtimeUnloadModel: vi.fn(),
    runtimeLogs: vi.fn(),
    runtimeMetrics: vi.fn(),
    runtimeSetConfig: vi.fn(),
    runtimeConfig: vi.fn(),
    apiDispatch: vi.fn(),
    apiCancelInference: vi.fn(),
    lanValidatePolicy: vi.fn(),
    lanConfigure: vi.fn(),
    lanDisable: vi.fn(),
    lanPolicy: vi.fn(),
    lanBeginPairing: vi.fn(),
    lanCompletePairing: vi.fn(),
    lanRevokeToken: vi.fn(),
    lanTokens: vi.fn(),
    lanAuditEvents: vi.fn(),
    httpServerStart: vi.fn(),
    httpServerStop: vi.fn(),
    httpServerStatus: vi.fn(),
    httpServerStoreTlsIdentity: vi.fn(),
  },
}));

vi.mock("../lib/runtimeHubClient", () => ({
  runtimeHubClient: mocks.client,
  createM3OperationId: (prefix: string) => `${prefix}-test-${++mocks.counter}`,
  sha256Text: vi.fn().mockResolvedValue("license-digest"),
}));

import { useRuntimeHubStore } from "./runtimeHubStore";

const hardware = {
  captured_at_ms: 1,
  total_ram_bytes: 32,
  available_ram_bytes: 24,
  logical_cpu_count: 8,
  platform: { os: "macos", arch: "aarch64", supported_runtimes: ["ollama", "llama_cpp"], accelerators: [] },
};
const profile = {
  tier: "performance",
  recommended_process_slots: 4,
  recommended_ram_reserve_bytes: 4,
  preferred_accelerator: "metal",
};
const storage = {
  root: "/models",
  quotaBytes: 1000,
  reserveBytes: 100,
  usedBytes: 10,
  availableForModelsBytes: 890,
  pendingDownloadBytes: 0,
};
const compatibilityReport = {
  capturedAtMs: 1,
  os: "macos",
  arch: "aarch64",
  accelerators: [
    {
      kind: "metal",
      status: "available",
      summary: "Metal is available.",
      deviceNames: ["Apple Silicon unified GPU"],
      driverVersion: null,
      computeCapability: null,
      confirmed: true,
    },
  ],
  jetson: { detected: false, model: null },
  hybridGraphicsDetected: false,
  notes: [],
};
const capability = {
  descriptor: { runtimeId: "ollama", kind: "ollama", label: "Ollama", managed: false, apiBackend: "ollama" },
  canLoad: true,
  canUnload: true,
  canLogs: true,
  canMetrics: true,
  canInfer: true,
  settings: [],
};
const policy = {
  bindAddress: "127.0.0.1",
  port: 1234,
  requireAuthentication: true,
  pairingRequired: true,
  tls: { mode: "disabled" },
  corsAllowlist: [],
  allowedBackends: ["managed_local", "ollama", "mlx"],
  allowedLanMutations: [],
  allowCloudProvidersOverLan: false,
  rateLimit: { windowMs: 60000, maxRequests: 60, maxInputBytes: 1024 },
  pairingTtlMs: 300000,
};

beforeEach(() => {
  mocks.counter = 0;
  for (const mock of Object.values(mocks.client)) mock.mockReset();
  useRuntimeHubStore.setState({
    section: "overview",
    hardware: null,
    profile: null,
    compatibilityReport: null,
    storage: null,
    installedModels: [],
    catalogSources: [],
    runtimes: [],
    runtimeDetails: {},
    catalogQuery: "",
    catalogResults: [],
    apiResult: null,
    lanPolicy: null,
    lanTokens: [],
    lanAudit: [],
    httpServerStatus: null,
    pairingChallenge: null,
    pairedToken: null,
    busy: {},
    errors: {},
    activeOperations: {},
    downloadProgress: {},
    cleanupReport: null,
    schedulingPlan: null,
    loaded: false,
  });
});

describe("runtimeHubStore", () => {
  it("loads hardware, profile, storage, installed models, runtimes, and disabled LAN state", async () => {
    mocks.client.hardwareSnapshot.mockResolvedValue(hardware);
    mocks.client.hardwareProfile.mockResolvedValue(profile);
    mocks.client.hardwareCompatibilityReport.mockResolvedValue(compatibilityReport);
    mocks.client.storageStatus.mockResolvedValue(storage);
    mocks.client.installedModels.mockResolvedValue([]);
    mocks.client.refreshRuntimes.mockResolvedValue([capability]);
    mocks.client.catalogSources.mockResolvedValue([]);
    mocks.client.lanPolicy.mockResolvedValue(null);
    mocks.client.httpServerStatus.mockResolvedValue({ status: "stopped" });

    await useRuntimeHubStore.getState().refresh();

    const state = useRuntimeHubStore.getState();
    expect(state.hardware).toEqual(hardware);
    expect(state.profile).toEqual(profile);
    expect(state.storage).toEqual(storage);
    expect(state.runtimes).toEqual([capability]);
    expect(state.compatibilityReport).toEqual(compatibilityReport);
    expect(state.loaded).toBe(true);
    expect(mocks.client.lanTokens).not.toHaveBeenCalled();
    expect(state.busy).toEqual({});
  });

  it("does not block the rest of the overview refresh when the compatibility report fails", async () => {
    mocks.client.hardwareSnapshot.mockResolvedValue(hardware);
    mocks.client.hardwareProfile.mockResolvedValue(profile);
    mocks.client.hardwareCompatibilityReport.mockRejectedValue(new Error("driver doctor offline"));
    mocks.client.storageStatus.mockResolvedValue(storage);
    mocks.client.installedModels.mockResolvedValue([]);
    mocks.client.refreshRuntimes.mockResolvedValue([capability]);
    mocks.client.catalogSources.mockResolvedValue([]);

    await useRuntimeHubStore.getState().refreshOverview();

    const state = useRuntimeHubStore.getState();
    expect(state.hardware).toEqual(hardware);
    expect(state.loaded).toBe(true);
    expect(state.compatibilityReport).toBeNull();
    expect(state.errors.compatibility).toContain("driver doctor offline");
  });

  it("tracks a cancellable catalog operation and stores exact results", async () => {
    const results = [{ model: { modelId: "qwen" }, fit: { rating: "recommended" } }];
    mocks.client.catalogSearch.mockResolvedValue(results);

    await useRuntimeHubStore.getState().searchCatalog("qwen");

    expect(mocks.client.catalogSearch).toHaveBeenCalledWith({
      operationId: "catalog-search-test-1",
      timeoutMs: 30000,
      query: "qwen",
      limit: 50,
    });
    expect(useRuntimeHubStore.getState().catalogResults).toEqual(results);

    useRuntimeHubStore.setState({ activeOperations: { catalog: "catalog-live" } });
    mocks.client.cancelOperation.mockResolvedValue(true);
    await expect(useRuntimeHubStore.getState().cancelOperation("catalog")).resolves.toBe(true);
    expect(mocks.client.cancelOperation).toHaveBeenCalledWith("catalog-live");
  });

  it("collects status, inventory, logs, and metrics only through capability-backed runtime calls", async () => {
    useRuntimeHubStore.setState({ runtimes: [capability] as never });
    const status = { runtimeType: "adapter", status: { state: "ready" }, running_models: [] };
    const inventory = { schema_version: 1, runtime_id: "ollama", models: [], captured_at_ms: 1 };
    const logs = { text: "ready", truncated: false };
    const metrics = { runtimeType: "adapter", status: { state: "ready" }, running_models: [] };
    mocks.client.runtimeStatus.mockResolvedValue(status);
    mocks.client.runtimeInventory.mockResolvedValue(inventory);
    mocks.client.runtimeLogs.mockResolvedValue(logs);
    mocks.client.runtimeMetrics.mockResolvedValue(metrics);
    mocks.client.runtimeConfig.mockResolvedValue(null);

    await useRuntimeHubStore.getState().refreshRuntime("ollama");

    expect(useRuntimeHubStore.getState().runtimeDetails.ollama).toMatchObject({ status, inventory, logs, metrics });
    expect(mocks.client.runtimeStatus).toHaveBeenCalledWith(expect.objectContaining({ runtimeId: "ollama" }));
    expect(mocks.client.runtimeLogs).toHaveBeenCalledWith(expect.objectContaining({ maxBytes: 128 * 1024 }));
  });

  it("validates and persists LAN policy before refreshing scoped tokens and audit events", async () => {
    mocks.client.lanValidatePolicy.mockResolvedValue(undefined);
    mocks.client.lanConfigure.mockResolvedValue(policy);
    mocks.client.httpServerStart.mockResolvedValue({ status: "running", bindAddress: "127.0.0.1", port: 1234 });
    mocks.client.lanPolicy.mockResolvedValue(policy);
    mocks.client.httpServerStatus.mockResolvedValue({ status: "running", bindAddress: "127.0.0.1", port: 1234 });
    mocks.client.lanTokens.mockResolvedValue([{ tokenId: "token-1" }]);
    mocks.client.lanAuditEvents.mockResolvedValue([{ eventId: "event-1" }]);

    await useRuntimeHubStore.getState().configureLan(policy as never);

    expect(mocks.client.lanValidatePolicy).toHaveBeenCalledWith(policy);
    expect(mocks.client.lanConfigure).toHaveBeenCalledWith(policy);
    expect(mocks.client.httpServerStart).toHaveBeenCalled();
    expect(useRuntimeHubStore.getState().lanPolicy).toEqual(policy);
    expect(useRuntimeHubStore.getState().lanTokens).toEqual([{ tokenId: "token-1" }]);
    expect(useRuntimeHubStore.getState().lanAudit).toEqual([{ eventId: "event-1" }]);
  });

  it("keeps a persisted LAN policy and exposes listener diagnostics when startup fails", async () => {
    const status = { status: "error", bindAddress: "127.0.0.1", port: 1234, lastError: "Address in use" };
    mocks.client.lanValidatePolicy.mockResolvedValue(undefined);
    mocks.client.lanConfigure.mockResolvedValue(policy);
    mocks.client.httpServerStart.mockRejectedValue(new Error("Address in use"));
    mocks.client.httpServerStatus.mockResolvedValue(status);

    await expect(useRuntimeHubStore.getState().configureLan(policy as never)).rejects.toThrow("Address in use");

    expect(useRuntimeHubStore.getState().lanPolicy).toEqual(policy);
    expect(useRuntimeHubStore.getState().httpServerStatus).toEqual(status);
    expect(useRuntimeHubStore.getState().errors["lan-policy"]).toContain("Address in use");
  });

  it("dispatches and cancels compatibility requests without losing the protocol result", async () => {
    const request = {
      protocol: "open_ai_chat_completions",
      runtimeId: "ollama",
      requestId: "req-1",
      body: [123, 125],
      caller: { type: "internal" },
      nowMs: 1,
    };
    mocks.client.apiDispatch.mockResolvedValue({ status: 200, body: { id: "response" } });
    mocks.client.apiCancelInference.mockResolvedValue(true);

    await useRuntimeHubStore.getState().dispatchApi(request as never);
    await expect(useRuntimeHubStore.getState().cancelInference({ ...request, modelId: "qwen" } as never)).resolves.toBe(true);

    expect(useRuntimeHubStore.getState().apiResult).toEqual({ status: 200, body: { id: "response" } });
    expect(mocks.client.apiDispatch).toHaveBeenCalledWith(expect.objectContaining({ request }));
    expect(mocks.client.apiCancelInference).toHaveBeenCalledWith(expect.objectContaining({ request: expect.objectContaining({ modelId: "qwen" }) }));
  });
});
