import { beforeEach, describe, expect, it, vi } from "vitest";

// None of the sibling stores this store reads from (`sessionStore.ts`,
// `stackStore.ts`, `knowledgeV2Store.ts`) exist under vitest's node
// environment without their Tauri IPC/event calls stubbed — same pattern as
// `sessionStore.split.test.ts`.
const invokeMock = vi.fn(async (..._args: unknown[]): Promise<unknown> => null);
vi.mock("@tauri-apps/api/core", () => ({ invoke: (...args: unknown[]) => invokeMock(...args), isTauri: () => false }));
vi.mock("@tauri-apps/api/event", () => ({ listen: vi.fn(async () => () => {}) }));

// `generateBriefAsset` itself is exercised end-to-end in `briefStudio.test.ts`
// — mocked here so these tests pin the STORE's own responsibility: picking
// the right source-building path, surfacing validation errors before ever
// calling it, and reflecting its result/error into state. The real
// `build*Source` helpers are kept (via `importOriginal`) so the assertions
// below can check the exact source shape the store assembled.
const generateBriefAssetMock = vi.fn();
vi.mock("../lib/briefStudio", async (importOriginal) => ({
  ...(await importOriginal<typeof import("../lib/briefStudio")>()),
  generateBriefAsset: (...args: unknown[]) => generateBriefAssetMock(...args),
}));

import { useBriefStudioStore } from "./briefStudioStore";
import { useSessionStore, type ChatSession } from "./sessionStore";
import { useStackStore, type KnowledgeStack } from "./stackStore";
import { useKnowledgeV2Store, type KnowledgeInspectorResponse } from "./knowledgeV2Store";
import type { GeneratedBriefAsset } from "../lib/briefStudio";

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

function makeStack(id: string, overrides: Partial<KnowledgeStack> = {}): KnowledgeStack {
  return {
    id,
    name: `Stack ${id}`,
    sources: [],
    embedding: { backend: "llama", model_id_or_tag: "nomic-embed-text-v1.5", dim: 768, query_prefix: "", doc_prefix: "" },
    chunk_chars: 1200,
    chunk_overlap: 200,
    indexed_at: Date.now(),
    chunk_count: 10,
    ...overrides,
  };
}

function fixtureAsset(overrides: Partial<GeneratedBriefAsset> = {}): GeneratedBriefAsset {
  return {
    assetType: "brief",
    sourceLabel: "Doc",
    generatedAtMs: Date.now(),
    targetKind: "local",
    ranLocally: true,
    supported: true,
    unsupportedReason: null,
    content: "Generated content.",
    citations: [],
    unverifiedCitationCount: 0,
    ...overrides,
  };
}

function inspectorResponse(hits: KnowledgeInspectorResponse["search"]["hits"]): KnowledgeInspectorResponse {
  return {
    query_id: "q1",
    normalized_query: "topic",
    excluded_source_ids: [],
    token_budget: 4000,
    estimated_context_tokens: 100,
    final_context: "",
    search: {
      hits,
      diagnostics: {
        diagnostic_version: 1,
        generation_id: "gen-1",
        index_digest: "digest",
        query_sha256: "sha",
        embedding_fingerprint: "fp",
        config: {
          lexical_candidates: 50,
          vector_candidates: 50,
          final_results: 8,
          rrf_k: 60,
          lexical_weight_micros: 1_000_000,
          vector_weight_micros: 1_000_000,
          rerank_candidates: 20,
        },
        reranker_id: null,
        candidates: [],
        result_chunk_ids: [],
        trace_sha256: "trace",
      },
    },
  };
}

function hit(text: string, uri = "doc.md"): KnowledgeInspectorResponse["search"]["hits"][number] {
  return {
    rank: 1,
    chunk: {
      chunk_id: "chunk-1",
      source_id: "source-1",
      object_id: "object-1",
      text,
      heading_path: [],
      location: { kind: "text" },
      citation: {
        citation_id: "cit-1",
        source_id: "source-1",
        object_id: "object-1",
        canonical_uri: uri,
        location: { kind: "text" },
        block_char_start: 0,
        block_char_end: text.length,
      },
      content_type: "text/plain",
      confidence_micros: null,
      low_confidence: false,
    },
    fused_score_units: 1,
    rerank_score_micros: null,
  };
}

beforeEach(() => {
  generateBriefAssetMock.mockReset();
  useBriefStudioStore.setState({
    sourceKind: "pasted",
    assetType: "brief",
    requireLocalOnly: false,
    pastedLabel: "",
    pastedText: "",
    selectedSessionId: null,
    selectedStackId: null,
    focusQuery: "",
    generating: false,
    error: null,
    result: null,
    history: [],
  });
  useSessionStore.setState({ sessions: [], activeSessionId: null } as never);
  useStackStore.setState({ stacks: [] } as never);
});

