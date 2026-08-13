/**
 * What re-entering a frozen turn decides, what it refuses to decide, and — the
 * part that needed a whole fake backend to state — the order it does things in.
 *
 * The durable continuation's own rules (one request id, one continuation, one
 * job, one run) are Rust's and are proved there against a real store. What is
 * proved *here* is the lifecycle on this side of the bridge, because that is
 * where the interesting failure lived: every ordering bug in a Resume is a bug
 * about which local state was destroyed before the backend had answered, and no
 * amount of testing `resume_accepted_turn` directly can see it.
 *
 * So the backend below is a fake, and it is faithful on exactly the two axes the
 * ordering depends on: a Resume is identified by its request id, and the same
 * request id always reaches the same continuation. Everything else it does is
 * fault injection — refuse, fail, lose the response, come back a process later —
 * because those are the moments the real one has and the real one cannot be
 * asked to have on demand.
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
  runAgentTurn: (...args: unknown[]) => runAgentTurnMock(...args),
}));

const exitProcessMock = vi.fn<(id: string, status: string, reason?: string | null) => Promise<void>>(
  async () => {},
);
vi.mock("./processTable", () => ({
  exitProcess: (id: string, status: string, reason?: string | null) =>
    exitProcessMock(id, status, reason),
}));

import { resumeFrozenTurn, type FrozenResumeOutcome } from "./frozenTurn";
import type { ProcessRecord } from "./processTable";
import { useSessionStore } from "../store/sessionStore";

const PARENT_TURN = "turn-1";
const SESSION = "session-1";

interface Continuation {
  ingressId: string;
  parentIngressId: string;
  jobId: string;
  runId: string;
  /** The model the *parent* was accepted with. Never re-derived. */
  modelTarget: string;
  requestId: string;
}

/**
 * The durable backend, as much of it as an ordering test can be wrong about.
 *
 * Holds one accepted turn with a frozen execution context, and answers a resume
 * from the request id alone: the continuation, its job and its run are derived
 * from `parent + requestId`, so asking twice returns what exists rather than
 * making a second one. That is the whole property the app's retry safety rests
 * on, mirrored here so a retry that broke it would fail loudly instead of
 * quietly producing two runs nobody can un-run.
 */
class FakeBackend {
  /** Images on disk, by id. Cleared only by `checkpoint_clear_freeze`. */
  images = new Map<string, { id: string; sessionId: string; frozenProcessId: string | null }>();
  /** Continuations, by the *derived* identity — never by call count. */
  continuations = new Map<string, Continuation>();
  /** The model the accepted turn was frozen with, at T1. */
  frozenModel = "provider:anthropic/model-a";
  /** What the operator has selected right now. Nothing may read this. */
  currentModel = "provider:anthropic/model-a";
  /** Set when the operator's frozen credential has since been deleted. */
  credentialRevoked = false;
  /** Fails every `ingress_turn_resume` while set — the transport is down. */
  transportDown = false;
  /** Accepts durably and then loses the response, once per request id. */
  loseResponseFor = new Set<string>();
  /** Every request id that reached the backend, in order, including retries. */
  requests: string[] = [];
  /** Every command the app sent, in order. */
  calls: string[] = [];

  freeze(id: string, processId: string): void {
    this.images.set(id, { id, sessionId: SESSION, frozenProcessId: processId });
  }

  /** How many *distinct* continuations exist — the count a duplicate resume
   * would move and a retry must not. */
  get continuationCount(): number {
    return this.continuations.size;
  }

