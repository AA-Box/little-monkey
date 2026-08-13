import { describe, expect, it } from "vitest";
import {
  SOURCE_LABELS,
  turnFailureReason,
  turnStatus,
  type ConversationSource,
  type IngressTurn,
} from "./ingressClient";

function turn(overrides: Partial<IngressTurn> = {}): IngressTurn {
  return {
    ingress_id: "ingr-1",
    source: "messaging_channel",
    source_account_id: "acct-1",
    account_label: "Ops bot",
    source_event_id: "42",
    session_key: "channel:telegram:acct-1:chat-7",
    state: "queued",
    attempts: 1,
    last_error: null,
    execution_version: 1,
    execution_digest: "d".repeat(64),
    job_id: "ingress-abc",
    run_id: "run-1",
    run_state: "running",
    run_error: null,
    created_at_ms: 1_700_000_000_000,
    updated_at_ms: 1_700_000_000_000,
    ...overrides,
  };
}

describe("ingress turn status", () => {
  it("shows a turn the daemon has but the queue does not as waiting", () => {
    expect(turnStatus(turn({ state: "accepted", job_id: null, run_id: null, run_state: null }))).toBe(
      "waiting",
    );
  });

  it("keeps a parked turn failed even though no run ever started", () => {
    expect(
      turnStatus(turn({ state: "failed", job_id: null, run_state: null, last_error: "kill switch" })),
    ).toBe("failed");
  });

  it("lets the run's own outcome win once there is a run", () => {
    expect(turnStatus(turn({ run_state: "succeeded" }))).toBe("done");
    expect(turnStatus(turn({ run_state: "failed" }))).toBe("failed");
    expect(turnStatus(turn({ run_state: "cancelled" }))).toBe("failed");
    expect(turnStatus(turn({ run_state: "needs_reconciliation" }))).toBe("failed");
    expect(turnStatus(turn({ run_state: "queued" }))).toBe("waiting");
    expect(turnStatus(turn({ run_state: "waiting_approval" }))).toBe("running");
  });

  it("prefers the run's failure reason, since a run that started was submitted fine", () => {
    expect(turnFailureReason(turn({ run_error: "recipe not found", last_error: "queue busy" }))).toBe(
      "recipe not found",
    );
    expect(turnFailureReason(turn({ run_error: null, last_error: "queue busy" }))).toBe("queue busy");
    expect(turnFailureReason(turn())).toBeNull();
  });
});

describe("ingress origins", () => {
  it("names every origin the durable contract defines", () => {
    const sources: ConversationSource[] = [
      "desktop",
      "mobile",
      "messaging_channel",
      "peer",
      "voice",
      "telephone",
    ];
    for (const source of sources) {
      expect(SOURCE_LABELS[source].length).toBeGreaterThan(0);
    }
    expect(Object.keys(SOURCE_LABELS)).toHaveLength(sources.length);
  });
});
