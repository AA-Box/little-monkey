import { beforeEach, describe, expect, it, vi } from "vitest";

// `runDebate` drives independent model calls via `turnEngine.ts`'s
// `attemptStream` — mocked here (same pattern as `sideTaskRunner.test.ts`)
// so these tests pin the RUNNER's own behavior (independence, parsing,
// partial-failure handling, cancellation) without a real streaming provider.
const attemptStreamMock = vi.fn();
vi.mock("./turnEngine", () => ({
  attemptStream: (...args: unknown[]) => attemptStreamMock(...args),
  describeUsageTarget: (target: { kind: string; providerId?: string; model?: string }) =>
    target.kind === "local" ? "Local model" : target.kind === "ollama" ? `Ollama · ${target.model}` : `${target.providerId} · ${target.model}`,
}));

const resolveTargetMock = vi.fn();
vi.mock("./agentLoop", () => ({ resolveTarget: (...args: unknown[]) => resolveTargetMock(...args) }));

import { DEBATE_ROLES, cancelDebate, runDebate, startDebate } from "./debateRunner";
import { useDebateStore } from "../store/debateStore";
import { useUsageHistoryStore } from "../store/usageHistoryStore";

const localTarget = { kind: "local" as const, baseUrl: "http://localhost:8090", modelLabel: "Local model" };

function roleReply(position: string, objections: string[] = ["Some risk."]): string {
  return [`POSITION: ${position}`, "OBJECTIONS:", ...objections.map((o) => `- ${o}`)].join("\n");
}

const SYNTHESIS_JSON = JSON.stringify({
  recommendation: "Go with option A.",
  objectionHandling: [
    { roleId: "critic", objection: "It might not scale.", resolution: "Accepted the risk given current load." },
    { roleId: "security", objection: "Needs auth review.", resolution: "Scheduled before launch." },
  ],
  tradeoffs: "Speed over long-term flexibility.",
  whyThisWon: "Fastest path that still passes security review.",
});

// Creates a run directly via the store (status `idle`, six pending
// positions) WITHOUT firing `runDebate` — mirrors `sideTaskRunner.test.ts`'s
// `seedTask` helper, which seeds `useSideTaskStore` directly rather than
// going through `startSideTask` so each test controls exactly one
// `runDebate`/`runSideTask` invocation instead of racing `startDebate`'s own
// fire-and-forget call against an explicit `await runDebate(id)`.
function seedRun(question = "Should we use Redis or Postgres for sessions?") {
  const initialPositions = DEBATE_ROLES.map((role) => ({
    roleId: role.id,
    roleLabel: role.label,
    status: "pending" as const,
    position: null,
    objections: [],
    rawOutput: "",
    error: null,
    startedAt: null,
    completedAt: null,
  }));
  return useDebateStore.getState().create(crypto.randomUUID(), question, initialPositions).id;
}

beforeEach(() => {
  attemptStreamMock.mockReset();
  resolveTargetMock.mockReset();
  resolveTargetMock.mockResolvedValue(localTarget);
  useDebateStore.setState({ runs: {}, order: [], activeRunId: null });
  useUsageHistoryStore.getState().clear();
});

describe("startDebate", () => {
  it("rejects an empty question without starting anything", () => {
    expect(() => startDebate("   ")).toThrow(/decision question/i);
    expect(attemptStreamMock).not.toHaveBeenCalled();
  });

  it("creates a run with all six roles pending before any model call resolves", () => {
    attemptStreamMock.mockImplementation(() => new Promise(() => {})); // never resolves
    const id = seedRun();
    const run = useDebateStore.getState().runs[id];
    expect(run.status).toBe("idle");
    expect(run.positions).toHaveLength(6);
    expect(run.positions.every((p) => p.status === "pending")).toBe(true);
    expect(run.positions.map((p) => p.roleId)).toEqual(DEBATE_ROLES.map((r) => r.id));
  });
});

describe("runDebate / independence", () => {
  it("gives every role ONLY its own system prompt and the bare question — never another role's reply", async () => {
    attemptStreamMock.mockImplementation((_target: unknown, wireHistory: { role: string; content: string }[]) => {
      // Only ever a two-message wire history: this role's own system
      // prompt, then the user question — no assistant/tool messages from
      // any sibling role ever appear here.
      expect(wireHistory).toHaveLength(2);
      expect(wireHistory[0].role).toBe("system");
      const isSynthesis = wireHistory[0].content.includes("synthesis judge");
      if (!isSynthesis) expect(wireHistory[1]).toEqual({ role: "user", content: "Pick a database" });
      return Promise.resolve({
        content: isSynthesis ? SYNTHESIS_JSON : roleReply("Pick Postgres."),
        toolCalls: [],
        streamError: null,
        contentStarted: true,
      });
    });

    const id = seedRun("Pick a database");

    await runDebate(id);

    // 6 independent role calls + 1 synthesis call.
    expect(attemptStreamMock).toHaveBeenCalledTimes(7);
    const run = useDebateStore.getState().runs[id];
    expect(run.status).toBe("completed");
    expect(run.positions.every((p) => p.status === "completed")).toBe(true);
  });
});

