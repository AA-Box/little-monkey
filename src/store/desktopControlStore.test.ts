import { beforeEach, describe, expect, it, vi } from "vitest";

const invokeMock = vi.fn();
// See mcpStore.test.ts's comment on why the captured handlers must be
// stashed via `vi.hoisted` rather than a plain outer-scope variable:
// `vi.mock` factories are hoisted above this file's other statements, so a
// normal `let` closed over by the factory is a different binding than the
// one the test bodies below read.
const handlers = vi.hoisted(() => ({
  pending: null as ((event: { payload: unknown }) => void) | null,
  emergencyStop: null as ((event: { payload: unknown }) => void) | null,
  sessionState: null as ((event: { payload: unknown }) => void) | null,
}));
vi.mock("@tauri-apps/api/core", () => ({ invoke: (...args: unknown[]) => invokeMock(...args), isTauri: () => true }));
vi.mock("@tauri-apps/api/event", () => ({
  listen: (name: string, handler: (event: { payload: unknown }) => void) => {
    if (name === "desktop-control://action-pending") handlers.pending = handler;
    if (name === "desktop-control://emergency-stop") handlers.emergencyStop = handler;
    if (name === "desktop-control://session-state") handlers.sessionState = handler;
    return Promise.resolve(() => {});
  },
}));

import {
  useDesktopControlStore,
  type ControlSession,
  type PendingActionSummary,
} from "./desktopControlStore";

function makeSession(overrides: Partial<ControlSession> = {}): ControlSession {
  return {
    sessionId: "desktop-control-1",
    allowedApplications: ["Notes"],
    allowedWindows: [],
    createdAtMs: 0,
    expiresAtMs: 60_000,
    active: true,
    indicatorVisible: true,
    approvedBatch: false,
    paused: false,
    approvalPolicy: "per_action",
    allowScreenshots: true,
    allowKeyboardInput: true,
    allowClipboardRead: false,
    ...overrides,
  };
}

function makePending(overrides: Partial<PendingActionSummary> = {}): PendingActionSummary {
  return {
    actionId: "control-action-1",
    sessionId: "desktop-control-1",
    targetApplicationId: "Notes",
    approvalLevel: "high",
    action: { kind: "key_press", key: "a" },
    ...overrides,
  };
}

beforeEach(() => {
  invokeMock.mockReset();
  useDesktopControlStore.setState({ sessions: [], pendingActions: [], error: null });
});

describe("desktopControlStore.refreshSessions", () => {
  it("loads sessions from the backend", async () => {
    const session = makeSession();
    invokeMock.mockResolvedValueOnce([session]);

    await useDesktopControlStore.getState().refreshSessions();

    expect(invokeMock).toHaveBeenCalledWith("desktop_control_sessions");
    expect(useDesktopControlStore.getState().sessions).toEqual([session]);
    expect(useDesktopControlStore.getState().error).toBeNull();
  });

  it("records the error text and leaves sessions alone on failure", async () => {
    invokeMock.mockRejectedValueOnce(new Error("backend unavailable"));

    await useDesktopControlStore.getState().refreshSessions();

    expect(useDesktopControlStore.getState().error).toBe("backend unavailable");
  });
});

describe("desktopControlStore.startSession", () => {
  it("passes the allowlist/lifetime/approvedBatch through and stores the returned session", async () => {
    const session = makeSession({ approvedBatch: true });
    invokeMock.mockResolvedValueOnce(session);

    const result = await useDesktopControlStore.getState().startSession(["Notes"], 60_000, true);

    expect(invokeMock).toHaveBeenCalledWith("desktop_control_start_session", {
      allowedApplications: ["Notes"],
      allowedWindows: [],
      lifetimeMs: 60_000,
      approvedBatch: true,
      allowScreenshots: true,
      allowKeyboardInput: true,
      allowClipboardRead: false,
    });
    expect(result).toEqual(session);
    expect(useDesktopControlStore.getState().sessions).toEqual([session]);
  });

  it("propagates a bypass-mode rejection and records the error", async () => {
    invokeMock.mockRejectedValueOnce(new Error("Safe Desktop Control can never be started while permission mode is bypass"));

    await expect(useDesktopControlStore.getState().startSession(["Notes"], 60_000, false)).rejects.toThrow("bypass");
    expect(useDesktopControlStore.getState().error).toContain("bypass");
    expect(useDesktopControlStore.getState().sessions).toEqual([]);
  });

  it("replaces an existing entry for the same session id rather than duplicating it", async () => {
    const first = makeSession({ active: true });
    const second = { ...first, active: false };
    invokeMock.mockResolvedValueOnce(first).mockResolvedValueOnce(second);

    await useDesktopControlStore.getState().startSession(["Notes"], 60_000, false);
    await useDesktopControlStore.getState().startSession(["Notes"], 60_000, false);

    expect(useDesktopControlStore.getState().sessions).toEqual([second]);
  });
});

