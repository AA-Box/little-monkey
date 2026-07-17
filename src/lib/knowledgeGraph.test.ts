import { describe, expect, it, vi } from "vitest";

import {
  answerRelationQuery,
  buildKnowledgeGraph,
  chunkIntoSourceBatches,
  emptyGraph,
  findNodesMatching,
  parseExtractionResponse,
  parseRelationQuery,
  shortestPath,
  slugify,
  toMermaidFlowchart,
  type GraphCallResult,
  type KnowledgeGraph,
  type SourceBatch,
} from "./knowledgeGraph";

function batch(overrides: Partial<SourceBatch> = {}): SourceBatch {
  return {
    id: "batch-1",
    sourceType: "knowledge_stack",
    sourceId: "stack-1",
    sourceLabel: "Docs",
    spans: [{ marker: "S1", quote: "Alice owns the auth module.", locator: "docs/auth.md#0-30" }],
    ...overrides,
  };
}

describe("slugify", () => {
  it("normalizes case and whitespace onto the same id", () => {
    expect(slugify("Alice")).toBe("alice");
    expect(slugify("  Alice  ")).toBe("alice");
    expect(slugify("Alice Smith")).toBe("alice-smith");
  });

  it("falls back to a stable placeholder for a label with no alnum chars", () => {
    expect(slugify("!!!")).toBe("entity");
  });
});

describe("chunkIntoSourceBatches", () => {
  it("splits spans across multiple batches once the char budget is exceeded", () => {
    const rawSpans = [
      { quote: "a".repeat(30), locator: "loc-1" },
      { quote: "b".repeat(30), locator: "loc-2" },
      { quote: "c".repeat(30), locator: "loc-3" },
    ];
    const batches = chunkIntoSourceBatches("knowledge_stack", "stack-1", "Docs", rawSpans, "docs", 50, 10);
    expect(batches.length).toBeGreaterThan(1);
    for (const b of batches) {
      expect(b.sourceId).toBe("stack-1");
      for (const span of b.spans) expect(span.marker).toMatch(/^S\d+$/);
    }
  });

  it("splits once the per-batch span count cap is hit even under the char budget", () => {
    const rawSpans = Array.from({ length: 5 }, (_, i) => ({ quote: `quote ${i}`, locator: `loc-${i}` }));
    const batches = chunkIntoSourceBatches("knowledge_stack", "stack-1", "Docs", rawSpans, "docs", 10_000, 2);
    expect(batches).toHaveLength(3);
    expect(batches[0].spans).toHaveLength(2);
    expect(batches[2].spans).toHaveLength(1);
  });

  it("drops empty/whitespace-only quotes instead of emitting a blank span", () => {
    const batches = chunkIntoSourceBatches(
      "knowledge_stack",
      "stack-1",
      "Docs",
      [{ quote: "   ", locator: "loc-1" }, { quote: "real content", locator: "loc-2" }],
      "docs",
    );
    expect(batches).toHaveLength(1);
    expect(batches[0].spans).toHaveLength(1);
    expect(batches[0].spans[0].quote).toBe("real content");
  });
});

describe("parseExtractionResponse", () => {
  it("parses a well-formed extraction reply", () => {
    const content = JSON.stringify({
      nodes: [
        { id: "a", label: "Alice", kind: "person" },
        { id: "m", label: "auth.ts", kind: "file" },
      ],
      edges: [{ source: "a", target: "m", relation: "owns", evidence: ["S1"] }],
    });
    const parsed = parseExtractionResponse(content);
    expect(parsed).not.toBeNull();
    expect(parsed!.nodes).toHaveLength(2);
    expect(parsed!.edges).toHaveLength(1);
  });

  it("recovers JSON embedded in surrounding prose", () => {
    const content = `Sure, here you go:\n${JSON.stringify({
      nodes: [{ id: "a", label: "Alice", kind: "person" }],
      edges: [],
    })}\nHope that helps!`;
    const parsed = parseExtractionResponse(content);
    expect(parsed).not.toBeNull();
    expect(parsed!.nodes[0].label).toBe("Alice");
  });

  it("drops an edge citing an unknown node id but keeps the rest", () => {
    const content = JSON.stringify({
      nodes: [{ id: "a", label: "Alice", kind: "person" }],
      edges: [
        { source: "a", target: "ghost", relation: "owns", evidence: ["S1"] },
      ],
    });
    const parsed = parseExtractionResponse(content);
    expect(parsed!.edges).toHaveLength(0);
  });

  it("drops an edge with no evidence markers at all", () => {
    const content = JSON.stringify({
      nodes: [
        { id: "a", label: "Alice", kind: "person" },
        { id: "b", label: "Bob", kind: "person" },
      ],
      edges: [{ source: "a", target: "b", relation: "relates_to", evidence: [] }],
    });
    const parsed = parseExtractionResponse(content);
    expect(parsed!.edges).toHaveLength(0);
  });

  it("defaults an out-of-enum node kind to 'other' rather than dropping the node", () => {
    const content = JSON.stringify({
      nodes: [{ id: "a", label: "Alice", kind: "wizard" }],
      edges: [],
    });
    const parsed = parseExtractionResponse(content);
    expect(parsed!.nodes[0].kind).toBe("other");
  });

  it("returns null for a reply with no parseable JSON object", () => {
    expect(parseExtractionResponse("no json here at all")).toBeNull();
  });

  it("returns null when the top-level shape is missing nodes/edges arrays", () => {
    expect(parseExtractionResponse(JSON.stringify({ foo: "bar" }))).toBeNull();
  });
});