  resume(account: string, event: string, requestId: string): Continuation {
    this.requests.push(requestId);
    if (this.transportDown) throw new Error("the resident runner is not answering");
    if (this.credentialRevoked) {
      // The refusal the daemon derives from the *frozen* context: the model this
      // turn was accepted to run on names a credential that is gone. A value,
      // not a throw — the caller must be able to tell this from a lost message.
      throw { __refused: `The credential for ${this.frozenModel} is no longer available.` };
    }
    // Identity from the caller's request id, exactly as `resume_of` derives it:
    // never a count of the continuations already here, which cannot tell a retry
    // from a second press.
    const key = `${account}/${event}#resume-${requestId}`;
    const existing = this.continuations.get(key);
    if (existing) {
      if (this.loseResponseFor.has(requestId)) throw new Error("the response was lost");
      return existing;
    }
    const continuation: Continuation = {
      ingressId: `ingress-${key}`,
      parentIngressId: `ingress-${account}/${event}`,
      jobId: `job-${key}`,
      runId: `run-${key}`,
      // Inherited from what the parent was accepted with. `currentModel` is
      // deliberately unreachable from here.
      modelTarget: this.frozenModel,
      requestId,
    };
    this.continuations.set(key, continuation);
    if (this.loseResponseFor.has(requestId)) throw new Error("the response was lost");
    return continuation;
  }
}

let backend: FakeBackend;
/** The payload of the last `checkpoint_restorability` call. */
let restorabilityArgs: Record<string, unknown> | null = null;

function install(backend: FakeBackend): void {
  invokeMock.mockImplementation(async (command: string, args?: Record<string, unknown>) => {
    backend.calls.push(command);
    switch (command) {
      case "checkpoint_list":
        return [...backend.images.values()];
      case "checkpoint_restorability": {
        restorabilityArgs = args ?? {};
        if (!backend.images.has(String(args?.id))) throw new Error("no such checkpoint");
        return {
          restorability: { state: "resumable", processId: "proc-frozen" },
          determinismCaveats: ["Model sampling is not replayed."],
          blockerExplanations: [],
        };
      }
      case "checkpoint_clear_freeze":
        backend.images.delete(String(args?.id));
        return undefined;
      case "ingress_turn_resume": {
        try {
          const resumed = backend.resume(
            String(args?.account),
            String(args?.event),
            String(args?.requestId),
          );
          return {
            ingress_id: resumed.ingressId,
            parent_ingress_id: resumed.parentIngressId,
            job_id: resumed.jobId,
            run_id: resumed.runId,
          };
        } catch (error) {
          // A refusal comes back as a value; a transport failure rejects. The
          // difference is the whole reason the app can tell them apart.
          if (error && typeof error === "object" && "__refused" in error) {
            return { refused: (error as { __refused: string }).__refused };
          }
          throw error;
        }
      }
      default:
        return undefined;
    }
  });
}

function record(processId = "proc-frozen", externalId = PARENT_TURN): ProcessRecord {
  return { processId, kind: "chat_turn", externalId } as ProcessRecord;
}

/** The `ResumedTurn` the loop was handed, if it was handed one. */
function handedToLoop(call = 0) {
  return runAgentTurnMock.mock.calls[call]?.[8] as
    | {
      resumedFromCheckpointId: string;
      parentTurnId: string;
      accepted: { ingressId: string; jobId: string; runId: string };
    }
    | undefined;
}

function transcript(): string[] {
  return useSessionStore
    .getState()
    .sessions[0].messages.map((message) => String(message.content));
}

beforeEach(() => {
  invokeMock.mockReset();
  runAgentTurnMock.mockReset();
  runAgentTurnMock.mockImplementation(async () => {});
  exitProcessMock.mockReset();
  restorabilityArgs = null;
  backend = new FakeBackend();
  backend.freeze("cp-1", "proc-frozen");
  install(backend);
  useSessionStore.setState({ sessions: [{ id: SESSION, messages: [] }] } as never);
});

