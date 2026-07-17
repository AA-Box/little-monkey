import { beforeEach, describe, expect, it, vi } from "vitest";

// `universalSearchStore` calls the real `tool_grep` / `stacks_query` Tauri
// commands (via `stackStore.query`) and reads several other stores'
// in-memory state directly — same hoisted-mock shape used by
// `sessionStore.split.test.ts` / `knowledgeV2Store.test.ts`.
const invokeMock = vi.fn(async (..._args: unknown[]): Promise<unknown> => null);
vi.mock("@tauri-apps/api/core", () => ({
  invoke: (...args: unknown[]) => invokeMock(...args),
  isTauri: () => true,
}));
vi.mock("@tauri-apps/api/event", () => ({ listen: vi.fn(async () => () => {}) }));

import { useUniversalSearchStore } from "./universalSearchStore";
import { useSessionStore, type ChatSession } from "./sessionStore";
import { useRunStore } from "./runStore";
import { useWorkspaceStore, type WorkspaceRootInfo } from "./workspaceStore";
import { useBrowserWorkbenchStore } from "./browserWorkbenchStore";
import { useMcpStore, type McpServerInfo } from "./mcpStore";
import { useStackStore, type KnowledgeStack } from "./stackStore";

function makeSession(id: string, overrides: Partial<ChatSession> = {}): ChatSession {
  const now = Date.now();
  return {
    id,
    title: `session ${id}`,
    messages: [],
    createdAt: now,
    updatedAt: now,
    pinned: false,
    unread: false,
    archived: false,
    groupId: null,
    modelTarget: null,
    comparisonBranch: null,
    workspacePath: null,
    personaId: null,
    attachedStackIds: [],
    docChatMode: false,
    subagentRuns: {},
    ...overrides,
  };
}

function root(overrides: Partial<WorkspaceRootInfo> = {}): WorkspaceRootInfo {
  return { id: "root-1", path: "/Users/dev/project", label: "project", is_primary: true, ...overrides };
}

function stack(overrides: Partial<KnowledgeStack> = {}): KnowledgeStack {
  return {
    id: "stack-1",
    name: "Docs",
    sources: [],
    embedding: { backend: "llama", model_id_or_tag: "m", dim: 8, query_prefix: "", doc_prefix: "" },
    chunk_chars: 1000,
    chunk_overlap: 100,
    indexed_at: 1,
    chunk_count: 10,
    ...overrides,
  };
}

function mcpServer(overrides: Partial<McpServerInfo> = {}): McpServerInfo {
  return {
    id: "srv-1",
    label: "GitHub",
    transport: { type: "stdio", command: "gh-mcp", args: [], env: {} },
    enabled: true,
    toolAllowlist: null,
    timeoutSecs: null,
    status: "connected",
    error: null,
    tools: [{ name: "search_issues", description: "Search issues", inputSchema: {} }],
    instructions: null,
    hasHttpToken: false,
    ...overrides,
  };
}

beforeEach(() => {
  invokeMock.mockReset();
  invokeMock.mockImplementation(async (...args: unknown[]) => {
    const command = args[0] as string;
    if (command === "tool_grep") return [];
    if (command === "stacks_query") return [];
    return null;
  });
  useUniversalSearchStore.setState({ hits: [], excludedCount: 0, loading: false, error: null });
  useSessionStore.setState({ sessions: [] });
  useRunStore.setState({ runs: [] });
  useWorkspaceStore.setState({ roots: [] });
  useBrowserWorkbenchStore.setState({ pendingBySession: {} });
  useMcpStore.setState({ servers: [] });
  useStackStore.setState({ stacks: [] });
});

