import { describe, expect, it } from "vitest";

import {
  matchPolicy,
  observedTimeToFirstTokenMs,
  routeRequest,
  type RoutingCandidate,
  type RoutingPolicy,
  type RoutingRequest,
} from "./modelRouting";

function policy(patch: Partial<RoutingPolicy> = {}): RoutingPolicy {
  return {
    id: "p1",
    name: "Cheap chat",
    enabled: true,
    taskClasses: [],
    preferredTargetKeys: [],
    requiresTools: false,
    sensitivity: "any",
    maxInputPerMillionUsd: null,
    maxOutputPerMillionUsd: null,
    maxTimeToFirstTokenMs: null,
    ...patch,
  };
}

function candidate(patch: Partial<RoutingCandidate> & { key: string }): RoutingCandidate {
  return {
    label: patch.key,
    isLocal: false,
    available: true,
    toolCalling: "unknown",
    vision: "no",
    inputPerMillionUsd: null,
    outputPerMillionUsd: null,
    observedTimeToFirstTokenMs: null,
    ...patch,
  };
}

const chat: RoutingRequest = { taskClass: "chat", requiresVision: false, requiresTools: false };

describe("matchPolicy", () => {
  it("takes the first enabled policy covering the class, so list order is precedence", () => {
    const first = policy({ id: "a", taskClasses: ["chat"] });
    const second = policy({ id: "b", taskClasses: ["chat"] });
    expect(matchPolicy([first, second], "chat")?.id).toBe("a");
    // Reordering is the only thing needed to change which one wins.
    expect(matchPolicy([second, first], "chat")?.id).toBe("b");
  });

  it("skips disabled policies and non-matching classes", () => {
    expect(matchPolicy([policy({ enabled: false })], "chat")).toBeNull();
    expect(matchPolicy([policy({ taskClasses: ["summarize"] })], "chat")).toBeNull();
    // An empty class list means every class.
    expect(matchPolicy([policy({ taskClasses: [] })], "chat")?.id).toBe("p1");
  });
});

