import { describe, expect, it } from "vitest";

import { formatPlanNotice } from "../../lib/agentLoop";
import type { ChatSession } from "../../store/sessionStore";
import { sessionStatus } from "./sessionStatus";

function makeSession(overrides: Partial<ChatSession> = {}): ChatSession {
  const now = Date.now();
  return {
    id: "s1",
    title: "session",
    messages: [],
    createdAt: now,
    updatedAt: now,
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
    ...overrides,
  };
}

function planMessage(status: "proposed" | "approved" | "dismissed") {
  return {
    role: "system" as const,
    content: formatPlanNotice({ id: "p1", title: "Plan", plan: "steps", status }),
  };
}

describe("sessionStatus", () => {
  it("says nothing for an idle, read session", () => {
    expect(sessionStatus(makeSession(), false, undefined)).toBeNull();
  });

  it("reports a running turn over a stale outcome", () => {
    expect(sessionStatus(makeSession(), true, "error")).toBe("working");
  });

  it("reports how a finished turn ended", () => {
    expect(sessionStatus(makeSession(), false, "done")).toBe("finished");
    expect(sessionStatus(makeSession(), false, "error")).toBe("error");
  });

  it("treats a hand-marked unread session as finished", () => {
    expect(sessionStatus(makeSession({ unread: true }), false, undefined)).toBe("finished");
  });

  it("flags a plan still waiting on the user, ahead of the turn outcome", () => {
    const session = makeSession({ messages: [{ role: "user", content: "hi" }, planMessage("proposed")] });
    expect(sessionStatus(session, false, "done")).toBe("attention");
  });

  it("drops the flag once the plan is acted on", () => {
    const session = makeSession({ messages: [{ role: "user", content: "hi" }, planMessage("approved")] });
    expect(sessionStatus(session, false, undefined)).toBeNull();
  });

  it("ignores a proposed plan the conversation has moved past", () => {
    const session = makeSession({
      messages: [planMessage("proposed"), { role: "user", content: "never mind" }, { role: "assistant", content: "ok" }],
    });
    expect(sessionStatus(session, false, undefined)).toBeNull();
  });
});
