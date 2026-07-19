import { describe, expect, it } from "vitest";

import type { ModelInfo, OllamaModelInfo, ProviderConfig } from "../store/modelStore";
import type { ConnectorAccount } from "../store/connectorsStore";
import type { McpServerInfo } from "../store/mcpStore";
import type { NativeSkillDescriptor } from "./nativeSkillsClient";
import type { PackageCatalogEntry, PluginRuntimeDescriptor, WorkflowDefinition, WorkflowRunHistory } from "./ecosystemClient";
import {
  scoreConnector,
  scoreLocalModel,
  scoreMcpServer,
  scoreOllamaModel,
  scorePlugin,
  scoreProviderModel,
  scoreSkill,
  scoreWorkflow,
  TRUST_DIMENSION_KEYS,
  type ModelUsageLookup,
} from "./trustScorecards";

const emptyUsage: ModelUsageLookup = { byModel: {} };

function localModel(overrides: Partial<ModelInfo> = {}): ModelInfo {
  return {
    id: "qwen-7b",
    name: "Qwen 7B",
    repo: "qwen/qwen-7b-gguf",
    file: "qwen-7b.Q4_K_M.gguf",
    size_gb: 4.5,
    tool_calling: true,
    installed: true,
    path: "/models/qwen-7b.gguf",
    is_external: false,
    kind: "chat",
    ...overrides,
  };
}

