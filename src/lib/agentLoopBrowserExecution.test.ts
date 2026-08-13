/**
 * The one place a conversational turn may still run in this process: a browser
 * or dev profile with no Tauri bridge at all.
 *
 * Nothing durable exists there to hand the turn to — no resident runner can be
 * installed, healthy or otherwise — so refusing would leave the dev profile with
 * no way to hold a conversation. This is deliberately NOT the packaged desktop
 * app, and the difference is a single fact about the environment rather than a
 * setting: `isTauri()`.
 */
import { beforeEach, describe, expect, it, vi } from "vitest";

const invokeMock = vi.fn();
vi.mock("@tauri-apps/api/core", () => ({
  invoke: (...args: unknown[]) => invokeMock(...args),
  isTauri: () => false,
}));
vi.mock("@tauri-apps/api/event", () => ({ listen: vi.fn(async () => () => {}) }));

const streamed: unknown[] = [];
vi.mock("./llamaClient", async (importOriginal) => ({
  ...(await importOriginal<typeof import("./llamaClient")>()),
  streamChat: async function* streamChat(...args: unknown[]) {
    streamed.push(args);
    yield { type: "delta", content: "answered in the browser" };
    yield { type: "done" };
  },
}));

const mocks = vi.hoisted(() => ({ submitDaemonDesktopTurn: vi.fn(), daemonStatus: vi.fn() }));
vi.mock("./daemonDesktopTurn", async (importOriginal) => ({
  ...(await importOriginal<typeof import("./daemonDesktopTurn")>()),
  submitDaemonDesktopTurn: mocks.submitDaemonDesktopTurn,
}));
vi.mock("./daemonClient", async (importOriginal) => ({
  ...(await importOriginal<typeof import("./daemonClient")>()),
  daemonStatus: mocks.daemonStatus,
}));

import { runAgentTurn } from "./agentLoop";
import { useModelStore } from "../store/modelStore";
import { usePermissionStore } from "../store/permissionStore";
import { useSessionStore, type ChatSession } from "../store/sessionStore";
import { useWorkspaceStore } from "../store/workspaceStore";

beforeEach(() => {
  streamed.length = 0;
  mocks.submitDaemonDesktopTurn.mockReset();
  mocks.daemonStatus.mockReset();
  invokeMock.mockReset();
  invokeMock.mockImplementation(async (command: string) => {
    if (command === "rules_list") return [];
    return undefined;
  });
  useWorkspaceStore.setState({ roots: [] });
  usePermissionStore.setState({ mode: "manual" });
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
    title: "Browser",
    messages: [],
    createdAt: 0,
    updatedAt: 0,
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
  };
  useSessionStore.setState({
    sessions: [session],
    activeSessionId: "s-1",
    messages: [],
    runningTurns: {},
  });
});

describe("a profile with no desktop bridge", () => {
  it("answers in this process, and never asks a runner that cannot exist", async () => {
    await runAgentTurn("s-1", "explain what this project does");

    expect(streamed).toHaveLength(1);
    expect(mocks.daemonStatus).not.toHaveBeenCalled();
    expect(mocks.submitDaemonDesktopTurn).not.toHaveBeenCalled();
    const messages = useSessionStore.getState().sessions[0].messages;
    expect(messages.map((message) => message.role)).toEqual(["user", "assistant"]);
    expect(messages[1].content).toContain("answered in the browser");

    // sessionStore's debounced persistence, drained so it cannot leak into a
    // later file sharing this worker.
    await new Promise((resolve) => setTimeout(resolve, 425));
  });
});