describe("useBriefStudioStore.generate — pasted source", () => {
  it("errors before calling generateBriefAsset when no text was pasted", async () => {
    useBriefStudioStore.setState({ sourceKind: "pasted", pastedText: "   " });
    await useBriefStudioStore.getState().generate();
    expect(useBriefStudioStore.getState().error).toMatch(/paste some source text/i);
    expect(generateBriefAssetMock).not.toHaveBeenCalled();
  });

  it("builds a pasted source and stores the result", async () => {
    generateBriefAssetMock.mockResolvedValue(fixtureAsset());
    useBriefStudioStore.setState({ sourceKind: "pasted", pastedLabel: "My doc", pastedText: "Some content here." });
    await useBriefStudioStore.getState().generate();

    expect(generateBriefAssetMock).toHaveBeenCalledTimes(1);
    const [source, assetType, options] = generateBriefAssetMock.mock.calls[0];
    expect(source.kind).toBe("pasted");
    expect(source.label).toBe("My doc");
    expect(assetType).toBe("brief");
    expect(options.requireLocalOnly).toBe(false);

    const state = useBriefStudioStore.getState();
    expect(state.generating).toBe(false);
    expect(state.error).toBeNull();
    expect(state.result?.content).toBe("Generated content.");
    expect(state.history).toHaveLength(1);
  });
});

describe("useBriefStudioStore.generate — session source", () => {
  it("errors when no session is selected", async () => {
    useBriefStudioStore.setState({ sourceKind: "session", selectedSessionId: null });
    await useBriefStudioStore.getState().generate();
    expect(useBriefStudioStore.getState().error).toMatch(/pick a chat session/i);
    expect(generateBriefAssetMock).not.toHaveBeenCalled();
  });

  it("builds a session source from the selected session's messages", async () => {
    useSessionStore.setState({
      sessions: [
        makeSession("s1", {
          title: "Planning chat",
          messages: [
            { role: "user", content: "What's the plan?" },
            { role: "assistant", content: "Ship the MVP first." },
          ],
        }),
      ],
    } as never);
    useBriefStudioStore.setState({ sourceKind: "session", selectedSessionId: "s1" });
    generateBriefAssetMock.mockResolvedValue(fixtureAsset());

    await useBriefStudioStore.getState().generate();

    const [source] = generateBriefAssetMock.mock.calls[0];
    expect(source.kind).toBe("session");
    expect(source.label).toBe("Planning chat");
    expect(source.blocks).toHaveLength(2);
    expect(useBriefStudioStore.getState().error).toBeNull();
  });
});

describe("useBriefStudioStore.generate — knowledge stack source", () => {
  it("errors when no stack is selected", async () => {
    useBriefStudioStore.setState({ sourceKind: "knowledge_stack", selectedStackId: null, focusQuery: "topic" });
    await useBriefStudioStore.getState().generate();
    expect(useBriefStudioStore.getState().error).toMatch(/pick a knowledge stack/i);
    expect(generateBriefAssetMock).not.toHaveBeenCalled();
  });

  it("errors when no focus topic was entered", async () => {
    useStackStore.setState({ stacks: [makeStack("stack-1")] } as never);
    useBriefStudioStore.setState({ sourceKind: "knowledge_stack", selectedStackId: "stack-1", focusQuery: "  " });
    await useBriefStudioStore.getState().generate();
    expect(useBriefStudioStore.getState().error).toMatch(/focus topic/i);
    expect(generateBriefAssetMock).not.toHaveBeenCalled();
  });

  it("queries the knowledge stack and builds a source from the hits", async () => {
    useStackStore.setState({ stacks: [makeStack("stack-1", { name: "Handbook" })] } as never);
    const queryMock = vi.fn().mockResolvedValue(inspectorResponse([hit("Revenue grew 12% year over year.")]));
    useKnowledgeV2Store.setState({ query: queryMock } as never);
    useBriefStudioStore.setState({ sourceKind: "knowledge_stack", selectedStackId: "stack-1", focusQuery: "revenue" });
    generateBriefAssetMock.mockResolvedValue(fixtureAsset());

    await useBriefStudioStore.getState().generate();

    expect(queryMock).toHaveBeenCalledWith("stack-1", "revenue", expect.any(Object), [], false, expect.any(Number));
    const [source] = generateBriefAssetMock.mock.calls[0];
    expect(source.kind).toBe("knowledge_stack");
    expect(source.label).toBe("Handbook");
    expect(source.blocks[0].text).toContain("Revenue grew 12%");
    expect(useBriefStudioStore.getState().error).toBeNull();
  });

  it("errors when the query returns no hits, without calling generateBriefAsset", async () => {
    useStackStore.setState({ stacks: [makeStack("stack-1")] } as never);
    const queryMock = vi.fn().mockResolvedValue(inspectorResponse([]));
    useKnowledgeV2Store.setState({ query: queryMock } as never);
    useBriefStudioStore.setState({ sourceKind: "knowledge_stack", selectedStackId: "stack-1", focusQuery: "nothing" });

    await useBriefStudioStore.getState().generate();

    expect(useBriefStudioStore.getState().error).toMatch(/no matching material/i);
    expect(generateBriefAssetMock).not.toHaveBeenCalled();
  });
});

describe("useBriefStudioStore.generate — errors from generateBriefAsset", () => {
  it("surfaces a rejected generateBriefAsset call (e.g. the local-only policy error) as store error", async () => {
    useBriefStudioStore.setState({ sourceKind: "pasted", pastedText: "content" });
    generateBriefAssetMock.mockRejectedValue(new Error("Run fully local is on but a cloud provider is active."));

    await useBriefStudioStore.getState().generate();

    const state = useBriefStudioStore.getState();
    expect(state.generating).toBe(false);
    expect(state.result).toBeNull();
    expect(state.error).toMatch(/cloud provider/i);
  });
});
