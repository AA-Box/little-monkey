import { beforeAll, beforeEach, describe, expect, it, vi } from "vitest";

const mocks = vi.hoisted(() => ({
  attemptStream: vi.fn(),
  invoke: vi.fn(),
}));

vi.mock("@tauri-apps/api/core", () => ({
  invoke: (...args: unknown[]) => mocks.invoke(...args),
  isTauri: () => false,
}));
vi.mock("@tauri-apps/api/event", () => ({ listen: vi.fn(async () => () => {}) }));
vi.mock("@tauri-apps/api/window", () => ({ getCurrentWindow: () => ({ label: "evidence-board-test" }) }));
vi.mock("../lib/turnEngine", () => ({
  attemptStream: (...args: unknown[]) => mocks.attemptStream(...args),
}));

import type { ModelTargetSnapshot } from "../lib/modelTargets";
import { useEvidenceBoardStore } from "./evidenceBoardStore";
import { useModelStore } from "./modelStore";
import { useSessionStore, type ChatSession } from "./sessionStore";

const TARGET: ModelTargetSnapshot = {
  kind: "provider",
  key: "provider:test:extractor",
  label: "Test",
  displayName: "extractor",
  providerId: "test",
  endpoint: "https://provider.test/v1",
  model: "extractor",
  credentialRefId: "keychain:com.littlemonkey.app:test",
  capabilities: {
    toolCalling: { state: "unknown", evidence: "test" },
    vision: { state: "unknown", evidence: "test" },
  },
  availability: { status: "available", evidence: "test" },
};

function session(overrides: Partial<ChatSession> = {}): ChatSession {
  return {
    id: "session-1",
    title: "Quarterly report review",
    messages: [
      { role: "user", content: "Summarize the report." },
      { role: "assistant", content: "Revenue grew 40% year over year. The launch date slipped twice." },
    ],
    createdAt: 1,
    updatedAt: 1,
    pinned: false,
    unread: false,
    archived: false,
    groupId: null,
    modelTarget: TARGET,
    comparisonBranch: null,
    crewRun: null,
    workspacePath: null,
    personaId: null,
    attachedStackIds: [],
    docChatMode: false,
    subagentRuns: {},
    messageTranslations: [],
    threadTranslations: [],
    displayTranslationLocale: null,
    ...overrides,
  };
}

function seedSession(overrides: Partial<ChatSession> = {}): void {
  const source = session(overrides);
  useSessionStore.setState({
    sessions: [source],
    activeSessionId: source.id,
    messages: source.messages,
    groups: [],
    runningTurns: {},
    runningSyntheses: {},
    runningCrews: {},
    runningVerifyLabel: {},
    persistError: null,
  });
}

beforeAll(() => {
  const values = new Map<string, string>();
  Object.defineProperty(globalThis, "localStorage", {
    configurable: true,
    value: {
      clear: () => values.clear(),
      getItem: (key: string) => values.get(key) ?? null,
      setItem: (key: string, value: string) => values.set(key, value),
      removeItem: (key: string) => values.delete(key),
    },
  });
});

const LOCAL_MODEL = {
  id: "local-1",
  name: "Local Test",
  repo: "test/repo",
  file: "local-1.gguf",
  size_gb: 1,
  tool_calling: true,
  installed: true,
  path: "/models/local-1.gguf",
  is_external: false,
  kind: "chat" as const,
};

function resetModelStore(overrides: Partial<{ active: typeof LOCAL_MODEL | null }> = {}): void {
  useModelStore.setState({
    installed: [LOCAL_MODEL],
    active: LOCAL_MODEL,
    llamaStatus: "ready",
    activeProvider: "local",
    activeOllamaModel: null,
    ollamaModels: [],
    ollamaReachable: false,
    providers: [],
    providerModels: {},
    activeProviderId: null,
    activeProviderModel: null,
    ...overrides,
  });
}

beforeEach(() => {
  localStorage.clear();
  mocks.attemptStream.mockReset();
  mocks.invoke.mockReset();
  mocks.invoke.mockImplementation(async (cmd: string) =>
    cmd === "llama_status" ? { status: "ready", port: 8080, model_path: LOCAL_MODEL.path } : undefined
  );
  useEvidenceBoardStore.setState({ boards: [], activeBoardId: null, extracting: false });
  resetModelStore();
  seedSession();
});

function mockClaimResponse(claims: Array<Record<string, unknown>>): void {
  mocks.attemptStream.mockImplementation(async () => ({
    content: JSON.stringify({ claims }),
    toolCalls: [],
    streamError: null,
    contentStarted: true,
  }));
}

