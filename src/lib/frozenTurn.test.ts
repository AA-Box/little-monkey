/**
 * What re-entering a frozen turn decides, and what it refuses to decide.
 *
 * The restorability verdict itself is Rust's (`checkpoints.rs` owns it and tests
 * it against every blocker). What is tested here is the half that only exists on
 * this side: which image belongs to which process, what a refusal does with the
 * row and the transcript, and the ordering that keeps an image from outliving
 * the turn it describes.
 */
import { beforeEach, describe, expect, it, vi } from "vitest";

const invokeMock = vi.fn();
vi.mock("@tauri-apps/api/core", () => ({
  invoke: (...args: unknown[]) => invokeMock(...args),
  isTauri: () => true,
}));
vi.mock("@tauri-apps/api/event", () => ({ listen: vi.fn(async () => () => {}) }));

const runAgentTurnMock = vi.fn<(...args: unknown[]) => Promise<void>>(async () => {});
vi.mock("./agentLoop", () => ({
  RESUME_NOTE_PREFIX: "[Resume]",
  resolveTarget: async () => ({ kind: "ollama", baseUrl: "", model: "llama-3.1-8b" }),
  runAgentTurn: (...args: unknown[]) => runAgentTurnMock(...args),
}));
vi.mock("./turnEngine", () => ({
  describeUsageTarget: () => "Ollama · llama-3.1-8b",
}));

const exitProcessMock = vi.fn<(id: string, status: string, reason?: string | null) => Promise<void>>(
  async () => {},
);
vi.mock("./processTable", () => ({
  exitProcess: (id: string, status: string, reason?: string | null) =>
    exitProcessMock(id, status, reason),
}));

import { resumeFrozenTurn } from "./frozenTurn";
import type { ProcessRecord } from "./processTable";
import { useSessionStore } from "../store/sessionStore";

const FROZEN = {
  id: "cp-1",
  sessionId: "session-1",
  frozenProcessId: "proc-frozen",
};

function record(): ProcessRecord {
  return { processId: "proc-frozen", kind: "chat_turn", externalId: "turn-1" } as ProcessRecord;
}

/** `checkpoint_list` returns every checkpoint; only one is an image. */
function respond(report: unknown, checkpoints: unknown[] = [{ id: "cp-0", sessionId: "session-1", frozenProcessId: null }, FROZEN]) {
  invokeMock.mockImplementation(async (command: string) => {
    if (command === "checkpoint_list") return checkpoints;
    if (command === "checkpoint_restorability") return report;
    return undefined;
  });
}

beforeEach(() => {
  invokeMock.mockReset();
  runAgentTurnMock.mockReset();
  exitProcessMock.mockReset();
  useSessionStore.setState({
    sessions: [{ id: "session-1", messages: [] }],
  } as never);
});

describe("resumeFrozenTurn", () => {
  it("re-enters the frozen turn's own session with no new user message", async () => {
    respond({
      restorability: { state: "resumable", processId: "proc-frozen" },
      determinismCaveats: ["Model sampling is not replayed."],
      blockerExplanations: [],
    });

    expect(await resumeFrozenTurn(record())).toBe("resumed");

    const [sessionId, userText, , , , , , , resume] = runAgentTurnMock.mock.calls[0];
    expect(sessionId).toBe("session-1");
    expect(userText).toBe("");
    expect(resume).toMatchObject({ resumedFromCheckpointId: "cp-1" });
  });

  /**
   * Ordering, not tidiness: an image cleared *after* the loop starts describes a
   * turn that is already running, and the next restart would offer to resume it
   * a second time.
   */
  it("clears the image before the loop starts", async () => {
    const order: string[] = [];
    invokeMock.mockImplementation(async (command: string) => {
      if (command === "checkpoint_list") return [FROZEN];
      if (command === "checkpoint_restorability") {
        return {
          restorability: { state: "resumable", processId: "proc-frozen" },
          determinismCaveats: [],
          blockerExplanations: [],
        };
      }
      order.push(command);
      return undefined;
    });
    runAgentTurnMock.mockImplementation(async () => {
      order.push("runAgentTurn");
    });

    await resumeFrozenTurn(record());

    expect(order).toEqual(["checkpoint_clear_freeze", "runAgentTurn"]);
  });

  /**
   * A refusal is answered once and retired. Leaving the row suspended would have
   * the two-second sweep re-deliver it and append the same refusal to the
   * transcript forever.
   */
  it("writes the blockers into the transcript and retires the row", async () => {
    respond({
      restorability: { state: "blocked", blockers: ["model-not-resident"] },
      determinismCaveats: [],
      blockerExplanations: ["The model this process was running against is not loaded on this host."],
    });

    expect(await resumeFrozenTurn(record())).toBe("blocked");
    expect(runAgentTurnMock).not.toHaveBeenCalled();
    expect(exitProcessMock.mock.calls[0]?.[1]).toBe("failed");

    const messages = useSessionStore.getState().sessions[0].messages;
    expect(messages).toHaveLength(1);
    expect(messages[0].content).toContain("not loaded on this host");
  });

  it("reports no image when nothing on disk claims this process", async () => {
    respond(null, [{ id: "cp-0", sessionId: "session-1", frozenProcessId: "someone-else" }]);

    expect(await resumeFrozenTurn(record())).toBe("no-image");
    expect(runAgentTurnMock).not.toHaveBeenCalled();
  });

  /** The conversation was deleted while the image sat on disk. Nothing to
   * continue, so the row is retired rather than re-read by every sweep. */
  it("retires the row when its conversation is gone", async () => {
    useSessionStore.setState({ sessions: [] } as never);
    respond(null);

    expect(await resumeFrozenTurn(record())).toBe("no-image");
    expect(exitProcessMock.mock.calls[0]?.[1]).toBe("cancelled");
    expect(runAgentTurnMock).not.toHaveBeenCalled();
  });
});
