import { beforeEach, describe, expect, it, vi } from "vitest";

const invokeMock = vi.fn();
vi.mock("@tauri-apps/api/core", () => ({ invoke: (...args: unknown[]) => invokeMock(...args) }));

import type { ApprovedInstallPreview, InstalledPackageState, McpOAuthServerRegistration, PackageCatalogEntry, WorkflowRunHistory } from "../lib/ecosystemClient";
import { makeWorkflowNode, useEcosystemStore } from "./ecosystemStore";

const catalogEntry = {
  manifest: {
    schema_version: 1,
    package_id: "first-party.skill",
    version: "1.0.0",
    kind: "skill",
    display_name: "First party skill",
    description: "Fixture",
    content: [],
    permissions: [],
    mcp_requirements: [],
    provenance: { publisher: "Little Monkey", source: {}, source_revision: "fixture", build_reproducible: true },
  },
  bundle_sha256: "a".repeat(64),
  trust: null,
  available: true,
  validation_error: null,
} satisfies PackageCatalogEntry;

const preview = {
  preview: {
    package_id: "first-party.skill",
    version: "1.0.0",
    kind: "skill",
    source: {},
    bundle_sha256: "a".repeat(64),
    trust: { signed: true, trust_root_id: "root", key_id: "key", registry_snapshot_sha256: "b".repeat(64), revocation: {} },
    permissions: [],
    permission_diff: null,
    mcp_actions_separate: [],
    file_count: 1,
    total_bytes: 10,
    warnings: [],
  },
  approval_digest: "approval-digest",
} satisfies ApprovedInstallPreview;

const installed = {
  schema_version: 1,
  sequence: 1,
  package_id: "first-party.skill",
  active_version: "1.0.0",
  versions: {},
  activation_history: ["1.0.0"],
  pinned_version: null,
  enabled: true,
  revoked: false,
  tombstoned: false,
  approved_permissions: [],
} satisfies InstalledPackageState;

beforeEach(() => {
  invokeMock.mockReset();
  useEcosystemStore.setState({
    catalog: [], installed: [], plugins: [], installPreview: null, workflows: [], workflowIr: null,
    histories: [], activeRunId: null, inspectedNode: null, oauthServers: [], oauthMetadata: {},
    busy: {}, error: null,
  });
});

