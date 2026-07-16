import { beforeEach, describe, expect, it, vi } from "vitest";

const invokeMock = vi.hoisted(() => vi.fn());
vi.mock("@tauri-apps/api/core", () => ({ invoke: invokeMock }));

import {
  createM3OperationId,
  runtimeHubClient,
  sha256Text,
  type LanServerPolicy,
  type M3ApiDispatchRequest,
  type M3CancelInferenceRequest,
  type M3CatalogModel,
  type M3LoadModelRequest,
  type M3SchedulingInput,
  type M3UnloadModelRequest,
  type PairingRequest,
} from "./runtimeHubClient";

beforeEach(() => {
  invokeMock.mockReset();
  invokeMock.mockResolvedValue(undefined);
});

describe("runtimeHubClient", () => {
  it("maps every M3 desktop capability to its exact Tauri command and camel-case arguments", async () => {
    const operation = { operationId: "op-1", timeoutMs: 1000 };
    const model = {} as M3CatalogModel;
    const load = {} as M3LoadModelRequest;
    const unload = {} as M3UnloadModelRequest;
    const scheduling = {} as M3SchedulingInput;
    const dispatch = {} as M3ApiDispatchRequest;
    const cancel = {} as M3CancelInferenceRequest;
    const policy = {} as LanServerPolicy;
    const pairing = {} as PairingRequest;

    await runtimeHubClient.hardwareSnapshot();
    await runtimeHubClient.hardwareProfile();
    await runtimeHubClient.hardwareCompatibilityReport();
    await runtimeHubClient.storageStatus();
    await runtimeHubClient.installedModels();
    await runtimeHubClient.catalogSources();
    await runtimeHubClient.catalogReplaceSources([{ sourceId: "curated", endpoint: "https://models.example.test" }]);
    await runtimeHubClient.runtimes();
    await runtimeHubClient.refreshRuntimes(operation);
    await runtimeHubClient.schedulePlan(scheduling);
    await runtimeHubClient.catalogSearch({ ...operation, query: "qwen", limit: 20 });
    await runtimeHubClient.modelDownload({ ...operation, request: { model, acceptedLicenseSha256: "digest" } });
    await runtimeHubClient.modelUpdate({ ...operation, assetId: "asset", request: { model, acceptedLicenseSha256: "digest" } });
    await runtimeHubClient.modelActivateVersion({ ...operation, request: { assetId: "asset", versionKey: "a".repeat(64) } });
    await runtimeHubClient.modelPruneVersions({ ...operation, request: { assetId: "asset", confirmation: "PRUNE asset" } });
    await runtimeHubClient.modelDelete({ ...operation, request: { assetId: "asset", confirmation: "DELETE asset" } });
    await runtimeHubClient.cleanupOrphans({ ...operation, confirmation: "CLEAN ORPHANS" });
    await runtimeHubClient.cancelOperation("op-1");
    await runtimeHubClient.runtimeStatus({ ...operation, runtimeId: "ollama" });
    await runtimeHubClient.runtimeInventory({ ...operation, runtimeId: "ollama" });
    await runtimeHubClient.runtimeLoadModel({ ...operation, request: load });
    await runtimeHubClient.runtimeUnloadModel({ ...operation, request: unload });
    await runtimeHubClient.runtimeLogs({ ...operation, runtimeId: "ollama", maxBytes: 1024 });
    await runtimeHubClient.runtimeMetrics({ ...operation, runtimeId: "ollama" });
    await runtimeHubClient.runtimeSetConfig({ runtimeId: "ollama", values: {} });
    await runtimeHubClient.runtimeConfig("ollama");
    await runtimeHubClient.apiDispatch({ ...operation, request: dispatch });
    await runtimeHubClient.apiCancelInference({ ...operation, request: cancel });
    await runtimeHubClient.lanValidatePolicy(policy);
    await runtimeHubClient.lanConfigure(policy);
    await runtimeHubClient.lanDisable("DISABLE LAN API");
    await runtimeHubClient.lanPolicy();
    await runtimeHubClient.lanBeginPairing(pairing, 100, "127.0.0.1");
    await runtimeHubClient.lanCompletePairing("challenge", "123456", 101, "127.0.0.1");
    await runtimeHubClient.lanRevokeToken("token", 102, "127.0.0.1");
    await runtimeHubClient.lanTokens();
    await runtimeHubClient.lanAuditEvents();
    await runtimeHubClient.httpServerStart();
    await runtimeHubClient.httpServerStop();
    await runtimeHubClient.httpServerStatus();
    await runtimeHubClient.httpServerStoreTlsIdentity("tls-ref", "cert", "key");

    expect(invokeMock.mock.calls.map(([command]) => command)).toEqual([
      "m3_hardware_snapshot",
      "m3_hardware_profile",
      "m3_hardware_compatibility_report",
      "m3_storage_status",
      "m3_installed_models",
      "m3_catalog_sources",
      "m3_catalog_replace_sources",
      "m3_runtimes",
      "m3_refresh_runtimes",
      "m3_schedule_plan",
      "m3_catalog_search",
      "m3_model_download",
      "m3_model_update",
      "m3_model_activate_version",
      "m3_model_prune_versions",
      "m3_model_delete",
      "m3_cleanup_orphans",
      "m3_cancel_operation",
      "m3_runtime_status",
      "m3_runtime_inventory",
      "m3_runtime_load_model",
      "m3_runtime_unload_model",
      "m3_runtime_logs",
      "m3_runtime_metrics",
      "m3_runtime_set_config",
      "m3_runtime_config",
      "m3_api_dispatch",
      "m3_api_cancel_inference",
      "m3_lan_validate_policy",
      "m3_lan_configure",
      "m3_lan_disable",
      "m3_lan_policy",
      "m3_lan_begin_pairing",
      "m3_lan_complete_pairing",
      "m3_lan_revoke_token",
      "m3_lan_tokens",
      "m3_lan_audit_events",
      "m3_http_server_start",
      "m3_http_server_stop",
      "m3_http_server_status",
      "m3_http_server_store_tls_identity",
    ]);
    expect(invokeMock).toHaveBeenCalledWith("m3_catalog_search", {
      operationId: "op-1",
      timeoutMs: 1000,
      query: "qwen",
      limit: 20,
    });
    expect(invokeMock).toHaveBeenCalledWith("m3_lan_complete_pairing", {
      challengeId: "challenge",
      pairingCode: "123456",
      nowMs: 101,
      remoteAddress: "127.0.0.1",
    });
    expect(invokeMock).toHaveBeenCalledWith("m3_http_server_store_tls_identity", {
      reference: "tls-ref",
      certificatePem: "cert",
      privateKeyPem: "key",
    });
  });

  it("creates traceable operation ids and hashes the exact license declaration", async () => {
    expect(createM3OperationId("catalog")).toMatch(/^catalog-/);
    expect(await sha256Text("abc")).toBe("ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
  });
});
