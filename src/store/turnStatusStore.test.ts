import { beforeEach, describe, expect, it } from "vitest";

import { selectTurnStatus, useTurnStatusStore } from "./turnStatusStore";

beforeEach(() => {
  useTurnStatusStore.setState({ turns: {} });
});

describe("turnStatusStore", () => {
  it("begin registers a fresh turn with zero tokens and no activity", () => {
    useTurnStatusStore.getState().begin("s1");

    const status = selectTurnStatus("s1")(useTurnStatusStore.getState());
    expect(status).toEqual({
      sessionId: "s1",
      startedAt: expect.any(Number),
      totalTokens: 0,
      activity: "",
      lastEventAt: expect.any(Number),
    });
  });

  it("addTokens and setActivity refresh lastEventAt (silence tracking)", () => {
    useTurnStatusStore.getState().begin("s1");
    // Backdate the entry so the refresh is observable.
    useTurnStatusStore.setState((state) => ({
      turns: { ...state.turns, s1: { ...state.turns.s1, lastEventAt: 1_000 } },
    }));

    useTurnStatusStore.getState().addTokens("s1", 10);
    const afterTokens = selectTurnStatus("s1")(useTurnStatusStore.getState())!.lastEventAt;
    expect(afterTokens).toBeGreaterThan(1_000);

    useTurnStatusStore.setState((state) => ({
      turns: { ...state.turns, s1: { ...state.turns.s1, lastEventAt: 1_000 } },
    }));
    useTurnStatusStore.getState().setActivity("s1", "read_file");
    expect(selectTurnStatus("s1")(useTurnStatusStore.getState())!.lastEventAt).toBeGreaterThan(1_000);
  });

  it("addTokens accumulates across attempts and no-ops without a registered turn", () => {
    useTurnStatusStore.getState().addTokens("s1", 500);
    expect(selectTurnStatus("s1")(useTurnStatusStore.getState())).toBeUndefined();

    useTurnStatusStore.getState().begin("s1");
    useTurnStatusStore.getState().addTokens("s1", 500);
    useTurnStatusStore.getState().addTokens("s1", 250);
    expect(selectTurnStatus("s1")(useTurnStatusStore.getState())?.totalTokens).toBe(750);
  });

  it("setActivity sets and clears the tool label, no-oping without a turn", () => {
    useTurnStatusStore.getState().setActivity("s1", "read_file");
    expect(selectTurnStatus("s1")(useTurnStatusStore.getState())).toBeUndefined();

    useTurnStatusStore.getState().begin("s1");
    useTurnStatusStore.getState().setActivity("s1", "read_file");
    expect(selectTurnStatus("s1")(useTurnStatusStore.getState())?.activity).toBe("read_file");
    useTurnStatusStore.getState().setActivity("s1", "");
    expect(selectTurnStatus("s1")(useTurnStatusStore.getState())?.activity).toBe("");
  });

  it("end removes the entry and leaves other sessions' turns untouched", () => {
    useTurnStatusStore.getState().begin("s1");
    useTurnStatusStore.getState().begin("s2");

    useTurnStatusStore.getState().end("s1");
    expect(selectTurnStatus("s1")(useTurnStatusStore.getState())).toBeUndefined();
    expect(selectTurnStatus("s2")(useTurnStatusStore.getState())).toBeDefined();

    // Ending an already-ended session is a harmless no-op.
    useTurnStatusStore.getState().end("s1");
    expect(selectTurnStatus("s2")(useTurnStatusStore.getState())).toBeDefined();
  });
});