describe("resumeFrozenTurn", () => {
  it("re-enters the frozen turn's own session with no new user message", async () => {
    expect(await resumeFrozenTurn(record())).toBe("resumed");

    const [sessionId, userText] = runAgentTurnMock.mock.calls[0];
    expect(sessionId).toBe(SESSION);
    expect(userText).toBe("");
    expect(handedToLoop()).toMatchObject({
      resumedFromCheckpointId: "cp-1",
      parentTurnId: PARENT_TURN,
    });
  });

  /**
   * The ordering the whole module is arranged around. An image cleared before
   * the backend has the continuation is a frozen turn destroyed to make room for
   * a resume that may never have happened; an image cleared after is a resume
   * that can be asked for again at any point until it works.
   */
  it("accepts the continuation durably before it clears the image or retires the row", async () => {
    const order: string[] = [];
    exitProcessMock.mockImplementation(async (_id, status) => {
      order.push(`exitProcess:${status}`);
    });
    runAgentTurnMock.mockImplementation(async () => {
      order.push("runAgentTurn");
    });
    invokeMock.mockImplementation(
      (
        (inner) =>
          async (command: string, args?: Record<string, unknown>) => {
            if (command !== "checkpoint_list" && command !== "checkpoint_restorability") {
              order.push(command);
            }
            return inner(command, args);
          }
      )(invokeMock.getMockImplementation()!),
    );

    await resumeFrozenTurn(record());

    expect(order).toEqual([
      "ingress_turn_resume",
      "checkpoint_clear_freeze",
      "exitProcess:succeeded",
      "runAgentTurn",
    ]);
  });

  // -- Test 1: the backend fails before it accepts anything ------------------

  /**
   * Nothing durable happened, so nothing local may be spent. The image is what
   * the operator would otherwise have lost, and the suspended row is how they
   * ask again.
   */
  it("keeps the image, the row and the request id when the backend never accepted", async () => {
    backend.transportDown = true;

    expect(await resumeFrozenTurn(record())).toBe("deferred");

    expect(backend.images.has("cp-1")).toBe(true);
    expect(exitProcessMock).not.toHaveBeenCalled();
    expect(backend.continuationCount).toBe(0);
    expect(runAgentTurnMock).not.toHaveBeenCalled();
    // Silent on purpose: the operator has not been answered, because there is
    // no answer yet — the sweep is still trying.
    expect(transcript()).toEqual([]);
    // Every attempt, including the retries inside one press, carried the one id.
    expect(new Set(backend.requests)).toEqual(new Set(["cp-1"]));

    // And the retry is a retry: same id, and it lands the moment the backend is
    // back rather than starting a second Resume.
    backend.transportDown = false;
    expect(await resumeFrozenTurn(record())).toBe("resumed");
    expect(backend.continuationCount).toBe(1);
    expect(new Set(backend.requests)).toEqual(new Set(["cp-1"]));
  });

  // -- Test 2: the backend accepted, the answer was lost ---------------------

  /**
   * The race that made the request id necessary in the first place. The caller
   * cannot know it was accepted, so it must retry — and the retry has to land on
   * what exists, not make a second one.
   */
  it("discovers the existing continuation when the response to an accepted resume is lost", async () => {
    backend.loseResponseFor.add("cp-1");

    expect(await resumeFrozenTurn(record())).toBe("deferred");
    // Accepted despite the caller not hearing so — which is exactly why the
    // image survived this attempt.
    expect(backend.continuationCount).toBe(1);
    expect(backend.images.has("cp-1")).toBe(true);
    expect(exitProcessMock).not.toHaveBeenCalled();

    // The answer gets through this time. Same request, same continuation.
    backend.loseResponseFor.clear();
    expect(await resumeFrozenTurn(record())).toBe("resumed");

    expect(backend.continuationCount).toBe(1);
    const [only] = [...backend.continuations.values()];
    expect(only.requestId).toBe("cp-1");
    expect(handedToLoop()).toMatchObject({
      accepted: { ingressId: only.ingressId, jobId: only.jobId, runId: only.runId },
    });
    // One logical run, and it is the one the loop was told to watch.
    expect(new Set([...backend.continuations.values()].map((entry) => entry.runId)).size).toBe(1);
  });

  // -- Test 3: a crash between acceptance and the checkpoint clear -----------

  /**
   * The window the new ordering deliberately creates, and the reason it is safe
   * to create it: after acceptance the image is stale rather than authoritative,
   * so encountering it again costs one repeated request and no second run.
   */
  it("re-finds the same continuation when a crash left the image uncleared", async () => {
    // The process dies right after the backend accepted: clearing the freeze
    // never runs.
    invokeMock.mockImplementation(
      (
        (inner) =>
          async (command: string, args?: Record<string, unknown>) => {
            if (command === "checkpoint_clear_freeze") throw new Error("the app died here");
            return inner(command, args);
          }
      )(invokeMock.getMockImplementation()!),
    );

    expect(await resumeFrozenTurn(record())).toBe("resumed");
    const accepted = [...backend.continuations.values()][0];
    // The image outlived the crash, which is what makes it findable again.
    expect(backend.images.has("cp-1")).toBe(true);

    // A new process starts, sweeps, and finds the same image on the same row.
    install(backend);
    runAgentTurnMock.mockClear();
    expect(await resumeFrozenTurn(record())).toBe("resumed");

    expect(backend.requests.every((id) => id === "cp-1")).toBe(true);
    expect(backend.continuationCount).toBe(1);
    expect(handedToLoop()).toMatchObject({
      accepted: { ingressId: accepted.ingressId, runId: accepted.runId },
    });
    // …and now the stale image is safely retired.
    expect(backend.images.has("cp-1")).toBe(false);
  });

  // -- Test 4: the operator changed models in between -----------------------

  /**
   * The coupling this module used to have, stated as a test. Resume eligibility
   * was decided by handing `checkpoint_restorability` the app's currently
   * selected target as the host's resident models — so switching models between
   * freezing and resuming refused the resume, and the current selection was
   * quietly the authority over a turn accepted before it existed.
   */
  it("resumes under the frozen model after the operator switched the current one", async () => {
    backend.currentModel = "provider:anthropic/model-b";

    expect(await resumeFrozenTurn(record())).toBe("resumed");

    // Nothing asked this host what it has loaded, which is what stops the
    // current selection from deciding anything.
    expect(restorabilityArgs).not.toHaveProperty("residentModels");
    const [only] = [...backend.continuations.values()];
    expect(only.modelTarget).toBe("provider:anthropic/model-a");
    expect(only.modelTarget).not.toBe(backend.currentModel);
  });

  // -- Test 5: the frozen model's credential is genuinely gone --------------

  /**
   * The opposite failure to Test 4 and the reason that one is safe: not checking
   * the *current* model does not mean not checking anything. The frozen context
   * names a credential, and when it is gone the resume fails saying so rather
   * than continuing the conversation on whatever is configured now.
   */
  it("fails explicitly when the frozen credential is gone, without falling back", async () => {
    backend.credentialRevoked = true;
    backend.currentModel = "provider:anthropic/model-b";

    expect(await resumeFrozenTurn(record())).toBe("blocked");

    expect(transcript().join("\n")).toContain("model-a");
    expect(transcript().join("\n")).not.toContain("model-b");
    expect(backend.continuationCount).toBe(0);
    expect(runAgentTurnMock).not.toHaveBeenCalled();
    // Answered once and retired, so the sweep stops — but the image stays on
    // disk for whoever restores the credential.
    expect(exitProcessMock.mock.calls[0]?.[1]).toBe("failed");
    expect(backend.images.has("cp-1")).toBe(true);
    // One request. A refusal is an answer, so it is not retried.
    expect(backend.requests).toEqual(["cp-1"]);
  });

  // -- Test 6: the same Resume action delivered repeatedly ------------------

  /**
   * The sweep re-delivers, a component re-renders, an operator presses twice
   * while the first is in flight. All of them read the same image, so all of
   * them send the same id.
   */
  it("collapses a re-delivered resume signal onto one continuation", async () => {
    // Delivered three times before the image is ever cleared — the shape a
    // re-delivery actually has.
    const outcomes: FrozenResumeOutcome[] = [];
    const clearing = invokeMock.getMockImplementation()!;
    invokeMock.mockImplementation(async (command: string, args?: Record<string, unknown>) =>
      command === "checkpoint_clear_freeze" ? undefined : clearing(command, args));

    outcomes.push(await resumeFrozenTurn(record()));
    outcomes.push(await resumeFrozenTurn(record()));
    outcomes.push(await resumeFrozenTurn(record()));

    expect(outcomes).toEqual(["resumed", "resumed", "resumed"]);
    expect(backend.requests).toEqual(["cp-1", "cp-1", "cp-1"]);
    expect(backend.continuationCount).toBe(1);
    const [only] = [...backend.continuations.values()];
    expect(new Set([only.jobId]).size).toBe(1);
    for (let call = 0; call < 3; call += 1) {
      expect(handedToLoop(call)).toMatchObject({ accepted: { runId: only.runId } });
    }
  });

  // -- Test 7: a genuinely new Resume, later ---------------------------------

  /**
   * The other half of the identity rule: two presses that are actually two
   * decisions must be two continuations. They are, because the second one is a
   * different freeze and therefore a different image.
   */
  it("gives a later, genuinely new freeze its own request id and continuation", async () => {
    expect(await resumeFrozenTurn(record())).toBe("resumed");
    expect(backend.images.has("cp-1")).toBe(false);

    // The continuation runs, freezes again at a later tool boundary, and the
    // operator resumes that.
    backend.freeze("cp-2", "proc-frozen-again");
    expect(await resumeFrozenTurn(record("proc-frozen-again"))).toBe("resumed");

    expect(backend.requests).toEqual(["cp-1", "cp-2"]);
    expect(backend.continuationCount).toBe(2);
    expect(handedToLoop(1)).toMatchObject({ resumedFromCheckpointId: "cp-2" });
  });

  // -- The verdicts that were already this module's ---------------------------

  /**
   * A refusal is answered once and retired. Leaving the row suspended would have
   * the two-second sweep re-deliver it and append the same refusal to the
   * transcript forever.
   */
  it("writes the blockers into the transcript and retires the row", async () => {
    const blocked = invokeMock.getMockImplementation()!;
    invokeMock.mockImplementation(async (command: string, args?: Record<string, unknown>) => {
      if (command !== "checkpoint_restorability") return blocked(command, args);
      return {
        restorability: { state: "blocked", blockers: ["workspace-gone"] },
        determinismCaveats: [],
        blockerExplanations: [
          "The workspace this process was running in no longer exists.",
        ],
      };
    });

    expect(await resumeFrozenTurn(record())).toBe("blocked");
    expect(runAgentTurnMock).not.toHaveBeenCalled();
    expect(exitProcessMock.mock.calls[0]?.[1]).toBe("failed");
    expect(transcript()[0]).toContain("no longer exists");
    // Nothing was asked of the backend, and the image is untouched.
    expect(backend.requests).toEqual([]);
    expect(backend.images.has("cp-1")).toBe(true);
  });

  /** An image from before turns were durable has no accepted turn to continue,
   * and resolving the current configuration instead is the one thing that must
   * not happen. */
  it("refuses an image whose turn predates durable identity", async () => {
    expect(await resumeFrozenTurn(record("proc-frozen", ""))).toBe("blocked");

    expect(transcript()[0]).toContain("predates durable turn identity");
    expect(backend.requests).toEqual([]);
    expect(runAgentTurnMock).not.toHaveBeenCalled();
    expect(exitProcessMock.mock.calls[0]?.[1]).toBe("failed");
  });

  it("reports no image when nothing on disk claims this process", async () => {
    expect(await resumeFrozenTurn(record("someone-else"))).toBe("no-image");
    expect(runAgentTurnMock).not.toHaveBeenCalled();
  });

  /** The conversation was deleted while the image sat on disk. Nothing to
   * continue, so the row is retired rather than re-read by every sweep. */
  it("retires the row when its conversation is gone", async () => {
    useSessionStore.setState({ sessions: [] } as never);

    expect(await resumeFrozenTurn(record())).toBe("no-image");
    expect(exitProcessMock.mock.calls[0]?.[1]).toBe("cancelled");
    expect(runAgentTurnMock).not.toHaveBeenCalled();
    expect(backend.requests).toEqual([]);
  });
});
