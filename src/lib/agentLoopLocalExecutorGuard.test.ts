/**
 * The second layer of the invariant.
 *
 * Routing already refuses to send a desktop turn anywhere but the resident
 * runner, so this configuration — a caller holding a `browser` route inside a
 * packaged desktop app — cannot happen today. It is what a future refactor that
 * skips `daemonDesktopRoute` would produce, and the point of the guard is that
 * such a refactor fails loudly here instead of quietly restoring the bypass.
 */
import { beforeEach, describe, expect, it, vi } from "vitest";

const invokeMock = vi.fn();
vi.mock("@tauri-apps/api/core", () => ({
  invoke: (...args: unknown[]) => invokeMock(...args),
  isTauri: () => true,
}));
vi.mock("@tauri-apps/api/event", () => ({ listen: vi.fn(async () => () => {}) }));

/** Every in-process model round trip. This must stay empty. */
const streamed: unknown[] = [];
vi.mock("./llamaClient", async (importOriginal) => ({
  ...(await importOriginal<typeof import("./llamaClient")>()),
  streamChat: async function* streamChat(...args: unknown[]) {
    streamed.push(args);
    yield { type: "delta", content: "answered in the webview" };
    yield { type: "done" };
  },
}));

// A routing regression, simulated exactly: the desktop resolver hands back the
// browser route it is no longer able to produce.
vi.mock("./daemonDesktopTurn", async (importOriginal) => ({
  ...(await importOriginal<typeof import("./daemonDesktopTurn")>()),
  daemonDesktopRoute: async () => "browser" as const,
}));

import { runAgentTurn } from "./agentLoop";
import { useModelStore } from "../store/modelStore";
import { usePermissionStore } from "../store/permissionStore";
import { useSessionStore, type ChatSession } from "../store/sessionStore";
import { useWorkspaceStore } from "../store/workspaceStore";

const WORKSPACE = "/workspace/project";

beforeEach(() => {
  streamed.length = 0;
  invokeMock.mockReset();
  invokeMock.mockImplementation(async (command: string) => {
    if (command === "process_admit") return { processId: "p-1" };
    if (command === "rules_list") return [];
    return undefined;
  });
  useWorkspaceStore.setState({
    roots: [{ id: "root-1", path: WORKSPACE, label: "project", is_primary: true }],
  });
  usePermissionStore.setState({ mode: "acceptEdits" });
  useModelStore.setState({
    installed: [],
    active: null,
    llamaStatus: "stopped",
    ollamaModels: [{
      name: "qwen2.5:7b",
      size_bytes: 1,
      is_cloud: false,
      tool_calling: true,
      vision: false,
      modified_at: "now",
    }],
    ollamaReachable: true,
    providers: [],
    providerModels: {},
    activeProvider: "ollama",
    activeOllamaModel: "qwen2.5:7b",
  });
  const session: ChatSession = {
    id: "s-1",
    title: "Guard",
    messages: [],
    createdAt: 0,
    updatedAt: 0,
    pinned: false,
    unread: false,
    archived: false,
    groupId: null,
    modelTarget: null,
    comparisonBranch: null,
    workspacePath: WORKSPACE,
    personaId: null,
    attachedStackIds: [],
    docChatMode: false,
    subagentRuns: {},
  };
  useSessionStore.setState({
    sessions: [session],
    activeSessionId: "s-1",
    messages: [],
    runningTurns: {},
  });
});

describe("the in-process loop defends the boundary itself", () => {
  it("refuses a browser route while the desktop bridge exists", async () => {
    await expect(runAgentTurn("s-1", "explain what this project does")).rejects.toThrow(
      /cannot be executed in the app process/i,
    );

    expect(streamed).toEqual([]);
    expect(useSessionStore.getState().sessions[0].messages).toEqual([]);
  });
});
