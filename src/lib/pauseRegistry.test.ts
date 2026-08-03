import { afterEach, describe, expect, it, vi } from "vitest";

const markProcessRunningMock = vi.fn();
const markProcessSuspendedMock = vi.fn();
vi.mock("./processTable", () => ({
  markProcessRunning: (...args: unknown[]) => markProcessRunningMock(...args),
  markProcessSuspended: (...args: unknown[]) => markProcessSuspendedMock(...args),
}));

import {
  clearPauseRegistryForTests,
  forgetPause,
  honourPause,
  isPauseRequested,
  setPauseRequested,
  waitWhileRequested,
} from "./pauseRegistry";

afterEach(() => {
  clearPauseRegistryForTests();
  markProcessRunningMock.mockReset();
  markProcessSuspendedMock.mockReset();
});

describe("pause registry", () => {
  it("is not paused for an unknown key", () => {
    expect(isPauseRequested("unknown")).toBe(false);
  });

  it("setPauseRequested flips the latch and is idempotent", () => {
    setPauseRequested("turn-a", true);
    expect(isPauseRequested("turn-a")).toBe(true);
    setPauseRequested("turn-a", true);
    expect(isPauseRequested("turn-a")).toBe(true);
    setPauseRequested("turn-a", false);
    expect(isPauseRequested("turn-a")).toBe(false);
  });

  it("forgetPause drops the latch entirely", () => {
    setPauseRequested("turn-a", true);
    forgetPause("turn-a");
    expect(isPauseRequested("turn-a")).toBe(false);
  });

  it("forgetPause releases a waiter parked on the latch it drops", async () => {
    // Deleting the map entry does not reach a waiter: `waitWhileRequested`
    // holds the entry OBJECT and is subscribed to that object's listener set.
    // Without clearing first, teardown would strand a parked loop forever.
    setPauseRequested("turn-a", true);
    const controller = new AbortController();
    let resolved = false;
    const waiter = waitWhileRequested("turn-a", controller.signal).then(() => {
      resolved = true;
    });
    await Promise.resolve();
    expect(resolved).toBe(false);

    forgetPause("turn-a");
    await waiter;
    expect(resolved).toBe(true);
  });

  describe("waitWhileRequested", () => {
    it("resolves immediately when the key isn't paused", async () => {
      const controller = new AbortController();
      await expect(waitWhileRequested("turn-a", controller.signal)).resolves.toBeUndefined();
    });

    it("blocks while paused and resolves once the latch clears", async () => {
      setPauseRequested("turn-a", true);
      let resolved = false;
      const controller = new AbortController();
      const waiter = waitWhileRequested("turn-a", controller.signal).then(() => {
        resolved = true;
      });

      await Promise.resolve();
      expect(resolved).toBe(false);

      setPauseRequested("turn-a", false);
      await waiter;
      expect(resolved).toBe(true);
    });

    it("resolves early on abort without waiting for the latch to clear", async () => {
      setPauseRequested("turn-a", true);
      const controller = new AbortController();
      const waiter = waitWhileRequested("turn-a", controller.signal);
      controller.abort();
      await expect(waiter).resolves.toBeUndefined();
      // The latch itself is untouched by an abort — only resume clears it.
      expect(isPauseRequested("turn-a")).toBe(true);
    });

    it("resolves immediately when the signal is already aborted", async () => {
      setPauseRequested("turn-a", true);
      const controller = new AbortController();
      controller.abort();
      await expect(waitWhileRequested("turn-a", controller.signal)).resolves.toBeUndefined();
    });
  });

  describe("honourPause", () => {
    it("does nothing when the key isn't paused", async () => {
      const controller = new AbortController();
      await honourPause("turn-a", "process-1", controller.signal);
      expect(markProcessSuspendedMock).not.toHaveBeenCalled();
      expect(markProcessRunningMock).not.toHaveBeenCalled();
    });

    it("does nothing when the signal is already aborted, even if paused", async () => {
      setPauseRequested("turn-a", true);
      const controller = new AbortController();
      controller.abort();
      await honourPause("turn-a", "process-1", controller.signal);
      expect(markProcessSuspendedMock).not.toHaveBeenCalled();
      expect(markProcessRunningMock).not.toHaveBeenCalled();
    });

    it("marks suspended, waits, then marks running again once resumed — in that order", async () => {
      setPauseRequested("turn-a", true);
      const controller = new AbortController();
      const order: string[] = [];
      markProcessSuspendedMock.mockImplementation(() => order.push("suspended"));
      markProcessRunningMock.mockImplementation(() => order.push("running"));

      const call = honourPause("turn-a", "process-1", controller.signal);
      await vi.waitFor(() => expect(markProcessSuspendedMock).toHaveBeenCalledWith("process-1"));
      expect(markProcessRunningMock).not.toHaveBeenCalled();

      setPauseRequested("turn-a", false);
      await call;

      expect(markProcessRunningMock).toHaveBeenCalledWith("process-1");
      expect(order).toEqual(["suspended", "running"]);
    });

    it("resolves a promised process id before marking suspended", async () => {
      setPauseRequested("turn-a", true);
      const controller = new AbortController();
      const processIdPromise = Promise.resolve("process-async");

      const call = honourPause("turn-a", processIdPromise, controller.signal);
      await vi.waitFor(() => expect(markProcessSuspendedMock).toHaveBeenCalledWith("process-async"));

      setPauseRequested("turn-a", false);
      await call;
      expect(markProcessRunningMock).toHaveBeenCalledWith("process-async");
    });

    it("skips both process-table calls when the process id is null", async () => {
      setPauseRequested("turn-a", true);
      const controller = new AbortController();
      const call = honourPause("turn-a", null, controller.signal);
      setPauseRequested("turn-a", false);
      await call;
      expect(markProcessSuspendedMock).not.toHaveBeenCalled();
      expect(markProcessRunningMock).not.toHaveBeenCalled();
    });

    it("does not mark running again if aborted while parked", async () => {
      setPauseRequested("turn-a", true);
      const controller = new AbortController();
      const call = honourPause("turn-a", "process-1", controller.signal);
      await vi.waitFor(() => expect(markProcessSuspendedMock).toHaveBeenCalledWith("process-1"));

      controller.abort();
      await call;
      expect(markProcessRunningMock).not.toHaveBeenCalled();
    });
  });
});
