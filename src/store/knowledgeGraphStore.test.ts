import { beforeEach, describe, expect, it, vi } from "vitest";

const invokeMock = vi.fn();
vi.mock("@tauri-apps/api/core", () => ({
  invoke: (...args: unknown[]) => invokeMock(...args),
  isTauri: () => false,
}));
vi.mock("@tauri-apps/api/event", () => ({ listen: vi.fn() }));

const resolveTargetMock = vi.fn();
vi.mock("../lib/agentLoop", () => ({
  resolveTarget: () => resolveTargetMock(),
}));

const attemptStreamMock = vi.fn();
vi.mock("../lib/turnEngine", () => ({
  attemptStream: (...args: unknown[]) => attemptStreamMock(...args),
}));

import { useStackStore, type KnowledgeStack } from "./stackStore";
import { useSessionStore } from "./sessionStore";
import { useKnowledgeGraphStore } from "./knowledgeGraphStore";

function fixtureStack(overrides: Partial<KnowledgeStack> = {}): KnowledgeStack {
  return {
    id: "stack-1",
    name: "Engineering Docs",
    sources: [],
    embedding: { backend: "llama", model_id_or_tag: "test", dim: 4, query_prefix: "", doc_prefix: "" },
    chunk_chars: 800,
    chunk_overlap: 100,
    indexed_at: 1,
    chunk_count: 1,
    ...overrides,
  };
}

function hitResponse(text: string, canonicalUri: string) {
  return {
    query_id: "q1",
    normalized_query: "overview",
    excluded_source_ids: [],
    token_budget: 4000,
    estimated_context_tokens: 100,
    final_context: "",
    search: {
      hits: [
        {
          rank: 1,
          chunk: {
            chunk_id: "c1",
            source_id: "src-1",
            object_id: "obj-1",
            text,
            heading_path: [],
            location: { kind: "text" },
            citation: {
              citation_id: "cit-1",
              source_id: "src-1",
              object_id: "obj-1",
              canonical_uri: canonicalUri,
              location: { kind: "text" },
              block_char_start: 0,
              block_char_end: text.length,
            },
            content_type: "text",
            confidence_micros: null,
            low_confidence: false,
          },
          fused_score_units: 1,
          rerank_score_micros: null,
        },
      ],
      diagnostics: {
        diagnostic_version: 1,
        generation_id: "g1",
        index_digest: "d1",
        query_sha256: "q1",
        embedding_fingerprint: "e1",
        config: {},
        reranker_id: null,
        candidates: [],
        result_chunk_ids: ["c1"],
        trace_sha256: "t1",
      },
    },
  };
}

function extractionReply(nodes: Array<{ id: string; label: string; kind: string }>, edges: Array<{ source: string; target: string; relation: string; evidence: string[] }>) {
  return { content: JSON.stringify({ nodes, edges }), streamError: null };
}

beforeEach(() => {
  invokeMock.mockReset();
  resolveTargetMock.mockReset();
  attemptStreamMock.mockReset();
  resolveTargetMock.mockResolvedValue({ kind: "local", baseUrl: "http://localhost:8090" });
  useStackStore.setState({ stacks: [] });
  useKnowledgeGraphStore.getState().reset();
});