describe("ecosystemStore", () => {
  it("creates runnable defaults for Browser, Git, and pull-request workflow nodes", () => {
    expect(makeWorkflowNode("browser", 1)).toMatchObject({
      kind: { kind: "browser", action: "inspect", effect: "read_only" },
      inputs: { arguments: { value: { value: { sessionId: "" } } } },
    });
    expect(makeWorkflowNode("git", 2)).toMatchObject({
      kind: { kind: "git", action: "inspect_worktree", effect: "read_only" },
      inputs: { arguments: { value: { value: { worktreeId: "" } } } },
    });
    expect(makeWorkflowNode("pull_request", 3)).toMatchObject({
      kind: { kind: "pull_request", action: "read_pull_request", effect: "read_only" },
      inputs: { arguments: { value: { value: { worktreeId: "", number: 1 } } } },
    });
  });

  it("seeds signed catalog state then installs only the reviewed approval digest", async () => {
    invokeMock
      .mockResolvedValueOnce([catalogEntry])
      .mockResolvedValueOnce([catalogEntry])
      .mockResolvedValueOnce([])
      .mockResolvedValueOnce([])
      .mockResolvedValueOnce(preview)
      .mockResolvedValueOnce(installed)
      .mockResolvedValueOnce([]);
    await useEcosystemStore.getState().refreshPackages();
    await useEcosystemStore.getState().previewPackage("first-party.skill", "1.0.0");
    await useEcosystemStore.getState().installPackage(true);
    expect(useEcosystemStore.getState().installed).toEqual([installed]);
    expect(invokeMock).toHaveBeenCalledWith("m4_packages_install", {
      authorization: { package_id: "first-party.skill", version: "1.0.0", approval_digest: "approval-digest", approved: true },
      nowUnixMs: expect.any(Number),
    });
  });

  it("drops the tombstoned state from the installed list on uninstall", async () => {
    useEcosystemStore.setState({ installed: [installed] });
    invokeMock
      .mockResolvedValueOnce({ ...installed, sequence: 2, active_version: null, enabled: false, tombstoned: true })
      .mockResolvedValueOnce([]);
    await useEcosystemStore.getState().uninstallPackage("first-party.skill");
    expect(invokeMock).toHaveBeenCalledWith("m4_packages_uninstall", { packageId: "first-party.skill" });
    expect(useEcosystemStore.getState().installed).toEqual([]);
  });

  it("restores OAuth registrations and exposes only token metadata", async () => {
    const registration = {
      server: { contract_version: 1, issuer: "https://auth.example/", authorization_endpoint: "https://auth.example/authorize", token_endpoint: "https://auth.example/token", revocation_endpoint: null, supported_scopes: ["read"], supports_pkce_s256: true },
      client: { server_id: "server", client_id: "desktop", redirect_uri: "http://127.0.0.1/callback", requested_scopes: ["read"] },
    } satisfies McpOAuthServerRegistration;
    const metadata = { token_reference: { vault_id: "keychain", reference_id: "opaque-ref" }, token_type: "Bearer", granted_scopes: ["read"], issued_unix_ms: 1, expires_unix_ms: 2 };
    invokeMock.mockResolvedValueOnce([registration]).mockResolvedValueOnce(metadata);
    await useEcosystemStore.getState().refreshOAuthServers();
    expect(useEcosystemStore.getState().oauthServers).toEqual([registration]);
    expect(useEcosystemStore.getState().oauthMetadata.server).toEqual(metadata);
    expect(JSON.stringify(useEcosystemStore.getState())).not.toContain("access_token");
  });

  it("updates with the exact permission diff the user reviewed instead of re-previewing", async () => {
    const updatePreview: ApprovedInstallPreview = {
      ...preview,
      preview: {
        ...preview.preview,
        version: "2.0.0",
        permission_diff: {
          added: [{ permission_id: "network", kind: "network", scope: "https://example.com", reason: "Fetch docs" }],
          removed: [],
          unchanged: [],
          approval_digest: "reviewed-update-digest",
          requires_new_approval: true,
        },
      },
    };
    const updated = { ...installed, sequence: 2, active_version: "2.0.0" };
    useEcosystemStore.setState({ installed: [installed], installPreview: updatePreview });
    invokeMock.mockResolvedValueOnce(updated).mockResolvedValueOnce([]);
    await useEcosystemStore.getState().updatePackage("first-party.skill", "2.0.0", true);
    expect(invokeMock).toHaveBeenCalledTimes(2);
    expect(invokeMock).toHaveBeenCalledWith("m4_packages_update", {
      packageId: "first-party.skill",
      version: "2.0.0",
      approval: {
        package_id: "first-party.skill",
        from_version: "1.0.0",
        to_version: "2.0.0",
        approval_digest: "reviewed-update-digest",
        approved: true,
      },
      nowUnixMs: expect.any(Number),
    });
    expect(useEcosystemStore.getState().installPreview).toBeNull();
  });

  it("tracks an active durable run and stores its completed history", async () => {
    const history = { run_id: "run-1", workflow_id: "flow", status: "succeeded" } as unknown as WorkflowRunHistory;
    let resolveRun: ((value: WorkflowRunHistory) => void) | undefined;
    invokeMock.mockImplementationOnce(() => new Promise<WorkflowRunHistory>((resolve) => { resolveRun = resolve; }));
    const promise = useEcosystemStore.getState().runWorkflow("flow", { run_id: "run-1", inputs: {}, secret_bindings: {}, trigger: { kind: "manual" } });
    expect(useEcosystemStore.getState().activeRunId).toBe("run-1");
    resolveRun?.(history);
    await promise;
    expect(useEcosystemStore.getState().activeRunId).toBeNull();
    expect(useEcosystemStore.getState().histories).toEqual([history]);
  });
});