describe("routeRequest", () => {
  it("leaves dispatch alone when no policy is enabled", () => {
    const decision = routeRequest([], [candidate({ key: "provider:x" })], chat, "provider:x");
    expect(decision.policyId).toBeNull();
    expect(decision.chosenKey).toBeNull();
    expect(decision.changedFromActive).toBe(false);
    expect(decision.sequence).toEqual([]);
  });

  it("prefers the policy's pinned target over a cheaper one", () => {
    const decision = routeRequest(
      [policy({ preferredTargetKeys: ["provider:pinned"] })],
      [
        candidate({ key: "provider:cheap", outputPerMillionUsd: 1 }),
        candidate({ key: "provider:pinned", outputPerMillionUsd: 50 }),
      ],
      chat,
      "provider:cheap",
    );
    expect(decision.chosenKey).toBe("provider:pinned");
    expect(decision.changedFromActive).toBe(true);
    // Everything else that qualifies still follows, as failover order.
    expect(decision.sequence).toEqual(["provider:pinned", "provider:cheap"]);
  });

  it("keeps the active target when it already satisfies the policy", () => {
    const decision = routeRequest(
      [policy({ maxOutputPerMillionUsd: 10 })],
      [
        candidate({ key: "provider:a", outputPerMillionUsd: 5 }),
        candidate({ key: "provider:b", outputPerMillionUsd: 1 }),
      ],
      chat,
      "provider:a",
    );
    expect(decision.chosenKey).toBe("provider:a");
    // No transcript note should fire for a policy that changed nothing.
    expect(decision.changedFromActive).toBe(false);
  });

  it("excludes candidates over a rate ceiling and records why", () => {
    const decision = routeRequest(
      [policy({ maxOutputPerMillionUsd: 10 })],
      [
        candidate({ key: "provider:pricey", outputPerMillionUsd: 75 }),
        candidate({ key: "provider:ok", outputPerMillionUsd: 2 }),
      ],
      chat,
      "provider:pricey",
    );
    expect(decision.chosenKey).toBe("provider:ok");
    expect(decision.rejected).toEqual([
      { key: "provider:pricey", reason: "output rate $75/M is over the $10/M ceiling" },
    ]);
  });

  it("treats a local target as free rather than unrated under a cost ceiling", () => {
    const decision = routeRequest(
      [policy({ maxOutputPerMillionUsd: 1 })],
      [
        candidate({ key: "ollama:llama", isLocal: true, inputPerMillionUsd: 0, outputPerMillionUsd: 0 }),
        candidate({ key: "provider:unrated" }),
      ],
      chat,
      "provider:unrated",
    );
    expect(decision.chosenKey).toBe("ollama:llama");
    expect(decision.rejected).toEqual([
      { key: "provider:unrated", reason: "has no output rate configured" },
    ]);
  });

  it("keeps a turn with an image away from a model that cannot see it", () => {
    const decision = routeRequest(
      [policy({ preferredTargetKeys: ["provider:blind"] })],
      [
        candidate({ key: "provider:blind", vision: "no" }),
        candidate({ key: "provider:sighted", vision: "yes" }),
      ],
      { taskClass: "chat", requiresVision: true, requiresTools: false },
      "provider:blind",
    );
    // A pin is a preference; the turn's own hard requirement still wins.
    expect(decision.chosenKey).toBe("provider:sighted");
  });

  it("does not reject an unknown capability, only an explicit no", () => {
    // Every provider model reports `unknown` tool-calling, so treating unknown
    // as "cannot" would make `requiresTools` match nothing at all.
    const decision = routeRequest(
      [policy({ requiresTools: true })],
      [
        candidate({ key: "provider:unknown", toolCalling: "unknown" }),
        candidate({ key: "ollama:no-tools", toolCalling: "no" }),
      ],
      chat,
      null,
    );
    expect(decision.chosenKey).toBe("provider:unknown");
    expect(decision.rejected).toEqual([{ key: "ollama:no-tools", reason: "cannot call tools" }]);
  });

  it("restricts a local-only policy to targets that stay on the machine", () => {
    const decision = routeRequest(
      [policy({ sensitivity: "local_only" })],
      [
        candidate({ key: "provider:cloud", outputPerMillionUsd: 0 }),
        candidate({ key: "ollama:local", isLocal: true, outputPerMillionUsd: 0 }),
      ],
      chat,
      "provider:cloud",
    );
    expect(decision.chosenKey).toBe("ollama:local");
    expect(decision.rejected[0].reason).toContain("local-only");
  });

  it("excludes a measured-too-slow target but never an unmeasured one", () => {
    const decision = routeRequest(
      [policy({ maxTimeToFirstTokenMs: 1000 })],
      [
        candidate({ key: "provider:slow", observedTimeToFirstTokenMs: 4000 }),
        candidate({ key: "provider:unmeasured", observedTimeToFirstTokenMs: null }),
      ],
      chat,
      null,
    );
    // "Not measured" is not "too slow" — this app does not act on a latency
    // number it never took.
    expect(decision.chosenKey).toBe("provider:unmeasured");
    expect(decision.rejected).toEqual([
      {
        key: "provider:slow",
        reason: "measured 4000ms to first token, over the 1000ms target",
      },
    ]);
  });

  it("ranks a measured-fast target ahead of an unmeasured one when latency matters", () => {
    const decision = routeRequest(
      [policy({ maxTimeToFirstTokenMs: 5000 })],
      [
        candidate({ key: "provider:unmeasured", observedTimeToFirstTokenMs: null }),
        candidate({ key: "provider:fast", observedTimeToFirstTokenMs: 200 }),
      ],
      chat,
      null,
    );
    expect(decision.sequence).toEqual(["provider:fast", "provider:unmeasured"]);
  });

  it("keeps the active target and says so when a matched policy excludes everything", () => {
    const decision = routeRequest(
      [policy({ name: "Local only", sensitivity: "local_only" })],
      [candidate({ key: "provider:cloud" })],
      chat,
      "provider:cloud",
    );
    // A policy is allowed to be unsatisfiable; it is not allowed to leave the
    // turn with nowhere to run.
    expect(decision.policyId).toBe("p1");
    expect(decision.chosenKey).toBeNull();
    expect(decision.changedFromActive).toBe(false);
    expect(decision.reason).toContain("no configured model satisfies it");
  });

  it("never returns a key that was not offered as a candidate", () => {
    const decision = routeRequest(
      [policy({ preferredTargetKeys: ["provider:not-configured"] })],
      [candidate({ key: "provider:real" })],
      chat,
      "provider:real",
    );
    expect(decision.sequence).toEqual(["provider:real"]);
  });

  it("skips unavailable targets", () => {
    const decision = routeRequest(
      [policy()],
      [
        candidate({ key: "ollama:offline", available: false }),
        candidate({ key: "provider:up" }),
      ],
      chat,
      null,
    );
    expect(decision.chosenKey).toBe("provider:up");
    expect(decision.rejected).toEqual([
      { key: "ollama:offline", reason: "not available right now" },
    ]);
  });

  it("orders deterministically when every ranking signal ties", () => {
    const tie = [candidate({ key: "provider:b" }), candidate({ key: "provider:a" })];
    expect(routeRequest([policy()], tie, chat, null).sequence).toEqual([
      "provider:a",
      "provider:b",
    ]);
  });
});

describe("observedTimeToFirstTokenMs", () => {
  it("returns null when nothing was measured for this target", () => {
    expect(observedTimeToFirstTokenMs([], "provider:a")).toBeNull();
    expect(
      observedTimeToFirstTokenMs(
        [{ targetKey: "provider:a", timeToFirstTokenMs: null }],
        "provider:a",
      ),
    ).toBeNull();
    // Entries written before latency was recorded carry no field at all.
    expect(observedTimeToFirstTokenMs([{ targetKey: "provider:a" }], "provider:a")).toBeNull();
  });

  it("takes the median so one cold start cannot disqualify a target", () => {
    const samples = [
      { targetKey: "provider:a", timeToFirstTokenMs: 100 },
      { targetKey: "provider:a", timeToFirstTokenMs: 120 },
      { targetKey: "provider:a", timeToFirstTokenMs: 9000 },
    ];
    expect(observedTimeToFirstTokenMs(samples, "provider:a")).toBe(120);
  });

  it("averages the middle pair for an even sample count", () => {
    const samples = [
      { targetKey: "provider:a", timeToFirstTokenMs: 100 },
      { targetKey: "provider:a", timeToFirstTokenMs: 200 },
    ];
    expect(observedTimeToFirstTokenMs(samples, "provider:a")).toBe(150);
  });

  it("ignores other targets and only reads the most recent samples", () => {
    const samples = [
      { targetKey: "provider:a", timeToFirstTokenMs: 5000 },
      { targetKey: "provider:b", timeToFirstTokenMs: 1 },
      { targetKey: "provider:a", timeToFirstTokenMs: 10 },
      { targetKey: "provider:a", timeToFirstTokenMs: 20 },
    ];
    expect(observedTimeToFirstTokenMs(samples, "provider:a", 2)).toBe(15);
  });
});
