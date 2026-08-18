import { beforeEach, describe, expect, it, vi } from "vitest";

const invokeMock = vi.fn();
vi.mock("@tauri-apps/api/core", () => ({ invoke: (...args: unknown[]) => invokeMock(...args), isTauri: () => false }));
vi.mock("@tauri-apps/api/event", () => ({ listen: () => Promise.resolve(() => {}) }));

import { useTrustScorecardsStore } from "./trustScorecardsStore";
import { useModelStore, type ModelInfo } from "./modelStore";
import { useConnectorsStore, type ConnectorAccount } from "./connectorsStore";
import { useMcpStore, type McpServerInfo } from "./mcpStore";
import { useEcosystemStore } from "./ecosystemStore";
import { useUsageHistoryStore } from "./usageHistoryStore";
import type { NativeSkillDescriptor } from "../lib/nativeSkillsClient";

function resetAllStores() {
  useModelStore.setState({
    curated: [],
    installed: [],
    ollamaModels: [],
    providers: [],
    providerModels: {},
    providerKeyError: {},
    active: null,
    llamaStatus: "stopped",
  } as Partial<ReturnType<typeof useModelStore.getState>>);
  useConnectorsStore.setState({ accounts: [], loading: false, error: null });
  useMcpStore.setState({ servers: [] });
  useEcosystemStore.setState({
    catalog: [],
    installed: [],
    plugins: [],
    workflows: [],
    histories: [],
  } as Partial<ReturnType<typeof useEcosystemStore.getState>>);
  useUsageHistoryStore.getState().clear();
  useTrustScorecardsStore.setState({ scorecards: [], loading: false, error: null, lastComputedAt: null });
}

beforeEach(() => {
  invokeMock.mockReset();
  resetAllStores();
});

const model: ModelInfo = {
  id: "m1",
  name: "Test Model",
  repo: "org/repo",
  file: "test.gguf",
  size_gb: 1,
  tool_calling: true,
  installed: true,
  path: "/models/test.gguf",
  is_external: false,
  kind: "chat",
};

const connector: ConnectorAccount = {
  id: "acct-1",
  provider: "slack",
  label: "Team Slack",
  scopes: [],
  credential_ref: "connector:slack:acct-1",
  identity: "bot",
  created_at: 1_700_000_000_000,
  last_verified_at: 1_700_000_000_000,
  last_error: null,
  connection: null,
};

const mcpServer: McpServerInfo = {
  id: "srv-1",
  label: "Local tools",
  transport: { type: "stdio", command: "node", args: [], env: {} },
  enabled: true,
  toolAllowlist: null,
  timeoutSecs: null,
  status: "connected",
  error: null,
  tools: [],
  instructions: null,
  hasHttpToken: false,
  hasOauth: false,
};

const skill: NativeSkillDescriptor = {
  name: "deploy-checklist",
  description: "…",
  command: "deploy-checklist",
  version: "1.0.0",
  instructions: "…",
  sha256: "abc",
  file_count: 1,
  total_bytes: 100,
  enabled: true,
  eligibility: { eligible: true, current_os: "darwin", unsupported_os: false, missing_bins: [], missing_env: [] },
  supported_os: ["darwin"],
  requirements: { bins: [], env: [] },
  source: { kind: "workspace", path: "/repo/.claude/skills/deploy-checklist" },
  permissions: [],
  git_repository: null,
  allowed_tools: [],
  resource_files: [],
};

describe("trustScorecardsStore.recompute", () => {
  it("calls native_skills_discover and produces a scorecard per entity across all stores", async () => {
    invokeMock.mockResolvedValueOnce([skill]);

    useModelStore.setState({ installed: [model] } as Partial<ReturnType<typeof useModelStore.getState>>);
    useConnectorsStore.setState({ accounts: [connector] });
    useMcpStore.setState({ servers: [mcpServer] });

    await useTrustScorecardsStore.getState().recompute();

    expect(invokeMock).toHaveBeenCalledWith("native_skills_discover");
    const { scorecards, error, lastComputedAt } = useTrustScorecardsStore.getState();
    expect(error).toBeNull();
    expect(lastComputedAt).not.toBeNull();

    const kinds = scorecards.map((c) => c.kind).sort();
    expect(kinds).toEqual(["connector", "mcp_server", "model", "skill"].sort());
    expect(scorecards.find((c) => c.id === `model:local:${model.id}`)).toBeDefined();
    expect(scorecards.find((c) => c.id === `connector:${connector.id}`)).toBeDefined();
    expect(scorecards.find((c) => c.id === `mcp_server:${mcpServer.id}`)).toBeDefined();
    expect(scorecards.find((c) => c.id === `skill:${skill.command}`)).toBeDefined();
  });

  it("still scores every other entity kind when native skill discovery fails", async () => {
    invokeMock.mockRejectedValueOnce(new Error("boom"));
    useConnectorsStore.setState({ accounts: [connector] });

    await useTrustScorecardsStore.getState().recompute();

    const { scorecards, error } = useTrustScorecardsStore.getState();
    expect(error).toBe("boom");
    expect(scorecards.find((c) => c.id === `connector:${connector.id}`)).toBeDefined();
    expect(scorecards.some((c) => c.kind === "skill")).toBe(false);
  });

  it("produces one scorecard per configured cloud-provider model", async () => {
    invokeMock.mockResolvedValueOnce([]);
    useModelStore.setState({
      providers: [{ id: "openai", label: "OpenAI", base_url: "https://api.openai.com", is_custom: false, has_key: true, is_extension: false }],
      providerModels: { openai: [{ id: "gpt-4o" }, { id: "gpt-4o-mini" }] },
    } as Partial<ReturnType<typeof useModelStore.getState>>);

    await useTrustScorecardsStore.getState().recompute();

    const modelCards = useTrustScorecardsStore.getState().scorecards.filter((c) => c.kind === "model");
    expect(modelCards.map((c) => c.id).sort()).toEqual(
      ["model:provider:openai:gpt-4o", "model:provider:openai:gpt-4o-mini"].sort(),
    );
  });
});