describe("buildKnowledgeGraph", () => {
  it("merges nodes across batches by normalized label and accumulates evidence", async () => {
    const batches: SourceBatch[] = [
      batch({
        id: "b1",
        sourceLabel: "Docs A",
        spans: [{ marker: "S1", quote: "Alice owns auth.ts.", locator: "a.md#0-20" }],
      }),
      batch({
        id: "b2",
        sourceLabel: "Docs B",
        spans: [{ marker: "S1", quote: "alice also maintains auth.ts.", locator: "b.md#0-30" }],
      }),
    ];

    const callModel = vi.fn(async (): Promise<GraphCallResult> => ({
      content: JSON.stringify({
        nodes: [
          { id: "a", label: "Alice", kind: "person" },
          { id: "f", label: "auth.ts", kind: "file" },
        ],
        edges: [{ source: "a", target: "f", relation: "owns", evidence: ["S1"] }],
      }),
      streamError: null,
    }));

    const result = await buildKnowledgeGraph(batches, callModel);
    expect(callModel).toHaveBeenCalledTimes(2);
    expect(result.batchErrors).toHaveLength(0);
    expect(result.nodes).toHaveLength(2);
    const alice = result.nodes.find((n) => n.id === "alice")!;
    expect(alice.mentions).toBe(2);
    expect(result.edges).toHaveLength(1);
    expect(result.edges[0].evidence).toHaveLength(2);
  });

  it("records a batch error on stream failure without aborting the rest of the build", async () => {
    const batches: SourceBatch[] = [
      batch({ id: "b1", sourceLabel: "Failing source" }),
      batch({ id: "b2", sourceLabel: "Good source" }),
    ];
    const callModel = vi
      .fn<() => Promise<GraphCallResult>>()
      .mockResolvedValueOnce({ content: "", streamError: "timed out" })
      .mockResolvedValueOnce({
        content: JSON.stringify({
          nodes: [{ id: "a", label: "Alice", kind: "person" }],
          edges: [],
        }),
        streamError: null,
      });

    const result = await buildKnowledgeGraph(batches, callModel);
    expect(result.batchErrors).toHaveLength(1);
    expect(result.batchErrors[0]).toContain("Failing source");
    expect(result.nodes).toHaveLength(1);
  });

  it("records a batch error when the model reply is not valid extraction JSON", async () => {
    const callModel = vi.fn(async (): Promise<GraphCallResult> => ({ content: "not json", streamError: null }));
    const result = await buildKnowledgeGraph([batch()], callModel);
    expect(result.batchErrors).toHaveLength(1);
    expect(result.nodes).toHaveLength(0);
  });

  it("never sends a raw quote to the model without it being addressable by the marker cited back", async () => {
    let sentMessages: unknown;
    const callModel = vi.fn(async (messages: unknown): Promise<GraphCallResult> => {
      sentMessages = messages;
      return {
        content: JSON.stringify({
          nodes: [{ id: "a", label: "Alice", kind: "person" }],
          edges: [],
        }),
        streamError: null,
      };
    });
    await buildKnowledgeGraph([batch()], callModel);
    const userMessage = (sentMessages as Array<{ role: string; content: string }>).find((m) => m.role === "user");
    expect(userMessage?.content).toContain("[S1]");
    expect(userMessage?.content).toContain("Alice owns the auth module.");
  });
});

describe("findNodesMatching", () => {
  const graph: KnowledgeGraph = {
    nodes: [
      { id: "alice", label: "Alice", kind: "person", mentions: 3 },
      { id: "auth-ts", label: "auth.ts", kind: "file", mentions: 1 },
    ],
    edges: [],
  };

  it("matches exact (case-insensitive) labels first", () => {
    const matches = findNodesMatching(graph, "alice");
    expect(matches[0]?.id).toBe("alice");
  });

  it("matches on substring containment", () => {
    const matches = findNodesMatching(graph, "auth");
    expect(matches[0]?.id).toBe("auth-ts");
  });

  it("returns an empty array when nothing matches", () => {
    expect(findNodesMatching(graph, "nonexistent")).toEqual([]);
  });
});