describe("trustScorecards", () => {
  it("computes every dimension for a scorecard", () => {
    const card = scoreLocalModel(localModel(), emptyUsage, "stopped", false);
    expect(new Set(Object.keys(card.dimensions))).toEqual(new Set(TRUST_DIMENSION_KEYS));
  });

  describe("scoreLocalModel", () => {
    it("marks a curated, downloaded model good on privacy/cost/security/provenance, unknown on quality", () => {
      const card = scoreLocalModel(localModel(), emptyUsage, "stopped", false);
      expect(card.dimensions.privacy.level).toBe("good");
      expect(card.dimensions.cost.level).toBe("good");
      expect(card.dimensions.quality.level).toBe("unknown");
      expect(card.dimensions.provenance.evidence[0].fact).toMatch(/curated model catalog/);
    });

    it("cites is_external for a user-added local model's provenance", () => {
      const card = scoreLocalModel(localModel({ is_external: true }), emptyUsage, "stopped", false);
      expect(card.dimensions.provenance.evidence[0].fact).toMatch(/models_add_external/);
    });

    it("derives reliability from the live llama-server status only for the active model", () => {
      const active = scoreLocalModel(localModel(), emptyUsage, "error", true);
      expect(active.dimensions.reliability.level).toBe("poor");
      expect(active.dimensions.reliability.evidence[0].fact).toMatch(/"error"/);

      const inactive = scoreLocalModel(localModel(), emptyUsage, "error", false);
      expect(inactive.dimensions.reliability.level).toBe("unknown");
    });

    it("cites real usage-history evidence for quality when turns were recorded", () => {
      const usage: ModelUsageLookup = { byModel: { "Qwen 7B": { promptTokens: 10, completionTokens: 20, totalTokens: 30, turns: 3 } } };
      const card = scoreLocalModel(localModel(), usage, "stopped", false);
      expect(card.dimensions.quality.level).toBe("unknown");
      expect(card.dimensions.quality.evidence[0].fact).toMatch(/3 completed turn/);
    });
  });

  describe("scoreOllamaModel", () => {
    const cloudModel: OllamaModelInfo = {
      name: "qwen2.5:7b-cloud",
      size_bytes: 1000,
      is_cloud: true,
      tool_calling: true,
      vision: false,
      modified_at: "2026-01-01",
    };
    const localTag: OllamaModelInfo = { ...cloudModel, name: "qwen2.5:7b", is_cloud: false };

    it("marks a cloud tag fair on privacy/cost, a local tag good", () => {
      expect(scoreOllamaModel(cloudModel, emptyUsage).dimensions.privacy.level).toBe("fair");
      expect(scoreOllamaModel(cloudModel, emptyUsage).dimensions.cost.level).toBe("fair");
      expect(scoreOllamaModel(localTag, emptyUsage).dimensions.privacy.level).toBe("good");
      expect(scoreOllamaModel(localTag, emptyUsage).dimensions.cost.level).toBe("good");
    });
  });

  describe("scoreProviderModel", () => {
    const provider: ProviderConfig = { id: "openai", label: "OpenAI", base_url: "https://api.openai.com", is_custom: false, has_key: true };

    it("marks cloud provider models fair on privacy (data leaves device) and good security when a key is stored", () => {
      const card = scoreProviderModel(provider, "gpt-4o", emptyUsage, {});
      expect(card.dimensions.privacy.level).toBe("fair");
      expect(card.dimensions.security.level).toBe("good");
      expect(card.dimensions.cost.level).toBe("unknown");
    });

    it("surfaces a recorded key error as poor reliability, quoting the message", () => {
      const card = scoreProviderModel(provider, "gpt-4o", emptyUsage, { openai: "401 Unauthorized" });
      expect(card.dimensions.reliability.level).toBe("poor");
      expect(card.dimensions.reliability.evidence[0].fact).toMatch(/401 Unauthorized/);
    });

    it("marks a custom endpoint's provenance as user-added", () => {
      const custom: ProviderConfig = { ...provider, id: "custom-1", is_custom: true, base_url: "https://my-llm.example" };
      const card = scoreProviderModel(custom, "local-model", emptyUsage, {});
      expect(card.dimensions.provenance.evidence[0].fact).toMatch(/Custom OpenAI-compatible endpoint/);
    });
  });

  describe("scoreConnector", () => {
    function account(overrides: Partial<ConnectorAccount> = {}): ConnectorAccount {
      return {
        id: "acct-1",
        provider: "slack",
        label: "Team Slack",
        scopes: ["channels:read"],
        credential_ref: "connector:slack:acct-1",
        identity: "bot",
        created_at: 1_700_000_000_000,
        last_verified_at: 1_700_000_000_000,
        last_error: null,
        connection: null,
        ...overrides,
      };
    }

    it("marks a keychain-backed connector good on security", () => {
      const card = scoreConnector(account());
      expect(card.dimensions.security.level).toBe("good");
      expect(card.dimensions.security.evidence[0].fact).toMatch(/keychain/);
    });

    it("marks GitHub's credential-less connector good via gh CLI auth", () => {
      const card = scoreConnector(account({ provider: "github", credential_ref: null }));
      expect(card.dimensions.security.level).toBe("good");
      expect(card.dimensions.security.evidence[0].fact).toMatch(/gh` CLI/);
    });

    it("surfaces a last_error as poor reliability", () => {
      const card = scoreConnector(account({ last_error: "401 invalid token" }));
      expect(card.dimensions.reliability.level).toBe("poor");
      expect(card.dimensions.reliability.evidence[0].fact).toMatch(/401 invalid token/);
    });

    it("always treats connectors as cloud-bound on privacy, citing declared scopes", () => {
      const card = scoreConnector(account({ scopes: ["repo", "read:org"] }));
      expect(card.dimensions.privacy.level).toBe("fair");
      expect(card.dimensions.privacy.evidence.some((e) => e.fact.includes("repo, read:org"))).toBe(true);
    });
  });

  describe("scoreMcpServer", () => {
    function server(overrides: Partial<McpServerInfo> = {}): McpServerInfo {
      return {
        id: "srv-1",
        label: "Local tools",
        transport: { type: "stdio", command: "node", args: ["server.js"], env: {} },
        enabled: true,
        toolAllowlist: null,
        timeoutSecs: null,
        status: "connected",
        error: null,
        tools: [],
        instructions: null,
        hasHttpToken: false,
        hasOauth: false,
        ...overrides,
      };
    }

    it("marks stdio transport fair on security (arbitrary local process) unless scoped by an allowlist", () => {
      expect(server().transport.type).toBe("stdio");
      expect(scoreMcpServer(server()).dimensions.security.level).toBe("fair");
      expect(scoreMcpServer(server({ toolAllowlist: ["read_file"] })).dimensions.security.level).toBe("good");
    });

    it("marks http transport fair on privacy (remote endpoint)", () => {
      const httpServer = server({ transport: { type: "http", url: "https://mcp.example.com" } });
      expect(scoreMcpServer(httpServer).dimensions.privacy.level).toBe("fair");
    });

    it("surfaces a connection error as poor reliability", () => {
      const card = scoreMcpServer(server({ status: "error", error: "ECONNREFUSED" }));
      expect(card.dimensions.reliability.level).toBe("poor");
      expect(card.dimensions.reliability.evidence[0].fact).toMatch(/ECONNREFUSED/);
    });
  });

  describe("scoreSkill", () => {
    function skill(overrides: Partial<NativeSkillDescriptor> = {}): NativeSkillDescriptor {
      return {
        name: "deploy-checklist",
        description: "Runs the deploy checklist",
        command: "deploy-checklist",
        version: "1.0.0",
        instructions: "…",
        sha256: "abc123",
        file_count: 2,
        total_bytes: 4096,
        enabled: true,
        eligibility: { eligible: true, current_os: "darwin", unsupported_os: false, missing_bins: [], missing_env: [] },
        supported_os: ["darwin"],
        requirements: { bins: [], env: [] },
        source: { kind: "workspace", path: "/repo/.claude/skills/deploy-checklist" },
        permissions: [],
        git_repository: null,
        allowed_tools: [],
        resource_files: [],
        ...overrides,
      };
    }

    it("marks an unrestricted skill fair on security, a scoped one good", () => {
      expect(scoreSkill(skill()).dimensions.security.level).toBe("fair");
      expect(scoreSkill(skill({ allowed_tools: ["run_shell"] })).dimensions.security.level).toBe("good");
    });

    it("marks an ineligible skill poor on reliability, citing the missing requirement", () => {
      const card = scoreSkill(
        skill({ eligibility: { eligible: false, current_os: "darwin", unsupported_os: false, missing_bins: ["ffmpeg"], missing_env: [] } }),
      );
      expect(card.dimensions.reliability.level).toBe("poor");
      expect(card.dimensions.reliability.evidence[0].fact).toMatch(/ffmpeg/);
    });

    it("marks a signed-package skill good on provenance, citing the package id", () => {
      const card = scoreSkill(skill({ source: { kind: "signed_package", package_id: "acme.deploy" } }));
      expect(card.dimensions.provenance.evidence[0].fact).toMatch(/acme.deploy/);
    });
  });

  describe("scoreWorkflow", () => {
    function workflow(overrides: Partial<WorkflowDefinition> = {}): WorkflowDefinition {
      return {
        schema_version: 1,
        workflow_id: "wf-1",
        workflow_version: 1,
        name: "Nightly digest",
        inputs: {},
        secrets: {},
        nodes: [
          {
            node_id: "output-1",
            kind: { kind: "output" },
            inputs: {},
            secret_ids: [],
            permission_policy: { permission_ids: [], approval_node_id: null },
            retry: { maximum_attempts: 1, initial_backoff_ms: 0, maximum_backoff_ms: 0, retry_on: [] },
            timeout_ms: 1000,
            estimate: { model_calls: 0, input_tokens: 0, output_tokens: 0, cost_microunits: 0 },
            idempotency: { kind: "none" },
            replay: "safe",
            guard: null,
          },
        ],
        outputs: {},
        budgets: {
          maximum_node_executions: 10,
          maximum_model_calls: 0,
          maximum_input_tokens: 1000,
          maximum_output_tokens: 1000,
          maximum_cost_microunits: 0,
          maximum_wall_time_ms: 60_000,
        },
        maximum_concurrency: 1,
        triggers: [{ kind: "manual" }],
        ...overrides,
      };
    }

    function history(status: WorkflowRunHistory["status"], overrides: Partial<WorkflowRunHistory> = {}): WorkflowRunHistory {
      return {
        schema_version: 1,
        run_id: `run-${Math.random()}`,
        workflow_id: "wf-1",
        definition_sha256: "sha",
        status,
        started_unix_ms: 1,
        finished_unix_ms: 2,
        trigger: { kind: "manual" },
        input_snapshot: {},
        secret_reference_snapshot: {},
        nodes: {},
        outputs: {},
        usage: {},
        events: [],
        ...overrides,
      };
    }

    it("has unknown quality/reliability with no run history", () => {
      const card = scoreWorkflow(workflow(), [], []);
      expect(card.dimensions.quality.level).toBe("unknown");
      expect(card.dimensions.reliability.level).toBe("unknown");
    });

    it("derives quality/reliability from the real recorded success rate", () => {
      const histories = [history("succeeded"), history("succeeded"), history("failed")];
      const card = scoreWorkflow(workflow(), histories, []);
      expect(card.dimensions.quality.evidence[0].fact).toMatch(/2 of 3/);
      expect(card.dimensions.reliability.level).toBe("fair");
    });

    it("marks a workflow with zero declared model calls good on cost", () => {
      expect(scoreWorkflow(workflow(), [], []).dimensions.cost.level).toBe("good");
    });

    it("flags shell/mcp/pull_request nodes as reaching outside the device on privacy", () => {
      const wf = workflow({
        nodes: [
          ...workflow().nodes,
          {
            node_id: "shell-1",
            kind: { kind: "shell", shell_profile: "posix-sh" },
            inputs: {},
            secret_ids: [],
            permission_policy: { permission_ids: [], approval_node_id: null },
            retry: { maximum_attempts: 1, initial_backoff_ms: 0, maximum_backoff_ms: 0, retry_on: [] },
            timeout_ms: 1000,
            estimate: { model_calls: 0, input_tokens: 0, output_tokens: 0, cost_microunits: 0 },
            idempotency: { kind: "none" },
            replay: "requires_approval",
            guard: null,
          },
        ],
      });
      expect(scoreWorkflow(wf, [], []).dimensions.privacy.level).toBe("fair");
    });
  });

  describe("scorePlugin", () => {
    function plugin(overrides: Partial<PluginRuntimeDescriptor> = {}): PluginRuntimeDescriptor {
      return {
        package_id: "acme.toolkit",
        version: "1.2.0",
        name: "Acme Toolkit",
        description: "…",
        kind: "collection",
        health: "healthy",
        enabled: true,
        signed: true,
        bundle_sha256: "sha",
        pinned_version: null,
        rollback_target: null,
        rollback_healthy: false,
        permissions: [],
        components: [],
        issues: [],
        ...overrides,
      };
    }

    function catalogEntry(overrides: Partial<PackageCatalogEntry> = {}): PackageCatalogEntry {
      return {
        manifest: {
          schema_version: 1,
          package_id: "acme.toolkit",
          version: "1.2.0",
          kind: "collection",
          display_name: "Acme Toolkit",
          description: "…",
          content: [],
          permissions: [],
          mcp_requirements: [],
          provenance: { publisher: "Acme Corp", source: {}, source_revision: "abc123", build_reproducible: true },
        },
        bundle_sha256: "sha",
        trust: { signed: true, trust_root_id: "root-1", key_id: "key-1", registry_snapshot_sha256: null, revocation: {} },
        available: true,
        validation_error: null,
        ...overrides,
      };
    }

    it("marks an unsigned plugin poor on security", () => {
      const card = scorePlugin(plugin({ signed: false }), []);
      expect(card.dimensions.security.level).toBe("poor");
    });

    it("marks a signed plugin with a high-risk permission fair, not good", () => {
      const card = scorePlugin(
        plugin({ permissions: [{ permission_id: "p1", kind: "execute_process", scope: "*", reason: "run build tools" }] }),
        [],
      );
      expect(card.dimensions.security.level).toBe("fair");
      expect(card.dimensions.security.evidence.some((e) => e.fact.includes("execute_process"))).toBe(true);
    });

    it("marks a network permission fair on privacy", () => {
      const card = scorePlugin(
        plugin({ permissions: [{ permission_id: "p1", kind: "network", scope: "*", reason: "call an API" }] }),
        [],
      );
      expect(card.dimensions.privacy.level).toBe("fair");
    });

    it("maps health to reliability directly", () => {
      expect(scorePlugin(plugin({ health: "blocked" }), []).dimensions.reliability.level).toBe("poor");
      expect(scorePlugin(plugin({ health: "needs_setup" }), []).dimensions.reliability.level).toBe("fair");
      expect(scorePlugin(plugin({ health: "healthy" }), []).dimensions.reliability.level).toBe("good");
    });

    it("cites the matching catalog entry's publisher and trust evidence for provenance", () => {
      const card = scorePlugin(plugin(), [catalogEntry()]);
      expect(card.dimensions.provenance.level).toBe("good");
      expect(card.dimensions.provenance.evidence[0].fact).toMatch(/Acme Corp/);
    });

    it("has unknown provenance when no catalog entry matches (e.g. a portable import)", () => {
      const card = scorePlugin(plugin(), []);
      expect(card.dimensions.provenance.level).toBe("unknown");
    });
  });
});