describe("runDebate / parsing and synthesis", () => {
  it("parses POSITION/OBJECTIONS per role and the synthesis JSON, preserving every role's objection", async () => {
    let call = 0;
    attemptStreamMock.mockImplementation(() => {
      call += 1;
      if (call <= 6) {
        return Promise.resolve({
          content: roleReply(`Position #${call}`, [`Objection A${call}`, `Objection B${call}`]),
          toolCalls: [],
          streamError: null,
          contentStarted: true,
        });
      }
      return Promise.resolve({ content: SYNTHESIS_JSON, toolCalls: [], streamError: null, contentStarted: true });
    });

    const id = seedRun();
    await runDebate(id);

    const run = useDebateStore.getState().runs[id];
    expect(run.status).toBe("completed");
    expect(run.positions.map((p) => p.position)).toEqual(["Position #1", "Position #2", "Position #3", "Position #4", "Position #5", "Position #6"]);
    expect(run.positions[0].objections).toEqual(["Objection A1", "Objection B1"]);
    expect(run.synthesis).not.toBeNull();
    expect(run.synthesis!.parseFailed).toBe(false);
    expect(run.synthesis!.recommendation).toBe("Go with option A.");
    expect(run.synthesis!.objectionHandling).toHaveLength(2);
    expect(run.synthesis!.objectionHandling[0]).toEqual({
      roleId: "critic",
      roleLabel: "Critic",
      objection: "It might not scale.",
      resolution: "Accepted the risk given current load.",
    });
  });

  it("falls back to a raw, parseFailed synthesis when the synthesis reply isn't valid JSON — never dropping it", async () => {
    let call = 0;
    attemptStreamMock.mockImplementation(() => {
      call += 1;
      if (call <= 6) {
        return Promise.resolve({ content: roleReply("A position"), toolCalls: [], streamError: null, contentStarted: true });
      }
      return Promise.resolve({ content: "I refuse to output JSON, here's my prose answer instead.", toolCalls: [], streamError: null, contentStarted: true });
    });

    const id = seedRun();
    await runDebate(id);

    const run = useDebateStore.getState().runs[id];
    expect(run.status).toBe("completed");
    expect(run.synthesis!.parseFailed).toBe(true);
    expect(run.synthesis!.recommendation).toBe("I refuse to output JSON, here's my prose answer instead.");
    expect(run.synthesis!.raw).toContain("I refuse to output JSON");
  });

  it("degrades a role reply that ignores the POSITION/OBJECTIONS shape to a plain position with no objections, instead of discarding it", async () => {
    let call = 0;
    attemptStreamMock.mockImplementation(() => {
      call += 1;
      if (call <= 6) {
        return Promise.resolve({ content: "Just a plain unstructured reply.", toolCalls: [], streamError: null, contentStarted: true });
      }
      return Promise.resolve({ content: SYNTHESIS_JSON, toolCalls: [], streamError: null, contentStarted: true });
    });

    const id = seedRun();
    await runDebate(id);

    const run = useDebateStore.getState().runs[id];
    expect(run.positions[0].status).toBe("completed");
    expect(run.positions[0].position).toBe("Just a plain unstructured reply.");
    expect(run.positions[0].objections).toEqual([]);
  });
});

describe("runDebate / partial failure", () => {
  it("keeps failed roles isolated — the rest still complete and synthesis still runs over what's left", async () => {
    let call = 0;
    attemptStreamMock.mockImplementation(() => {
      call += 1;
      if (call === 1) {
        return Promise.resolve({ content: "", toolCalls: [], streamError: "provider timed out", contentStarted: false });
      }
      if (call <= 6) {
        return Promise.resolve({ content: roleReply("Fine position"), toolCalls: [], streamError: null, contentStarted: true });
      }
      return Promise.resolve({ content: SYNTHESIS_JSON, toolCalls: [], streamError: null, contentStarted: true });
    });

    const id = seedRun();
    await runDebate(id);

    const run = useDebateStore.getState().runs[id];
    expect(run.status).toBe("completed");
    expect(run.positions[0].status).toBe("failed");
    expect(run.positions[0].error).toBe("provider timed out");
    expect(run.positions.slice(1).every((p) => p.status === "completed")).toBe(true);
    // Synthesis call still happened (the 7th call).
    expect(attemptStreamMock).toHaveBeenCalledTimes(7);
  });

  it("fails the whole run without calling synthesis when every role fails", async () => {
    attemptStreamMock.mockResolvedValue({ content: "", toolCalls: [], streamError: "boom", contentStarted: false });

    const id = seedRun();
    await runDebate(id);

    const run = useDebateStore.getState().runs[id];
    expect(run.status).toBe("failed");
    expect(run.synthesis).toBeNull();
    // Only the 6 role calls — no synthesis call.
    expect(attemptStreamMock).toHaveBeenCalledTimes(6);
  });

  it("fails the run when the active target cannot be resolved, without calling attemptStream", async () => {
    resolveTargetMock.mockRejectedValue(new Error("No AI provider model selected"));

    const id = seedRun();
    await runDebate(id);

    const run = useDebateStore.getState().runs[id];
    expect(run.status).toBe("failed");
    expect(run.error).toBe("No AI provider model selected");
    expect(attemptStreamMock).not.toHaveBeenCalled();
  });
});

describe("cancelDebate", () => {
  it("marks a running debate cancelled once its in-flight calls observe the abort signal", async () => {
    attemptStreamMock.mockImplementation((_target: unknown, _wire: unknown, _tools: unknown, signal: AbortSignal) => {
      return new Promise((resolve) => {
        signal.addEventListener("abort", () => resolve({ content: "", toolCalls: [], streamError: null, contentStarted: false }), { once: true });
      });
    });

    const id = seedRun();
    const done = runDebate(id);
    cancelDebate(id);
    await done;

    const run = useDebateStore.getState().runs[id];
    expect(run.status).toBe("cancelled");
  });
});