describe("shortestPath", () => {
  const graph: KnowledgeGraph = {
    nodes: [
      { id: "a", label: "A", kind: "other", mentions: 1 },
      { id: "b", label: "B", kind: "other", mentions: 1 },
      { id: "c", label: "C", kind: "other", mentions: 1 },
    ],
    edges: [
      { id: "e1", source: "a", target: "b", relation: "relates_to", evidence: [] },
      { id: "e2", source: "b", target: "c", relation: "depends_on", evidence: [] },
    ],
  };

  it("finds a multi-hop path", () => {
    const path = shortestPath(graph, "a", "c");
    expect(path).toHaveLength(2);
  });

  it("treats edges as undirected — finds a path walking a 'depends_on' edge backwards", () => {
    const path = shortestPath(graph, "c", "a");
    expect(path).toHaveLength(2);
  });

  it("returns an empty array for the same node", () => {
    expect(shortestPath(graph, "a", "a")).toEqual([]);
  });

  it("returns null when either id is unknown", () => {
    expect(shortestPath(graph, "a", "ghost")).toBeNull();
  });

  it("returns null when no path connects two known but disconnected nodes", () => {
    const disconnected: KnowledgeGraph = {
      nodes: [...graph.nodes, { id: "d", label: "D", kind: "other", mentions: 1 }],
      edges: graph.edges,
    };
    expect(shortestPath(disconnected, "a", "d")).toBeNull();
  });
});

describe("parseRelationQuery", () => {
  it("parses 'how is X related to Y'", () => {
    expect(parseRelationQuery("How is Alice related to auth.ts?")).toEqual({ fromText: "Alice", toText: "auth.ts" });
  });

  it("parses 'relationship between X and Y'", () => {
    expect(parseRelationQuery("What is the relationship between Alice and Bob")).toEqual({
      fromText: "Alice",
      toText: "Bob",
    });
  });

  it("falls back to a plain 'X and Y' split", () => {
    expect(parseRelationQuery("Alice and Bob")).toEqual({ fromText: "Alice", toText: "Bob" });
  });

  it("returns null when no two entities can be found", () => {
    expect(parseRelationQuery("what is this")).toBeNull();
  });
});

describe("answerRelationQuery", () => {
  const graph: KnowledgeGraph = {
    nodes: [
      { id: "alice", label: "Alice", kind: "person", mentions: 2 },
      { id: "auth-ts", label: "auth.ts", kind: "file", mentions: 1 },
    ],
    edges: [
      {
        id: "e1",
        source: "alice",
        target: "auth-ts",
        relation: "owns",
        evidence: [
          { sourceType: "knowledge_stack", sourceId: "s1", sourceLabel: "Docs", quote: "Alice owns auth.ts.", locator: "a.md#0-20" },
        ],
      },
    ],
  };

  it("returns the path and evidence for a resolvable query", () => {
    const result = answerRelationQuery(graph, "How is Alice related to auth.ts?");
    expect(result.error).toBeNull();
    expect(result.path).toHaveLength(1);
    expect(result.evidence).toHaveLength(1);
    expect(result.explanation).toContain("Alice");
    expect(result.explanation).toContain("auth.ts");
  });

  it("reports an unparseable query without throwing", () => {
    const result = answerRelationQuery(graph, "hello");
    expect(result.error).toContain("Could not find two entity names");
    expect(result.evidence).toEqual([]);
  });

  it("reports an unknown entity by name", () => {
    const result = answerRelationQuery(graph, "How is Zeus related to Alice?");
    expect(result.error).toContain("Zeus");
  });

  it("reports no-path found for two known but disconnected entities", () => {
    const disconnected: KnowledgeGraph = {
      nodes: [...graph.nodes, { id: "carol", label: "Carol", kind: "person", mentions: 1 }],
      edges: graph.edges,
    };
    const result = answerRelationQuery(disconnected, "How is Alice related to Carol?");
    expect(result.error).toContain("No connection was found");
    expect(result.evidence).toEqual([]);
  });

  it("handles an empty graph gracefully", () => {
    const result = answerRelationQuery(emptyGraph(), "How is Alice related to Bob?");
    expect(result.error).toContain("Alice");
  });
});

describe("toMermaidFlowchart", () => {
  it("renders a flowchart with nodes and a labeled edge", () => {
    const graph: KnowledgeGraph = {
      nodes: [
        { id: "alice", label: "Alice", kind: "person", mentions: 1 },
        { id: "auth-ts", label: "auth.ts", kind: "file", mentions: 1 },
      ],
      edges: [{ id: "e1", source: "alice", target: "auth-ts", relation: "owns", evidence: [] }],
    };
    const diagram = toMermaidFlowchart(graph);
    expect(diagram).toContain("flowchart LR");
    expect(diagram).toContain("Alice (person)");
    expect(diagram).toContain("-->|owns|");
  });

  it("marks highlighted edges/nodes with the highlight class", () => {
    const graph: KnowledgeGraph = {
      nodes: [
        { id: "alice", label: "Alice", kind: "person", mentions: 1 },
        { id: "auth-ts", label: "auth.ts", kind: "file", mentions: 1 },
      ],
      edges: [{ id: "e1", source: "alice", target: "auth-ts", relation: "owns", evidence: [] }],
    };
    const diagram = toMermaidFlowchart(graph, ["e1"]);
    expect(diagram).toContain("classDef highlight");
    expect(diagram).toContain("class ");
  });

  it("renders a placeholder node for an empty graph", () => {
    const diagram = toMermaidFlowchart(emptyGraph());
    expect(diagram).toContain("No entities extracted yet");
  });
});
