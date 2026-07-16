import { create } from "zustand";
import {
  ecosystemClient,
  type ApprovedInstallPreview,
  type InstalledPackageState,
  type McpOAuthServerRegistration,
  type NodeRunRecord,
  type OAuthAuthorizationPlan,
  type OAuthTokenMetadata,
  type PackageCatalogEntry,
  type PluginRuntimeDescriptor,
  type PortablePackageExport,
  type SemanticVersion,
  type WorkflowDefinition,
  type WorkflowIr,
  type WorkflowNode,
  type WorkflowNodeKind,
  type WorkflowRunHistory,
  type WorkflowRunRequest,
} from "../lib/ecosystemClient";

function errorText(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

function baseNode(nodeId: string, kind: WorkflowNodeKind): WorkflowNode {
  return {
    node_id: nodeId,
    kind,
    inputs: {},
    secret_ids: [],
    permission_policy: { permission_ids: [], approval_node_id: null },
    retry: {
      maximum_attempts: 1,
      initial_backoff_ms: 0,
      maximum_backoff_ms: 0,
      retry_on: [],
    },
    timeout_ms: 60_000,
    estimate: {
      model_calls: 0,
      input_tokens: 0,
      output_tokens: 0,
      cost_microunits: 0,
    },
    idempotency: { kind: "none" },
    replay: "safe",
    guard: null,
  };
}

export function makeWorkflowNode(kind: WorkflowNodeKind["kind"], index: number): WorkflowNode {
  const nodeId = `${kind}-${index}`;
  if (kind === "prompt_model") {
    const node = baseNode(nodeId, { kind, model_selector: "ollama:qwen2.5:7b" });
    node.inputs.prompt = { source: "literal", value: { kind: "string", value: "" } };
    node.estimate = { model_calls: 1, input_tokens: 4_096, output_tokens: 4_096, cost_microunits: 0 };
    return node;
  }
  if (kind === "agent" || kind === "subagent") {
    const node = baseNode(nodeId, { kind, agent_profile: "default", effect: "read_only" });
    node.inputs.prompt = { source: "literal", value: { kind: "string", value: "" } };
    return node;
  }
  if (kind === "tool") {
    const node = baseNode(nodeId, { kind, tool_id: "builtin.read_file", effect: "read_only" });
    node.inputs.arguments = { source: "literal", value: { kind: "json", value: { path: "" } } };
    return node;
  }
  if (kind === "mcp") {
    const node = baseNode(nodeId, { kind, server_id: "server", tool_name: "tool", effect: "external_mutation" });
    node.inputs.arguments = { source: "literal", value: { kind: "json", value: {} } };
    node.inputs.approval = { source: "literal", value: { kind: "boolean", value: false } };
    node.replay = "requires_approval";
    return node;
  }
  if (kind === "browser") {
    const node = baseNode(nodeId, { kind, action: "inspect", effect: "read_only" });
    node.inputs.arguments = {
      source: "literal",
      value: { kind: "json", value: { sessionId: "" } },
    };
    return node;
  }
  if (kind === "git") {
    const node = baseNode(nodeId, { kind, action: "inspect_worktree", effect: "read_only" });
    node.inputs.arguments = {
      source: "literal",
      value: { kind: "json", value: { worktreeId: "" } },
    };
    return node;
  }
  if (kind === "pull_request") {
    const node = baseNode(nodeId, { kind, action: "read_pull_request", effect: "read_only" });
    node.inputs.arguments = {
      source: "literal",
      value: { kind: "json", value: { worktreeId: "", number: 1 } },
    };
    return node;
  }
  if (kind === "shell") {
    const node = baseNode(nodeId, { kind, shell_profile: "posix-sh" });
    node.inputs.command = { source: "literal", value: { kind: "string", value: "" } };
    node.inputs.approval = { source: "literal", value: { kind: "boolean", value: false } };
    node.replay = "requires_approval";
    return node;
  }
  if (kind === "verify") {
    const node = baseNode(nodeId, { kind, verifier_id: "sha256" });
    node.inputs.input = { source: "literal", value: { kind: "json", value: null } };
    return node;
  }
  if (kind === "transform") {
    const node = baseNode(nodeId, { kind, transform_id: "identity" });
    node.inputs.input = { source: "literal", value: { kind: "json", value: null } };
    return node;
  }
  if (kind === "condition") {
    const node = baseNode(nodeId, { kind });
    node.inputs.condition = { source: "literal", value: { kind: "boolean", value: true } };
    return node;
  }
  if (kind === "bounded_loop") {
    const node = baseNode(nodeId, { kind, maximum_iterations: 10 });
    node.inputs.input = { source: "literal", value: { kind: "json", value: null } };
    return node;
  }
  if (kind === "human_approval") {
    const node = baseNode(nodeId, { kind, approval_policy_id: "explicit-user" });
    node.inputs.summary = { source: "literal", value: { kind: "string", value: "Approve this action" } };
    return node;
  }
  if (kind === "artifact") {
    const node = baseNode(nodeId, { kind, media_type: "text/plain" });
    node.inputs.content = { source: "literal", value: { kind: "string", value: "" } };
    return node;
  }
  const node = baseNode(nodeId, { kind: "output" });
  node.inputs.value = { source: "literal", value: { kind: "json", value: null } };
  return node;
}

export function newWorkflowDefinition(id = `workflow-${crypto.randomUUID()}`): WorkflowDefinition {
  const outputNode = makeWorkflowNode("output", 1);
  return {
    schema_version: 1,
    workflow_id: id,
    workflow_version: 1,
    name: "New workflow",
    inputs: {},
    secrets: {},
    nodes: [outputNode],
    outputs: {
      result: {
        value_type: { kind: "json" },
        binding: { source: "node_output", node_id: outputNode.node_id, port: "out" },
      },
    },
    budgets: {
      maximum_node_executions: 100,
      maximum_model_calls: 10,
      maximum_input_tokens: 100_000,
      maximum_output_tokens: 100_000,
      maximum_cost_microunits: 10_000_000,
      maximum_wall_time_ms: 60_000,
    },
    maximum_concurrency: 4,
    triggers: [{ kind: "manual" }],
  };
}

interface EcosystemStore {
  catalog: PackageCatalogEntry[];
  installed: InstalledPackageState[];
  plugins: PluginRuntimeDescriptor[];
  installPreview: ApprovedInstallPreview | null;
  workflows: WorkflowDefinition[];
  workflowIr: WorkflowIr | null;
  histories: WorkflowRunHistory[];
  activeRunId: string | null;
  inspectedNode: NodeRunRecord | null;
  oauthServers: McpOAuthServerRegistration[];
  oauthMetadata: Record<string, OAuthTokenMetadata | null>;
  busy: Record<string, boolean>;
  error: string | null;

  clearError: () => void;
  refreshPackages: () => Promise<void>;
  refreshPluginRuntime: () => Promise<void>;
  importPortablePackage: (portable: PortablePackageExport, expectedBundleSha256?: string | null) => Promise<ApprovedInstallPreview>;
  previewPackage: (packageId: string, version: SemanticVersion) => Promise<ApprovedInstallPreview>;
  installPackage: (approved: boolean) => Promise<InstalledPackageState>;
  updatePackage: (packageId: string, version: SemanticVersion, approved: boolean) => Promise<InstalledPackageState>;
  setPackageEnabled: (packageId: string, enabled: boolean) => Promise<void>;
  pinPackage: (packageId: string, version: SemanticVersion | null) => Promise<void>;
  rollbackPackage: (packageId: string) => Promise<void>;
  uninstallPackage: (packageId: string) => Promise<void>;
  activatePluginWorkflow: (packageId: string, contentPath: string) => Promise<void>;
  deactivatePluginWorkflow: (packageId: string, contentPath: string) => Promise<void>;

  registerOAuth: (registration: McpOAuthServerRegistration) => Promise<void>;
  refreshOAuthServers: () => Promise<void>;
  beginOAuth: (serverId: string) => Promise<OAuthAuthorizationPlan>;
  completeOAuth: (serverId: string, state: string, code: string) => Promise<OAuthTokenMetadata>;
  refreshOAuth: (serverId: string) => Promise<OAuthTokenMetadata>;
  revokeOAuth: (serverId: string) => Promise<void>;
  loadOAuthMetadata: (serverId: string) => Promise<OAuthTokenMetadata | null>;

  refreshWorkflows: () => Promise<void>;
  validateWorkflow: (definition: WorkflowDefinition) => Promise<WorkflowIr>;
  saveWorkflow: (definition: WorkflowDefinition, exists: boolean) => Promise<WorkflowIr>;
  deleteWorkflow: (workflowId: string) => Promise<void>;
  runWorkflow: (workflowId: string, request: WorkflowRunRequest) => Promise<WorkflowRunHistory>;
  cancelWorkflow: (runId: string) => Promise<boolean>;
  refreshHistories: () => Promise<void>;
  inspectNode: (runId: string, nodeId: string) => Promise<NodeRunRecord>;
}

export const useEcosystemStore = create<EcosystemStore>((set, get) => {
  const perform = async <T>(key: string, task: () => Promise<T>): Promise<T> => {
    set((state) => ({ busy: { ...state.busy, [key]: true }, error: null }));
    try {
      return await task();
    } catch (error) {
      set({ error: errorText(error) });
      throw error;
    } finally {
      set((state) => ({ busy: { ...state.busy, [key]: false } }));
    }
  };

  const replaceInstalled = (next: InstalledPackageState) => {
    set((state) => {
      const others = state.installed.filter((item) => item.package_id !== next.package_id);
      return {
        installed: (next.tombstoned ? others : [...others, next])
          .sort((left, right) => left.package_id.localeCompare(right.package_id)),
      };
    });
  };

  return {
    catalog: [],
    installed: [],
    plugins: [],
    installPreview: null,
    workflows: [],
    workflowIr: null,
    histories: [],
    activeRunId: null,
    inspectedNode: null,
    oauthServers: [],
    oauthMetadata: {},
    busy: {},
    error: null,

    clearError: () => set({ error: null }),
    refreshPackages: () => perform("packages", async () => {
      await ecosystemClient.seedPackages();
      const [catalog, installed, plugins] = await Promise.all([
        ecosystemClient.packageCatalog(),
        ecosystemClient.installedPackages(),
        ecosystemClient.pluginRuntime(),
      ]);
      set({ catalog, installed, plugins });
    }),
    refreshPluginRuntime: () => perform("plugins", async () => {
      set({ plugins: await ecosystemClient.pluginRuntime() });
    }),
    importPortablePackage: (portable, expectedBundleSha256 = portable.bundle_sha256) => perform("package-import", async () => {
      const entry = await ecosystemClient.importPortablePackage(portable, expectedBundleSha256);
      const preview = await ecosystemClient.previewPackage(
        entry.manifest.package_id,
        entry.manifest.version,
      );
      set((state) => ({
        catalog: [
          ...state.catalog.filter((candidate) => !(
            candidate.manifest.package_id === entry.manifest.package_id
            && candidate.manifest.version === entry.manifest.version
          )),
          entry,
        ].sort((left, right) => left.manifest.package_id.localeCompare(right.manifest.package_id)),
        installPreview: preview,
      }));
      return preview;
    }),
    previewPackage: (packageId, version) => perform("package-preview", async () => {
      const preview = await ecosystemClient.previewPackage(packageId, version);
      set({ installPreview: preview });
      return preview;
    }),
    installPackage: (approved) => perform("package-install", async () => {
      const preview = get().installPreview;
      if (!preview) throw new Error("Open an install preview first.");
      const installed = await ecosystemClient.installPackage({
        package_id: preview.preview.package_id,
        version: preview.preview.version,
        approval_digest: preview.approval_digest,
        approved,
      });
      replaceInstalled(installed);
      set({ installPreview: null });
      await get().refreshPluginRuntime();
      return installed;
    }),
    updatePackage: (packageId, version, approved) => perform("package-update", async () => {
      const preview = get().installPreview;
      if (!preview
        || preview.preview.package_id !== packageId
        || preview.preview.version !== version) {
        throw new Error("Open and review this exact package update first.");
      }
      const current = get().installed.find((item) => item.package_id === packageId);
      const diff = preview.preview.permission_diff;
      const approval = diff?.requires_new_approval && current?.active_version
        ? {
            package_id: packageId,
            from_version: current.active_version,
            to_version: version,
            approval_digest: diff.approval_digest,
            approved,
          }
        : null;
      const next = await ecosystemClient.updatePackage(packageId, version, approval);
      replaceInstalled(next);
      set({ installPreview: null });
      await get().refreshPluginRuntime();
      return next;
    }),
    setPackageEnabled: (packageId, enabled) => perform(`package-enable-${packageId}`, async () => {
      replaceInstalled(await ecosystemClient.setPackageEnabled(packageId, enabled));
      await get().refreshPluginRuntime();
    }),
    pinPackage: (packageId, version) => perform(`package-pin-${packageId}`, async () => {
      replaceInstalled(await ecosystemClient.pinPackage(packageId, version));
      await get().refreshPluginRuntime();
    }),
    rollbackPackage: (packageId) => perform(`package-rollback-${packageId}`, async () => {
      replaceInstalled(await ecosystemClient.rollbackPackage(packageId));
      await get().refreshPluginRuntime();
    }),
    uninstallPackage: (packageId) => perform(`package-uninstall-${packageId}`, async () => {
      replaceInstalled(await ecosystemClient.uninstallPackage(packageId));
      await get().refreshPluginRuntime();
    }),
    activatePluginWorkflow: (packageId, contentPath) => perform(`plugin-workflow-${packageId}-${contentPath}`, async () => {
      await ecosystemClient.activatePluginWorkflow(packageId, contentPath);
      await Promise.all([get().refreshPluginRuntime(), get().refreshWorkflows()]);
    }),
    deactivatePluginWorkflow: (packageId, contentPath) => perform(`plugin-workflow-${packageId}-${contentPath}`, async () => {
      await ecosystemClient.deactivatePluginWorkflow(packageId, contentPath);
      await Promise.all([get().refreshPluginRuntime(), get().refreshWorkflows()]);
    }),

    registerOAuth: (registration) => perform("oauth-register", async () => {
      await ecosystemClient.registerOAuth(registration);
      set((state) => ({
        oauthServers: [...state.oauthServers, registration]
          .sort((left, right) => left.client.server_id.localeCompare(right.client.server_id)),
        oauthMetadata: { ...state.oauthMetadata, [registration.client.server_id]: null },
      }));
    }),
    refreshOAuthServers: () => perform("oauth-servers", async () => {
      const oauthServers = await ecosystemClient.oauthServers();
      set({ oauthServers });
      await Promise.all(oauthServers.map(({ client }) => get().loadOAuthMetadata(client.server_id)));
    }),
    beginOAuth: (serverId) => perform("oauth-begin", () => ecosystemClient.beginOAuth(serverId)),
    completeOAuth: (serverId, state, code) => perform("oauth-complete", async () => {
      const metadata = await ecosystemClient.completeOAuth(serverId, state, code);
      set((current) => ({ oauthMetadata: { ...current.oauthMetadata, [serverId]: metadata } }));
      return metadata;
    }),
    refreshOAuth: (serverId) => perform("oauth-refresh", async () => {
      const metadata = await ecosystemClient.refreshOAuth(serverId);
      set((current) => ({ oauthMetadata: { ...current.oauthMetadata, [serverId]: metadata } }));
      return metadata;
    }),
    revokeOAuth: (serverId) => perform("oauth-revoke", async () => {
      await ecosystemClient.revokeOAuth(serverId);
      set((current) => ({ oauthMetadata: { ...current.oauthMetadata, [serverId]: null } }));
    }),
    loadOAuthMetadata: (serverId) => perform("oauth-metadata", async () => {
      const metadata = await ecosystemClient.oauthMetadata(serverId);
      set((current) => ({ oauthMetadata: { ...current.oauthMetadata, [serverId]: metadata } }));
      return metadata;
    }),

    refreshWorkflows: () => perform("workflows", async () => {
      const [workflows, histories] = await Promise.all([
        ecosystemClient.workflows(),
        ecosystemClient.workflowHistories(),
      ]);
      set({ workflows, histories });
    }),
    validateWorkflow: (definition) => perform("workflow-validate", async () => {
      const ir = await ecosystemClient.validateWorkflow(definition);
      set({ workflowIr: ir });
      return ir;
    }),
    saveWorkflow: (definition, exists) => perform("workflow-save", async () => {
      const ir = exists
        ? await ecosystemClient.updateWorkflow(definition)
        : await ecosystemClient.createWorkflow(definition);
      await get().refreshWorkflows();
      set({ workflowIr: ir });
      return ir;
    }),
    deleteWorkflow: (workflowId) => perform("workflow-delete", async () => {
      await ecosystemClient.deleteWorkflow(workflowId);
      set((state) => ({ workflows: state.workflows.filter((item) => item.workflow_id !== workflowId) }));
    }),
    runWorkflow: (workflowId, request) => perform("workflow-run", async () => {
      set({ activeRunId: request.run_id });
      try {
        const history = await ecosystemClient.runWorkflow(workflowId, request);
        set((state) => ({
          histories: [history, ...state.histories.filter((item) => item.run_id !== history.run_id)],
        }));
        return history;
      } finally {
        set({ activeRunId: null });
      }
    }),
    cancelWorkflow: (runId) => perform("workflow-cancel", () => ecosystemClient.cancelWorkflow(runId)),
    refreshHistories: () => perform("histories", async () => {
      set({ histories: await ecosystemClient.workflowHistories() });
    }),
    inspectNode: (runId, nodeId) => perform("node-inspect", async () => {
      const record = await ecosystemClient.inspectWorkflowNode(runId, nodeId);
      set({ inspectedNode: record });
      return record;
    }),
  };
});