describe("evidenceBoardStore", () => {
  it("creates a session board once and reuses it on a second open", () => {
    const first = useEvidenceBoardStore.getState().openSessionBoard("session-1", "Quarterly report review");
    const second = useEvidenceBoardStore.getState().openSessionBoard("session-1", "Quarterly report review");
    expect(first).toBe(second);
    expect(useEvidenceBoardStore.getState().boards).toHaveLength(1);
    expect(useEvidenceBoardStore.getState().activeBoardId).toBe(first);
  });

  it("extracts grounded claims from the session's assistant messages", async () => {
    mockClaimResponse([
      {
        claim: "Revenue grew 40% year over year",
        confidence: "high",
        supporting: ["Revenue grew 40% year over year."],
        conflicting: [],
        unresolvedQuestion: null,
      },
      {
        claim: "The launch happened on schedule",
        confidence: "medium",
        supporting: [],
        conflicting: ["The launch date slipped twice."],
      },
    ]);

    const boardId = useEvidenceBoardStore.getState().openSessionBoard("session-1", "Quarterly report review");
    await useEvidenceBoardStore.getState().runExtraction(boardId);

    const board = useEvidenceBoardStore.getState().boards.find((b) => b.id === boardId)!;
    expect(board.claims).toHaveLength(2);
    expect(board.claims[0]).toMatchObject({ confidence: "high", unresolved: false, owner: "", status: "open" });
    expect(board.claims[1].unresolved).toBe(true);
    expect(board.lastExtractionError).toBeNull();
    expect(board.sourceText).toContain("Revenue grew 40% year over year.");

    const [, history] = mocks.attemptStream.mock.calls[0];
    expect(history[1].content).toContain("Revenue grew 40% year over year.");
  });

  it("records an extraction failure on the board without throwing it away silently", async () => {
    mocks.attemptStream.mockImplementation(async () => ({
      content: "not json",
      toolCalls: [],
      streamError: null,
      contentStarted: true,
    }));
    const boardId = useEvidenceBoardStore.getState().openSessionBoard("session-1", "Quarterly report review");
    await expect(useEvidenceBoardStore.getState().runExtraction(boardId)).rejects.toThrow();
    const board = useEvidenceBoardStore.getState().boards.find((b) => b.id === boardId)!;
    expect(board.lastExtractionError).toMatch(/did not return any extractable claims/);
    expect(useEvidenceBoardStore.getState().extracting).toBe(false);
  });

  it("preserves owner/status edits across a re-extraction that finds the same claim text again", async () => {
    mockClaimResponse([{ claim: "Revenue grew 40% year over year", confidence: "high", supporting: ["Revenue grew 40% year over year."] }]);
    const boardId = useEvidenceBoardStore.getState().openSessionBoard("session-1", "Quarterly report review");
    await useEvidenceBoardStore.getState().runExtraction(boardId);
    const claimId = useEvidenceBoardStore.getState().boards[0].claims[0].id;

    useEvidenceBoardStore.getState().updateClaimOwner(boardId, claimId, "Alex");
    useEvidenceBoardStore.getState().updateClaimStatus(boardId, claimId, "confirmed");

    await useEvidenceBoardStore.getState().runExtraction(boardId);
    const board = useEvidenceBoardStore.getState().boards[0];
    expect(board.claims).toHaveLength(1);
    expect(board.claims[0]).toMatchObject({ id: claimId, owner: "Alex", status: "confirmed" });
  });

  it("creates a pasted board and extracts from its stored text without touching any session", async () => {
    mockClaimResponse([{ claim: "The vendor missed the deadline", confidence: "medium", supporting: ["The vendor missed the deadline twice."] }]);
    const boardId = useEvidenceBoardStore
      .getState()
      .createPastedBoard("Vendor incident report", "The vendor missed the deadline twice. No penalty clause was invoked.");
    await useEvidenceBoardStore.getState().runExtraction(boardId);
    const board = useEvidenceBoardStore.getState().boards.find((b) => b.id === boardId)!;
    expect(board.sourceKind).toBe("pasted");
    expect(board.claims).toHaveLength(1);
  });

  it("throws instead of running a second concurrent extraction on the same board", async () => {
    mocks.attemptStream.mockImplementation(() => new Promise(() => {}));
    const boardId = useEvidenceBoardStore.getState().openSessionBoard("session-1", "Quarterly report review");
    const first = useEvidenceBoardStore.getState().runExtraction(boardId);
    await vi.waitFor(() => expect(useEvidenceBoardStore.getState().extracting).toBe(true));
    await expect(useEvidenceBoardStore.getState().runExtraction(boardId)).rejects.toThrow("already running");
    void first;
  });

  it("deletes a board and clears activeBoardId if it was active", () => {
    const boardId = useEvidenceBoardStore.getState().openSessionBoard("session-1", "Quarterly report review");
    useEvidenceBoardStore.getState().deleteBoard(boardId);
    expect(useEvidenceBoardStore.getState().boards).toHaveLength(0);
    expect(useEvidenceBoardStore.getState().activeBoardId).toBeNull();
  });

  it("persists boards to localStorage and hydrates them back", () => {
    useEvidenceBoardStore.getState().createPastedBoard("Persisted board", "Some claim-bearing text here.");
    const raw = localStorage.getItem("little-monkey-evidence-boards-v1");
    expect(raw).toBeTruthy();
    const parsed = JSON.parse(raw!);
    expect(parsed.boards).toHaveLength(1);
    expect(parsed.boards[0].name).toBe("Persisted board");
  });

  it("throws a clear error when no model target is available", async () => {
    resetModelStore({ active: null });
    seedSession({ modelTarget: undefined as unknown as ChatSession["modelTarget"] });
    const boardId = useEvidenceBoardStore.getState().openSessionBoard("session-1", "Quarterly report review");
    await expect(useEvidenceBoardStore.getState().runExtraction(boardId)).rejects.toThrow("Select and connect a chat model");
  });
});
