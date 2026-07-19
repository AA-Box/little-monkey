import { beforeEach, describe, expect, it, vi } from "vitest";

const mocks = vi.hoisted(() => ({
  resolveTarget: vi.fn(),
  attemptStream: vi.fn(),
}));

vi.mock("../lib/agentLoop", () => ({
  resolveTarget: (...args: unknown[]) => mocks.resolveTarget(...args),
}));
vi.mock("../lib/turnEngine", () => ({
  attemptStream: (...args: unknown[]) => mocks.attemptStream(...args),
}));

import {
  __resetSpecScorerControllersForTests,
  useSpecScorerStore,
} from "./specScorerStore";

const VALID_JSON =
  '{"dimensions":{"clarity":20,"scope":20,"missingContext":10,"testability":10,"dependencies":30,"agentReadiness":15},"missingInfo":["What API should this call?","What is the expected response shape?"],"summary":"Too vague to implement unattended."}';

const HIGH_SCORE_JSON =
  '{"dimensions":{"clarity":90,"scope":85,"missingContext":90,"testability":88,"dependencies":95,"agentReadiness":92},"missingInfo":[],"summary":"Ready for an autonomous run."}';

function localTarget() {
  return { kind: "local" as const, baseUrl: "http://127.0.0.1:8080", modelLabel: "Local model" };
}

beforeEach(() => {
  mocks.resolveTarget.mockReset();
  mocks.attemptStream.mockReset();
  __resetSpecScorerControllersForTests();
  mocks.resolveTarget.mockResolvedValue(localTarget());
  useSpecScorerStore.setState({ scoresByRun: {}, statusByRun: {}, errorByRun: {} });
});

describe("specScorerStore", () => {
  it("scores a run and stores the parsed result", async () => {
    mocks.attemptStream.mockResolvedValue({ content: VALID_JSON, streamError: null, toolCalls: [], contentStarted: true });

    await useSpecScorerStore.getState().scoreRun("run-1", "Vague issue", "Do the thing");

    const state = useSpecScorerStore.getState();
    expect(state.statusByRun["run-1"]).toBe("done");
    expect(state.scoresByRun["run-1"]?.overall).toBeLessThan(60);
    expect(state.scoresByRun["run-1"]?.missingInfo).toHaveLength(2);
    expect(state.errorByRun["run-1"]).toBeNull();
  });

  it("passes the resolved target's effort through attemptStream with no tools and recordUsage=false", async () => {
    mocks.attemptStream.mockResolvedValue({ content: HIGH_SCORE_JSON, streamError: null, toolCalls: [], contentStarted: true });

    await useSpecScorerStore.getState().scoreRun("run-2", "Clear issue", "Well-specified body");

    expect(mocks.attemptStream).toHaveBeenCalledTimes(1);
    const call = mocks.attemptStream.mock.calls[0];
    expect(call[0]).toEqual(localTarget()); // target
    expect(call[2]).toEqual([]); // tools
    expect(call[7]).toBe(false); // recordUsage
  });

  it("scoreRun is a no-op once a run already has a status (cached, not re-scored on every call)", async () => {
    mocks.attemptStream.mockResolvedValue({ content: HIGH_SCORE_JSON, streamError: null, toolCalls: [], contentStarted: true });

    await useSpecScorerStore.getState().scoreRun("run-3", "Title", "Body");
    await useSpecScorerStore.getState().scoreRun("run-3", "Title", "Body");
    await useSpecScorerStore.getState().scoreRun("run-3", "Title", "Body");

    expect(mocks.attemptStream).toHaveBeenCalledTimes(1);
  });

  it("rescoreRun always re-runs even if a cached result already exists", async () => {
    mocks.attemptStream.mockResolvedValue({ content: HIGH_SCORE_JSON, streamError: null, toolCalls: [], contentStarted: true });

    await useSpecScorerStore.getState().scoreRun("run-4", "Title", "Body");
    await useSpecScorerStore.getState().rescoreRun("run-4", "Title", "Body");

    expect(mocks.attemptStream).toHaveBeenCalledTimes(2);
  });

  it("reports an error status without throwing when resolveTarget rejects", async () => {
    mocks.resolveTarget.mockRejectedValue(new Error("No AI provider model selected"));

    await useSpecScorerStore.getState().scoreRun("run-5", "Title", "Body");

    const state = useSpecScorerStore.getState();
    expect(state.statusByRun["run-5"]).toBe("error");
    expect(state.errorByRun["run-5"]).toBe("No AI provider model selected");
    expect(state.scoresByRun["run-5"]).toBeUndefined();
  });

  it("reports an error status (fails closed) when the model reply cannot be parsed", async () => {
    mocks.attemptStream.mockResolvedValue({ content: "not json", streamError: null, toolCalls: [], contentStarted: true });

    await useSpecScorerStore.getState().scoreRun("run-6", "Title", "Body");

    const state = useSpecScorerStore.getState();
    expect(state.statusByRun["run-6"]).toBe("error");
    expect(state.scoresByRun["run-6"]).toBeNull();
  });

  it("clearRun removes cached state so a later scoreRun call scores again", async () => {
    mocks.attemptStream.mockResolvedValue({ content: HIGH_SCORE_JSON, streamError: null, toolCalls: [], contentStarted: true });

    await useSpecScorerStore.getState().scoreRun("run-7", "Title", "Body");
    useSpecScorerStore.getState().clearRun("run-7");

    expect(useSpecScorerStore.getState().statusByRun["run-7"]).toBeUndefined();

    await useSpecScorerStore.getState().scoreRun("run-7", "Title", "Body");
    expect(mocks.attemptStream).toHaveBeenCalledTimes(2);
  });

  it("keeps each run's score independent of other runs", async () => {
    mocks.attemptStream
      .mockResolvedValueOnce({ content: VALID_JSON, streamError: null, toolCalls: [], contentStarted: true })
      .mockResolvedValueOnce({ content: HIGH_SCORE_JSON, streamError: null, toolCalls: [], contentStarted: true });

    await useSpecScorerStore.getState().scoreRun("run-vague", "Title", "Body");
    await useSpecScorerStore.getState().scoreRun("run-ready", "Title", "Body");

    const state = useSpecScorerStore.getState();
    expect(state.scoresByRun["run-vague"]?.overall).toBeLessThan(60);
    expect(state.scoresByRun["run-ready"]?.overall).toBeGreaterThanOrEqual(60);
  });
});