describe("desktopControlStore.stopSession", () => {
  it("marks the session inactive locally and drops its pending actions", async () => {
    const session = makeSession();
    const pending = makePending();
    useDesktopControlStore.setState({ sessions: [session], pendingActions: [pending] });
    invokeMock.mockResolvedValueOnce(true);

    const stopped = await useDesktopControlStore.getState().stopSession(session.sessionId);

    expect(invokeMock).toHaveBeenCalledWith("desktop_control_stop_session", { sessionId: session.sessionId });
    expect(stopped).toBe(true);
    expect(useDesktopControlStore.getState().sessions).toEqual([{ ...session, active: false }]);
    expect(useDesktopControlStore.getState().pendingActions).toEqual([]);
  });

  it("leaves another session's pending actions untouched", async () => {
    const pendingOther = makePending({ actionId: "control-action-other", sessionId: "desktop-control-other" });
    useDesktopControlStore.setState({
      sessions: [makeSession()],
      pendingActions: [makePending(), pendingOther],
    });
    invokeMock.mockResolvedValueOnce(true);

    await useDesktopControlStore.getState().stopSession("desktop-control-1");

    expect(useDesktopControlStore.getState().pendingActions).toEqual([pendingOther]);
  });
});

describe("desktopControlStore.requestAction", () => {
  it("invokes with the exact session/target/action shape and returns the outcome", async () => {
    const outcome = { actionId: "control-action-1", executed: true };
    invokeMock.mockResolvedValueOnce(outcome);
    const action = { kind: "mouse_move" as const, x: 10, y: 20 };

    const result = await useDesktopControlStore.getState().requestAction("desktop-control-1", "Notes", action);

    expect(invokeMock).toHaveBeenCalledWith("desktop_control_request_action", {
      sessionId: "desktop-control-1",
      targetApplicationId: "Notes",
      action,
    });
    expect(result).toEqual(outcome);
  });

  it("records the error and rejects when the backend denies the action", async () => {
    invokeMock.mockRejectedValueOnce(new Error("Control action was denied"));

    await expect(
      useDesktopControlStore.getState().requestAction("desktop-control-1", "Notes", { kind: "key_press", key: "a" }),
    ).rejects.toThrow("denied");
    expect(useDesktopControlStore.getState().error).toContain("denied");
  });
});

describe("desktopControlStore.respondAction", () => {
  it("invokes with the action id/approve flag and removes the entry from pendingActions", async () => {
    useDesktopControlStore.setState({ pendingActions: [makePending()] });
    invokeMock.mockResolvedValueOnce(undefined);

    await useDesktopControlStore.getState().respondAction("control-action-1", true);

    expect(invokeMock).toHaveBeenCalledWith("desktop_control_respond_action", { actionId: "control-action-1", approve: true });
    expect(useDesktopControlStore.getState().pendingActions).toEqual([]);
  });

  it("still removes the entry from pendingActions even if the backend call rejects", async () => {
    useDesktopControlStore.setState({ pendingActions: [makePending()] });
    invokeMock.mockRejectedValueOnce(new Error("No pending control action with id control-action-1"));

    await expect(useDesktopControlStore.getState().respondAction("control-action-1", false)).rejects.toThrow();
    expect(useDesktopControlStore.getState().pendingActions).toEqual([]);
  });
});

describe("desktopControlStore.emergencyStop", () => {
  it("clears every pending action, marks every session inactive, and re-fetches sessions", async () => {
    const session = makeSession();
    useDesktopControlStore.setState({ sessions: [session], pendingActions: [makePending()] });
    invokeMock
      .mockResolvedValueOnce({ sessionsDeactivated: 1, actionsCancelled: 1 })
      .mockResolvedValueOnce([{ ...session, active: false }]);

    const result = await useDesktopControlStore.getState().emergencyStop();

    expect(invokeMock).toHaveBeenCalledWith("desktop_control_emergency_stop");
    expect(result).toEqual({ sessionsDeactivated: 1, actionsCancelled: 1 });
    expect(useDesktopControlStore.getState().pendingActions).toEqual([]);
    expect(useDesktopControlStore.getState().sessions.every((s) => !s.active)).toBe(true);
  });

  it("is safe to call when nothing is active", async () => {
    invokeMock.mockResolvedValueOnce({ sessionsDeactivated: 0, actionsCancelled: 0 }).mockResolvedValueOnce([]);

    const result = await useDesktopControlStore.getState().emergencyStop();

    expect(result).toEqual({ sessionsDeactivated: 0, actionsCancelled: 0 });
  });
});

describe("desktopControlStore.clearError", () => {
  it("resets error to null", () => {
    useDesktopControlStore.setState({ error: "boom" });
    useDesktopControlStore.getState().clearError();
    expect(useDesktopControlStore.getState().error).toBeNull();
  });
});

describe("desktopControlStore event listeners", () => {
  it("queues a newly-arrived pending action from the desktop-control://action-pending event", () => {
    expect(handlers.pending).not.toBeNull();
    const pending = makePending();

    handlers.pending?.({ payload: pending });

    expect(useDesktopControlStore.getState().pendingActions).toEqual([pending]);
  });

  it("does not duplicate an already-queued pending action id", () => {
    const pending = makePending();
    useDesktopControlStore.setState({ pendingActions: [pending] });

    handlers.pending?.({ payload: pending });

    expect(useDesktopControlStore.getState().pendingActions).toEqual([pending]);
  });

  it("clears pendingActions and deactivates sessions on the emergency-stop event", () => {
    expect(handlers.emergencyStop).not.toBeNull();
    useDesktopControlStore.setState({
      sessions: [makeSession()],
      pendingActions: [makePending()],
    });

    handlers.emergencyStop?.({ payload: { sessionsDeactivated: 1, actionsCancelled: 1 } });

    expect(useDesktopControlStore.getState().pendingActions).toEqual([]);
    expect(useDesktopControlStore.getState().sessions.every((s) => !s.active)).toBe(true);
  });
});
