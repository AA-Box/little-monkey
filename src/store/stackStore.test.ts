import { beforeEach, describe, expect, it, vi } from "vitest";

const invokeMock = vi.fn();
// See `modelStore.test.ts`'s comment on why the `listen` handler must be
// stashed via `vi.hoisted` rather than a plain outer-scope variable — a
// normal `let`/`var` closed over by a hoisted `vi.mock` factory is a
// *different* binding than the one this file's test bodies read later.
vi.mock("@tauri-apps/api/core", () => ({ invoke: (...args: unknown[]) => invokeMock(...args) }));
vi.mock("@tauri-apps/api/event", () => ({
  listen: () => Promise.resolve(() => {}),
}));

import { useStackStore, type KnowledgeStack } from "./stackStore";

function makeStack(overrides: Partial<KnowledgeStack> = {}): KnowledgeStack {
  return {
    id: "stack-1",
    name: "My Docs",
    sources: [],
    embedding: {
      backend: "llama",
      model_id_or_tag: "nomic-embed-text-v1.5",
      dim: 768,
      query_prefix: "search_query: ",
      doc_prefix: "search_document: ",
    },
    chunk_chars: 1600,
    chunk_overlap: 200,
    indexed_at: null,
    chunk_count: 0,
    ...overrides,
  };
}

beforeEach(() => {
  invokeMock.mockReset();
  useStackStore.setState({
    stacks: [],
    indexProgress: {},
    reindexError: {},
    embedStatus: "stopped",
    embedPort: 8091,
    embedModelPath: null,
    embedError: null,
  });
});

describe("stackStore.refresh", () => {
  it("populates stacks from stacks_list", async () => {
    const stack = makeStack();
    invokeMock.mockResolvedValueOnce([stack]);

    await useStackStore.getState().refresh();

    expect(invokeMock).toHaveBeenCalledWith("stacks_list");
    expect(useStackStore.getState().stacks).toEqual([stack]);
  });
});

describe("stackStore.create", () => {
  it("creates a stack then refreshes the list", async () => {
    const stack = makeStack();
    invokeMock.mockResolvedValueOnce(stack); // stacks_create
    invokeMock.mockResolvedValueOnce([stack]); // stacks_list (refresh)

    const created = await useStackStore.getState().create("My Docs", stack.embedding);

    expect(invokeMock).toHaveBeenNthCalledWith(1, "stacks_create", { name: "My Docs", embedding: stack.embedding });
    expect(created).toEqual(stack);
    expect(useStackStore.getState().stacks).toEqual([stack]);
  });
});

describe("stackStore.remove", () => {
  it("deletes the stack, clears its progress/error state, and refreshes", async () => {
    useStackStore.setState({
      indexProgress: { "stack-1": { stack_id: "stack-1", files_done: 1, files_total: 1, chunks: 3, phase: "done" } },
      reindexError: { "stack-1": "boom" },
    });
    invokeMock.mockResolvedValueOnce(undefined); // stacks_delete
    invokeMock.mockResolvedValueOnce([]); // stacks_list (refresh)

    await useStackStore.getState().remove("stack-1");

    expect(invokeMock).toHaveBeenNthCalledWith(1, "stacks_delete", { id: "stack-1" });
    expect(useStackStore.getState().indexProgress).toEqual({});
    expect(useStackStore.getState().reindexError).toEqual({});
    expect(useStackStore.getState().stacks).toEqual([]);
  });
});

describe("stackStore.reindex", () => {
  it("clears a prior error and refreshes on success", async () => {
    useStackStore.setState({ reindexError: { "stack-1": "old error" } });
    invokeMock.mockResolvedValueOnce(undefined); // stacks_reindex
    invokeMock.mockResolvedValueOnce([makeStack({ indexed_at: 123, chunk_count: 5 })]); // refresh

    await useStackStore.getState().reindex("stack-1");

    expect(invokeMock).toHaveBeenNthCalledWith(1, "stacks_reindex", { id: "stack-1" });
    expect(useStackStore.getState().reindexError["stack-1"]).toBeUndefined();
    expect(useStackStore.getState().stacks[0].chunk_count).toBe(5);
  });

  it("records the failure message and rethrows on error", async () => {
    invokeMock.mockRejectedValueOnce(new Error("embedding server unreachable"));

    await expect(useStackStore.getState().reindex("stack-1")).rejects.toThrow("embedding server unreachable");
    expect(useStackStore.getState().reindexError["stack-1"]).toBe("embedding server unreachable");
  });
});

describe("stackStore.cancelIndex", () => {
  it("invokes stacks_cancel_index with the stack id", async () => {
    invokeMock.mockResolvedValueOnce(undefined);
    await useStackStore.getState().cancelIndex("stack-1");
    expect(invokeMock).toHaveBeenCalledWith("stacks_cancel_index", { id: "stack-1" });
  });
});

describe("stackStore.query", () => {
  it("passes stackIds/query/k through to stacks_query and returns the results", async () => {
    const results = [
      { stack_id: "stack-1", stack_name: "My Docs", source_path: "a.md", score: 0.9, text: "hello", heading: null },
    ];
    invokeMock.mockResolvedValueOnce(results);

    const hits = await useStackStore.getState().query(["stack-1"], "hello world", 3);

    expect(invokeMock).toHaveBeenCalledWith("stacks_query", { stackIds: ["stack-1"], query: "hello world", k: 3 });
    expect(hits).toEqual(results);
  });
});

describe("stackStore embed server controls", () => {
  it("startEmbedServer sets status to starting immediately, then invokes embed_server_start", async () => {
    invokeMock.mockResolvedValueOnce(undefined);
    const promise = useStackStore.getState().startEmbedServer("/models/nomic.gguf");
    expect(useStackStore.getState().embedStatus).toBe("starting");
    await promise;
    expect(invokeMock).toHaveBeenCalledWith("embed_server_start", { modelPath: "/models/nomic.gguf" });
  });

  it("startEmbedServer records the error message and rethrows on failure", async () => {
    invokeMock.mockRejectedValueOnce(new Error("pooling mode not supported"));
    await expect(useStackStore.getState().startEmbedServer("/models/nomic.gguf")).rejects.toThrow(
      "pooling mode not supported",
    );
    expect(useStackStore.getState().embedError).toBe("pooling mode not supported");
  });

  it("stopEmbedServer invokes embed_server_stop and resets status to stopped", async () => {
    useStackStore.setState({ embedStatus: "ready" });
    invokeMock.mockResolvedValueOnce(undefined);
    await useStackStore.getState().stopEmbedServer();
    expect(invokeMock).toHaveBeenCalledWith("embed_server_stop");
    expect(useStackStore.getState().embedStatus).toBe("stopped");
  });

  it("refreshEmbedStatus syncs status/port/modelPath from embed_server_status", async () => {
    invokeMock.mockResolvedValueOnce({ status: "ready", port: 8091, model_path: "/models/nomic.gguf" });
    await useStackStore.getState().refreshEmbedStatus();
    expect(useStackStore.getState().embedStatus).toBe("ready");
    expect(useStackStore.getState().embedModelPath).toBe("/models/nomic.gguf");
  });
});