describe("knowledgeGraphStore", () => {
  it("reports no source content when there are no stacks and no active-session inclusion", async () => {
    await useKnowledgeGraphStore.getState().build({});
    const state = useKnowledgeGraphStore.getState();
    expect(state.building).toBe(false);
    expect(state.buildError).toBe("No source content was found to build a graph from.");
    expect(state.nodes).toEqual([]);
  });

  it("builds a graph from a knowledge stack's query hits", async () => {
    useStackStore.setState({ stacks: [fixtureStack()] });
    invokeMock.mockResolvedValueOnce(hitResponse("Alice owns the auth module.", "docs/auth.md"));
    attemptStreamMock.mockResolvedValueOnce(
      extractionReply(
        [
          { id: "a", label: "Alice", kind: "person" },
          { id: "m", label: "auth module", kind: "term" },
        ],
        [{ source: "a", target: "m", relation: "owns", evidence: ["S1"] }],
      ),
    );

    await useKnowledgeGraphStore.getState().build({ stackIds: ["stack-1"] });

    const state = useKnowledgeGraphStore.getState();
    expect(state.buildError).toBeNull();
    expect(state.building).toBe(false);
    expect(state.nodes).toHaveLength(2);
    expect(state.edges).toHaveLength(1);
    expect(state.edges[0].evidence[0].sourceLabel).toBe("Engineering Docs");
    expect(state.batchCount).toBe(1);
    expect(state.lastBuiltAtMs).not.toBeNull();
    expect(resolveTargetMock).toHaveBeenCalled();
  });

  it("records a per-stack gather error without aborting other stacks", async () => {
    useStackStore.setState({ stacks: [fixtureStack({ id: "stack-1", name: "Broken" }), fixtureStack({ id: "stack-2", name: "Good" })] });
    invokeMock.mockRejectedValueOnce(new Error("query failed")).mockResolvedValueOnce(hitResponse("Bob wrote the spec.", "docs/spec.md"));
    attemptStreamMock.mockResolvedValueOnce(
      extractionReply([{ id: "b", label: "Bob", kind: "person" }], []),
    );

    await useKnowledgeGraphStore.getState().build({});

    const state = useKnowledgeGraphStore.getState();
    expect(state.batchErrors.some((e) => e.includes("Broken"))).toBe(true);
    expect(state.nodes.map((n) => n.label)).toContain("Bob");
    expect(state.buildError).toBeNull();
  });

  it("sets buildError when target resolution fails", async () => {
    useStackStore.setState({ stacks: [fixtureStack()] });
    invokeMock.mockResolvedValueOnce(hitResponse("Alice owns the auth module.", "docs/auth.md"));
    resolveTargetMock.mockRejectedValueOnce(new Error("No AI provider model selected"));

    await useKnowledgeGraphStore.getState().build({});

    const state = useKnowledgeGraphStore.getState();
    expect(state.buildError).toBe("No AI provider model selected");
    expect(state.building).toBe(false);
  });

  it("includes the active chat session's transcript when requested", async () => {
    useSessionStore.setState({
      activeSessionId: "session-1",
      sessions: [
        {
          id: "session-1",
          title: "Auth planning",
          messages: [
            { role: "user", content: "Should Alice own the auth module refactor?" },
            { role: "assistant", content: "Yes, Alice already owns auth.ts." },
          ],
          createdAt: 1,
          updatedAt: 1,
          pinned: false,
          unread: false,
          archived: false,
          groupId: null,
          modelTarget: null,
          comparisonBranch: null,
        } as never,
      ],
    });
    attemptStreamMock.mockResolvedValueOnce(
      extractionReply(
        [{ id: "a", label: "Alice", kind: "person" }],
        [],
      ),
    );

    await useKnowledgeGraphStore.getState().build({ includeActiveSession: true });

    const state = useKnowledgeGraphStore.getState();
    expect(state.buildError).toBeNull();
    expect(state.nodes.map((n) => n.label)).toContain("Alice");
    expect(attemptStreamMock).toHaveBeenCalledTimes(1);
  });

  it("queryRelation computes a path/evidence result against the currently built graph", async () => {
    useKnowledgeGraphStore.setState({
      nodes: [
        { id: "alice", label: "Alice", kind: "person", mentions: 1 },
        { id: "auth-ts", label: "auth.ts", kind: "file", mentions: 1 },
      ],
      edges: [
        {
          id: "e1",
          source: "alice",
          target: "auth-ts",
          relation: "owns",
          evidence: [{ sourceType: "knowledge_stack", sourceId: "s1", sourceLabel: "Docs", quote: "Alice owns auth.ts.", locator: "docs/auth.md#0-20" }],
        },
      ],
    });

    useKnowledgeGraphStore.getState().queryRelation("How is Alice related to auth.ts?");

    const state = useKnowledgeGraphStore.getState();
    expect(state.queryResult?.error).toBeNull();
    expect(state.queryResult?.path).toHaveLength(1);
    expect(state.queryResult?.evidence).toHaveLength(1);
  });

  it("clearQuery resets the query text and result", () => {
    useKnowledgeGraphStore.setState({ queryText: "foo", queryResult: { queryText: "foo" } as never });
    useKnowledgeGraphStore.getState().clearQuery();
    const state = useKnowledgeGraphStore.getState();
    expect(state.queryText).toBe("");
    expect(state.queryResult).toBeNull();
  });

  it("reset clears the graph and any build/query state", async () => {
    useKnowledgeGraphStore.setState({
      nodes: [{ id: "a", label: "A", kind: "other", mentions: 1 }],
      buildError: "oops",
      queryText: "x",
    });
    useKnowledgeGraphStore.getState().reset();
    const state = useKnowledgeGraphStore.getState();
    expect(state.nodes).toEqual([]);
    expect(state.buildError).toBeNull();
    expect(state.queryText).toBe("");
  });
});
