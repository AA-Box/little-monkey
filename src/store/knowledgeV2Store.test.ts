import { beforeEach, describe, expect, it, vi } from "vitest";

const invokeMock = vi.fn();
vi.mock("@tauri-apps/api/core", () => ({
  invoke: (...args: unknown[]) => invokeMock(...args),
  isTauri: () => false,
}));
vi.mock("@tauri-apps/api/event", () => ({ listen: vi.fn() }));

import {
  DEFAULT_HYBRID_CONFIG,
  connectorUsesAccountReference,
  useKnowledgeV2Store,
  type KnowledgeConnector,
  type KnowledgeSourceV2,
} from "./knowledgeV2Store";

const source: KnowledgeSourceV2 = {
  id: "source-1",
  stack_id: "stack-1",
  label: "Docs",
  enabled: true,
  connector: { kind: "local_folder", path: "/docs" },
  cursor: null,
  checkpoint: null,
  last_refresh_at_ms: null,
  last_error: null,
  objects: [],
  retries: [],
};

beforeEach(() => {
  invokeMock.mockReset();
  useKnowledgeV2Store.setState({
    sources: [],
    progress: {},
    reports: {},
    errors: {},
    loading: false,
  });
});

describe("knowledgeV2Store", () => {
  it("loads a stack-filtered source catalog", async () => {
    invokeMock.mockResolvedValueOnce([source]);
    await useKnowledgeV2Store.getState().refreshSources("stack-1");
    expect(invokeMock).toHaveBeenCalledWith("knowledge_v2_list_sources", { stackId: "stack-1" });
    expect(useKnowledgeV2Store.getState().sources).toEqual([source]);
  });

  it("adds WebDAV credentials only through the command boundary", async () => {
    const webdav = {
      ...source,
      connector: {
        kind: "web_dav" as const,
        url: "https://dav.example/docs.md",
        username: "user",
        credential_ref: "source-secret",
        allow_loopback: false,
      },
    };
    invokeMock.mockResolvedValueOnce(webdav);
    await useKnowledgeV2Store
      .getState()
      .addSource("stack-1", "DAV", webdav.connector, "secret-password");
    expect(invokeMock).toHaveBeenCalledWith("knowledge_v2_add_source", {
      stackId: "stack-1",
      label: "DAV",
      connector: webdav.connector,
      webdavPassword: "secret-password",
    });
    expect(JSON.stringify(useKnowledgeV2Store.getState().sources)).not.toContain("secret-password");
  });

  it("records a refresh report and then reloads connector checkpoints", async () => {
    const report = {
      stack_id: "stack-1",
      generation_id: "generation-1",
      parent_generation_id: null,
      source_count: 1,
      object_count: 1,
      changed_objects: 1,
      unchanged_objects: 0,
      deleted_objects: 0,
      embedded_chunks: 3,
      reused_chunks: 0,
      warnings: [],
      duration_ms: 12,
    };
    invokeMock.mockResolvedValueOnce(report).mockResolvedValueOnce([source]).mockResolvedValueOnce([]);
    await useKnowledgeV2Store.getState().refreshStack("stack-1");
    expect(useKnowledgeV2Store.getState().reports["stack-1"]).toEqual(report);
    expect(invokeMock).toHaveBeenNthCalledWith(2, "knowledge_v2_list_sources", { stackId: null });
    expect(invokeMock).toHaveBeenNthCalledWith(3, "stacks_list");
  });

  it("passes hybrid tuning, exclusions, reranking, and token budget exactly", async () => {
    invokeMock.mockResolvedValueOnce({ search: { hits: [], diagnostics: {} } });
    await useKnowledgeV2Store
      .getState()
      .query("stack-1", "what changed", DEFAULT_HYBRID_CONFIG, ["source-2"], true, 2048, "query-1");
    expect(invokeMock).toHaveBeenCalledWith("knowledge_v2_query", {
      request: {
        stack_id: "stack-1",
        query_id: "query-1",
        query: "what changed",
        config: DEFAULT_HYBRID_CONFIG,
        excluded_source_ids: ["source-2"],
        rerank: true,
        token_budget: 2048,
      },
    });
  });

  it("cancels the exact inspector query id", async () => {
    invokeMock.mockResolvedValueOnce(true);
    await expect(useKnowledgeV2Store.getState().cancelQuery("query-1")).resolves.toBe(true);
    expect(invokeMock).toHaveBeenCalledWith("knowledge_v2_cancel_query", { queryId: "query-1" });
  });

  it("persists chunking through Knowledge 2.0 and refreshes the shared stack definition", async () => {
    const stack = { id: "stack-1", chunk_chars: 900, chunk_overlap: 120 };
    invokeMock.mockResolvedValueOnce(stack).mockResolvedValueOnce([stack]);
    await expect(useKnowledgeV2Store.getState().updateChunking("stack-1", 900, 120)).resolves.toEqual(stack);
    expect(invokeMock).toHaveBeenNthCalledWith(1, "knowledge_v2_update_chunking", {
      stackId: "stack-1",
      chunkChars: 900,
      chunkOverlap: 120,
    });
    expect(invokeMock).toHaveBeenNthCalledWith(2, "stacks_list");
  });

  describe("External Knowledge Sync connector shapes", () => {
    const cases: Array<{ label: string; connector: KnowledgeConnector; usesAccountReference: boolean }> = [
      {
        label: "GitHub repo",
        connector: {
          kind: "git_hub_repo",
          owner: "acme",
          repo: "widgets",
          git_ref: "main",
          path_prefix: "docs",
          connector_account_id: "account-1",
        },
        usesAccountReference: true,
      },
      {
        label: "S3 bucket",
        connector: {
          kind: "s3_bucket",
          endpoint: "https://s3.example.com",
          bucket: "reports",
          prefix: "2024/",
          region: "us-east-1",
          connector_account_id: "account-2",
        },
        usesAccountReference: true,
      },
      {
        label: "watched folder",
        connector: { kind: "watched_folder", path: "/Users/me/Notes", debounce_ms: 2_000 },
        usesAccountReference: false,
      },
      {
        label: "Notion pages",
        connector: { kind: "notion_pages", connector_account_id: "account-3", root_id: "root-page-id" },
        usesAccountReference: true,
      },
      {
        label: "Slack channels",
        connector: {
          kind: "slack_channels",
          connector_account_id: "account-4",
          channel_ids: ["C0123456789", "C9876543210"],
        },
        usesAccountReference: true,
      },
      {
        label: "Jira project",
        connector: { kind: "jira_project", connector_account_id: "account-5", project_key: "PROJ" },
        usesAccountReference: true,
      },
    ];

    it.each(cases)("adds a $label source with no pasted credential in the payload", async ({ connector }) => {
      const created = { ...source, connector };
      invokeMock.mockResolvedValueOnce(created);
      await useKnowledgeV2Store.getState().addSource("stack-1", "New source", connector);
      expect(invokeMock).toHaveBeenCalledWith("knowledge_v2_add_source", {
        stackId: "stack-1",
        label: "New source",
        connector,
        webdavPassword: null,
      });
      expect(useKnowledgeV2Store.getState().sources).toContainEqual(created);
    });

    it.each(cases)("reports connectorUsesAccountReference($usesAccountReference) for $label", ({ connector, usesAccountReference }) => {
      expect(connectorUsesAccountReference(connector.kind)).toBe(usesAccountReference);
    });

    it("updates a source that references a connector account without ever sending a secret", async () => {
      const connector: KnowledgeConnector = {
        kind: "jira_project",
        connector_account_id: "account-5",
        project_key: "PROJ",
      };
      const updated = { ...source, connector, enabled: false };
      invokeMock.mockResolvedValueOnce(updated);
      await useKnowledgeV2Store.getState().updateSource("source-1", "Jira", false, connector);
      expect(invokeMock).toHaveBeenCalledWith("knowledge_v2_update_source", {
        sourceId: "source-1",
        label: "Jira",
        enabled: false,
        connector,
        webdavPassword: null,
      });
      expect(JSON.stringify(invokeMock.mock.calls[0])).not.toContain("secret");
    });
  });
});