describe("useUniversalSearchStore", () => {
  it("clears results for a blank query without calling the backend", async () => {
    useUniversalSearchStore.setState({ hits: [{ id: "stale" } as never], excludedCount: 2 });
    await useUniversalSearchStore.getState().run("   ", { includeArchived: false });
    expect(useUniversalSearchStore.getState().hits).toEqual([]);
    expect(useUniversalSearchStore.getState().excludedCount).toBe(0);
    expect(invokeMock).not.toHaveBeenCalled();
  });

  it("combines a session hit with a live workspace-file grep hit", async () => {
    useWorkspaceStore.setState({ roots: [root()] });
    useSessionStore.setState({ sessions: [makeSession("s1", { title: "Investigate widget bug" })] });
    invokeMock.mockImplementation(async (...callArgs: unknown[]) => {
      const [command, args] = callArgs as [string, unknown];
      if (command === "tool_grep") {
        expect((args as { pattern: string }).pattern).toBe("widget");
        return [{ file: "src/widget.ts", line: 3, text: "// widget rendering" }];
      }
      if (command === "stacks_query") return [];
      return null;
    });

    await useUniversalSearchStore.getState().run("widget", { includeArchived: false });

    const { hits, loading } = useUniversalSearchStore.getState();
    expect(loading).toBe(false);
    const kinds = hits.map((hit) => hit.sourceKind).sort();
    expect(kinds).toEqual(["session", "workspace_file"]);
  });

  it("drops a session outside every attached workspace root and reports it as excluded", async () => {
    useWorkspaceStore.setState({ roots: [root({ path: "/Users/dev/project" })] });
    useSessionStore.setState({
      sessions: [makeSession("s1", { title: "widget work", workspacePath: "/Users/dev/other-project" })],
    });

    await useUniversalSearchStore.getState().run("widget", { includeArchived: false });

    const { hits, excludedCount } = useUniversalSearchStore.getState();
    expect(hits).toEqual([]);
    expect(excludedCount).toBe(1);
  });

  it("queries only locally-indexed knowledge stacks", async () => {
    useStackStore.setState({ stacks: [stack({ id: "indexed", indexed_at: 1 }), stack({ id: "not-indexed", indexed_at: null })] });
    invokeMock.mockImplementation(async (...callArgs: unknown[]) => {
      const [command, args] = callArgs as [string, unknown];
      if (command === "tool_grep") return [];
      if (command === "stacks_query") {
        expect((args as { stackIds: string[] }).stackIds).toEqual(["indexed"]);
        return [{ stack_id: "indexed", stack_name: "Docs", source_path: "readme.md", score: 0.9, text: "needle passage", heading: null }];
      }
      return null;
    });

    await useUniversalSearchStore.getState().run("needle", { includeArchived: false });

    const { hits } = useUniversalSearchStore.getState();
    expect(hits.some((hit) => hit.sourceKind === "knowledge")).toBe(true);
  });

  it("excludes a matching but disconnected MCP server from connected_app results", async () => {
    useMcpStore.setState({ servers: [mcpServer({ status: "disconnected" })] });
    await useUniversalSearchStore.getState().run("github", { includeArchived: false });
    const { hits, excludedCount } = useUniversalSearchStore.getState();
    expect(hits).toEqual([]);
    expect(excludedCount).toBe(1);
  });

  it("includes a matching connected MCP server", async () => {
    useMcpStore.setState({ servers: [mcpServer({ status: "connected", enabled: true })] });
    await useUniversalSearchStore.getState().run("github", { includeArchived: false });
    const { hits } = useUniversalSearchStore.getState();
    expect(hits.some((hit) => hit.sourceKind === "connected_app")).toBe(true);
  });

  it("only keeps results from the latest call when queries race", async () => {
    const pending: { resolve: (() => void) | null } = { resolve: null };
    invokeMock.mockImplementation(async (...callArgs: unknown[]) => {
      const command = callArgs[0] as string;
      if (command === "tool_grep") {
        await new Promise<void>((resolve) => {
          pending.resolve = resolve;
        });
        return [{ file: "stale.ts", line: 1, text: "first" }];
      }
      return [];
    });
    useWorkspaceStore.setState({ roots: [root()] });

    const firstRun = useUniversalSearchStore.getState().run("first", { includeArchived: false });
    invokeMock.mockImplementation(async (...callArgs: unknown[]) => {
      const command = callArgs[0] as string;
      if (command === "tool_grep") return [{ file: "fresh.ts", line: 1, text: "second" }];
      return [];
    });
    await useUniversalSearchStore.getState().run("second", { includeArchived: false });
    pending.resolve?.();
    await firstRun;

    const { hits } = useUniversalSearchStore.getState();
    expect(hits.every((hit) => hit.title !== "stale.ts")).toBe(true);
  });
});
