import { beforeEach, describe, expect, it, vi } from "vitest";

const invokeMock = vi.fn();
vi.mock("@tauri-apps/api/core", () => ({ invoke: (...args: unknown[]) => invokeMock(...args) }));

import { ecosystemClient, type McpUiManifest, type PortablePackageExport, type RegistrySnapshot, type WorkflowDefinition, type WorkflowRunRequest } from "./ecosystemClient";

const manifest: McpUiManifest = {
  contract_version: 1,
  server_id: "server",
  resource_uri: "ui://server/app.html",
  resource_sha256: "a".repeat(64),
  entry_media_type: "text/html",
  network_origins: [],
  host_actions: {},
  text_fallback: "fallback",
};

beforeEach(() => invokeMock.mockReset());

describe("ecosystemClient command contracts", () => {
  it("passes explicit package authorization and OAuth server discovery to M4", async () => {
    invokeMock.mockResolvedValue(undefined);
    await ecosystemClient.installPackage({ package_id: "pkg", version: "1.0.0", approval_digest: "digest", approved: true }, 123);
    expect(invokeMock).toHaveBeenNthCalledWith(1, "m4_packages_install", {
      authorization: { package_id: "pkg", version: "1.0.0", approval_digest: "digest", approved: true },
      nowUnixMs: 123,
    });
    await ecosystemClient.oauthServers();
    expect(invokeMock).toHaveBeenNthCalledWith(2, "m4_mcp_oauth_servers");
  });

  it("binds MCP UI approval to the session capability and exact request", async () => {
    invokeMock.mockResolvedValue(undefined);
    const request = {
      session_id: "session",
      server_id: "server",
      resource_sha256: "a".repeat(64),
      action_id: "copy",
      payload: { text: "hello" },
    };
    await ecosystemClient.openMcpUi(manifest, [1, 2, 3], ["clipboard.write"]);
    await ecosystemClient.prepareMcpUiAction("session", "opaque-capability", request);
    await ecosystemClient.decideMcpUiAction("challenge", true);
    await ecosystemClient.authorizeMcpUiAction("session", "opaque-capability", request);
    expect(invokeMock).toHaveBeenNthCalledWith(1, "m4_mcp_ui_open", { manifest, resourceBytes: [1, 2, 3], grantedPermissions: ["clipboard.write"] });
    expect(invokeMock).toHaveBeenNthCalledWith(2, "m4_mcp_ui_prepare_action", { sessionId: "session", presentedCapability: "opaque-capability", request });
    expect(invokeMock).toHaveBeenNthCalledWith(3, "m4_mcp_ui_decide_action", { challengeId: "challenge", approved: true });
    expect(invokeMock).toHaveBeenNthCalledWith(4, "m4_mcp_ui_authorize_action", { sessionId: "session", presentedCapability: "opaque-capability", request });
  });

  it("keeps workflow replay and persistent trigger arguments explicit", async () => {
    invokeMock.mockResolvedValue(undefined);
    const request: WorkflowRunRequest = { run_id: "replay-1", inputs: {}, secret_bindings: {}, trigger: { kind: "manual" } };
    await ecosystemClient.replayWorkflow("workflow", "source-run", "node-2", true, request);
    await ecosystemClient.registerWorkflowTriggers("workflow");
    expect(invokeMock).toHaveBeenNthCalledWith(1, "m4_workflows_replay", {
      workflowId: "workflow",
      sourceRunId: "source-run",
      boundaryNodeId: "node-2",
      replayApprovalGranted: true,
      request,
    });
    expect(invokeMock).toHaveBeenNthCalledWith(2, "m4_workflows_register_triggers", { workflowId: "workflow" });
  });

  it("passes definitions without reshaping typed DAG JSON", async () => {
    invokeMock.mockResolvedValue(undefined);
    const definition = { workflow_id: "flow", workflow_version: 1, nodes: [] } as unknown as WorkflowDefinition;
    await ecosystemClient.validateWorkflow(definition);
    expect(invokeMock).toHaveBeenCalledWith("m4_workflows_validate", { definition });
  });

  it("keeps portable acquisition digest pinning and plugin workflow ownership explicit", async () => {
    invokeMock.mockResolvedValue(undefined);
    const portable = {
      schema_version: 1,
      bundle_sha256: "a".repeat(64),
      manifest: { package_id: "com.example.plugin", version: "1.0.0" },
      files_hex: { "instructions.md": "6869" },
    } as unknown as PortablePackageExport;
    await ecosystemClient.importPortablePackage(portable, portable.bundle_sha256, 456);
    await ecosystemClient.pluginRuntime();
    await ecosystemClient.activePluginSnapshots();
    await ecosystemClient.activatePluginWorkflow("com.example.plugin", "workflows/review.json");
    await ecosystemClient.deactivatePluginWorkflow("com.example.plugin", "workflows/review.json");
    expect(invokeMock).toHaveBeenNthCalledWith(1, "m4_packages_import_portable", {
      portable,
      expectedBundleSha256: portable.bundle_sha256,
      nowUnixMs: 456,
    });
    expect(invokeMock).toHaveBeenNthCalledWith(2, "m4_plugins_runtime");
    expect(invokeMock).toHaveBeenNthCalledWith(3, "m4_plugins_active_snapshot");
    expect(invokeMock).toHaveBeenNthCalledWith(4, "m4_plugins_activate_workflow", {
      packageId: "com.example.plugin",
      contentPath: "workflows/review.json",
    });
    expect(invokeMock).toHaveBeenNthCalledWith(5, "m4_plugins_deactivate_workflow", {
      packageId: "com.example.plugin",
      contentPath: "workflows/review.json",
    });
  });

  it("forwards the local team-approved toggle without any role/permission argument", async () => {
    invokeMock.mockResolvedValue(undefined);
    await ecosystemClient.setPackageTeamApproved("com.example.collection", true);
    expect(invokeMock).toHaveBeenCalledWith("m4_packages_set_team_approved", {
      packageId: "com.example.collection",
      teamApproved: true,
    });
  });

  it("keeps the additional-registry-source lifecycle (list/add/remove/verify) explicit", async () => {
    invokeMock.mockResolvedValue(undefined);
    await ecosystemClient.listRegistrySources();
    await ecosystemClient.addRegistrySource("team-catalog", "Team Catalog", "https://team.example.com/registry.json", 111);
    await ecosystemClient.removeRegistrySource("team-catalog");
    const snapshot = { registry_id: "team-catalog", sequence: 1 } as unknown as RegistrySnapshot;
    await ecosystemClient.verifyRegistrySource("team-catalog", snapshot, 222);
    expect(invokeMock).toHaveBeenNthCalledWith(1, "m4_registries_list");
    expect(invokeMock).toHaveBeenNthCalledWith(2, "m4_registries_add", {
      sourceId: "team-catalog",
      displayName: "Team Catalog",
      location: "https://team.example.com/registry.json",
      nowUnixMs: 111,
    });
    expect(invokeMock).toHaveBeenNthCalledWith(3, "m4_registries_remove", { sourceId: "team-catalog" });
    expect(invokeMock).toHaveBeenNthCalledWith(4, "m4_registries_verify", {
      sourceId: "team-catalog",
      snapshot,
      nowUnixMs: 222,
    });
  });
});
