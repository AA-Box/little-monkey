/**
 * The turn → run link the per-process resource ledger attributes through.
 *
 * `agent_processes.run_id` is a foreign key into `runs` and a turn's process row
 * is minted before its run exists, so the link can only be written after the
 * fact (`processTable.ts`'s `linkProcessRun`). Without it the ledger has no row
 * to charge a turn's measured egress to and buckets the bytes as unattributed.
 *
 * Separate from `processTable.test.ts` because this suite has to hand
 * `agentLoop.ts` a real durable run, which means replacing `beginDurableRun` —
 * and that file asserts the projection of a turn that has no recorder at all.
 */
import { beforeEach, describe, expect, it, vi } from "vitest";

const invokeMock = vi.fn();
vi.mock("@tauri-apps/api/core", () => ({
  invoke: (...args: unknown[]) => invokeMock(...args),
  isTauri: () => true,
}));
vi.mock("@tauri-apps/api/event", () => ({ listen: vi.fn(async () => () => {}) }));

let scriptedRounds: Array<{ content?: string; toolCalls?: unknown[] }> = [];

vi.mock("./llamaClient", async (importOriginal) => {
  const actual = await importOriginal<typeof import("./llamaClient")>();
  return {
    ...actual,
    streamChat: async function* streamChat() {
      const round = scriptedRounds.shift() ?? { content: "done" };
      yield { type: "delta", content: round.content ?? "done" };
      yield { type: "done" };
    },
  };
});

/** Every run id `agentLoop.ts` began, in order. A real `beginDurableRun` would
 * need the whole model-target inventory plumbed in to return anything but
 * `null`; this suite is about what happens to the process row once a run id
 * exists, not about how the run itself is submitted. */
let begunRunIds: string[] = [];
vi.mock("./durableRun", async (importOriginal) => {
  const actual = await importOriginal<typeof import("./durableRun")>();
  return {
    ...actual,
    beginDurableRun: async (options: { runId: string }) => {
      begunRunIds.push(options.runId);
      return new actual.DurableRunRecorder(options.runId);
    },
  };
});

import { runAgentTurn } from "./agentLoop";
import { admitProcess } from "./processTable";
import { useModelStore } from "../store/modelStore";
import { usePermissionStore } from "../store/permissionStore";
import { useSessionStore, type ChatSession } from "../store/sessionStore";
import { useWorkspaceStore } from "../store/workspaceStore";

const NO_DAEMON = {
  installed: false,
  serviceRunning: false,
  heartbeatFresh: false,
  killSwitch: false,
};

interface AdmitCall {
  kind: string;
  externalId: string;
  runId?: string | null;
}

interface LinkCall {
  processId: string;
  runId: string;
}

let admits: AdmitCall[] = [];
let links: LinkCall[] = [];

function installBackend(options: { failLink?: boolean } = {}): void {
  invokeMock.mockImplementation(async (command: string, payload?: Record<string, unknown>) => {
    if (command === "daemon_desktop_status") return NO_DAEMON;
    if (command === "process_admit") {
      const args = payload?.args as AdmitCall;
      admits.push(args);
      return { processId: `p-${args.kind}-${admits.length}`, ...args };
    }
    if (command === "process_link_run") {
      if (options.failLink) throw new Error("foreign key mismatch");
      links.push(payload as unknown as LinkCall);
      return undefined;
    }
    return undefined;
  });
}

function seedSession(sessionId: string): void {
  const session: ChatSession = {
    id: sessionId,
    title: "Run link",
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
    activeSessionId: sessionId,
    messages: [],
    runningTurns: {},
  });
}

async function drainPersistence(): Promise<void> {
  await new Promise((resolve) => setTimeout(resolve, 425));
}

describe("a chat turn's process row carries its run id", () => {
  beforeEach(() => {
    invokeMock.mockReset();
    admits = [];
    links = [];
    begunRunIds = [];
    scriptedRounds = [];
    installBackend();
    useWorkspaceStore.setState({
      roots: [{ id: "r0", path: "/tmp/run-link-workspace", label: "ws", is_primary: true }],
    });
    usePermissionStore.setState({ mode: "manual" });
    // The model has to be in the inventory too, not just selected:
    // `snapshotForResolvedTarget` resolves the target back to its inventory
    // record and no snapshot means no durable run at all.
    useModelStore.setState({
      activeProvider: "ollama",
      activeOllamaModel: "test-model",
      ollamaReachable: true,
      ollamaModels: [
        {
          name: "test-model",
          size_bytes: 1,
          is_cloud: false,
          tool_calling: true,
          vision: false,
          modified_at: "2026-01-01T00:00:00Z",
        },
      ],
    });
  });

  it("links the row to the run once the run row exists", async () => {
    const sessionId = "run-link-ok";
    seedSession(sessionId);
    scriptedRounds = [{ content: "all done" }];

    await runAgentTurn(sessionId, "Say hello.");

    expect(admits).toHaveLength(1);
    // Impossible at admission time: the run row does not exist yet, and the
    // column is a foreign key into `runs`.
    expect(admits[0].runId).toBeUndefined();
    expect(begunRunIds).toEqual([admits[0].externalId]);
    expect(links).toEqual([{ processId: "p-chat_turn-1", runId: admits[0].externalId }]);

    await drainPersistence();
  });

  it("leaves exactly one row claiming the run", async () => {
    const sessionId = "run-link-single";
    seedSession(sessionId);
    scriptedRounds = [{ content: "all done" }];

    await runAgentTurn(sessionId, "Say hello.");

    // The ledger charges a run only when exactly one row claims it: zero rows
    // and several rows both fall back to the unattributed bucket, so a second
    // link (or a second admitted row for the same run) would silently
    // *un*-attribute the bytes rather than double-count them.
    expect(links).toHaveLength(1);
    expect(new Set(links.map((call) => call.runId)).size).toBe(links.length);
    expect(admits.filter((call) => call.runId)).toEqual([]);

    await drainPersistence();
  });

  it("completes the turn even when the link cannot be written", async () => {
    const sessionId = "run-link-broken";
    seedSession(sessionId);
    installBackend({ failLink: true });
    scriptedRounds = [{ content: "still worked" }];

    await expect(runAgentTurn(sessionId, "Carry on.")).resolves.toBeUndefined();

    const messages =
      useSessionStore.getState().sessions.find((entry) => entry.id === sessionId)?.messages ?? [];
    expect(messages.some((message) => message.role === "assistant")).toBe(true);
    expect(links).toEqual([]);

    await drainPersistence();
  });

  it("admits a kind whose model has no run without one", async () => {
    // `agent_processes.run_id` is NULL by design for `subagent` and the m4
    // workflow kinds — the ledger reports their token counts as structurally
    // unavailable, which is the honest answer, not a gap to paper over with a
    // borrowed run id.
    await expect(admitProcess({ kind: "subagent", externalId: "task-1" })).resolves.toBe(
      "p-subagent-1",
    );
    expect(admits[0].runId).toBeUndefined();
    expect(links).toEqual([]);
  });
});
