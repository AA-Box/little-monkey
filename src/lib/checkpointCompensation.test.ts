/**
 * The compensating half of revert that Rust cannot run, and the ordering that
 * makes it correct.
 */
import { beforeEach, describe, expect, it, vi } from "vitest";

const invokeMock = vi.fn();
vi.mock("@tauri-apps/api/core", () => ({
  invoke: (...args: unknown[]) => invokeMock(...args),
  isTauri: () => true,
}));

import { reapplyCheckpoint, revertCheckpoint } from "./checkpointCompensation";
import { useTaskSuggestionStore } from "../store/taskSuggestionStore";

function stage(title: string): string {
  return useTaskSuggestionStore
    .getState()
    .create({ sessionId: "session-1", title, tldr: "", prompt: "do the thing" }).id;
}

function statusOf(id: string): string | undefined {
  return useTaskSuggestionStore.getState().suggestions[id]?.status;
}

beforeEach(() => {
  invokeMock.mockReset();
  useTaskSuggestionStore.setState({ suggestions: {}, order: [] });
});

describe("revertCheckpoint", () => {
  it("withdraws the chips the reverted turn proposed", async () => {
    const staged = stage("Fix stale README badge");
    const untouched = stage("From another turn entirely");
    invokeMock.mockImplementation(async (command: string) =>
      command === "checkpoint_staged_task_suggestions" ? [staged] : undefined,
    );

    await revertCheckpoint("cp-1");

    expect(statusOf(staged)).toBe("dismissed");
    expect(statusOf(untouched)).toBe("pending");
  });

  /**
   * The read has to come first. `checkpoint_revert` rewrites the manifest, so a
   * caller reading it afterwards could find the list it needs already changed —
   * the same ordering `forget_remembered` follows on the Rust side.
   */
  it("reads the staged list before reverting, not after", async () => {
    const order: string[] = [];
    invokeMock.mockImplementation(async (command: string) => {
      order.push(command);
      return command === "checkpoint_staged_task_suggestions" ? [] : undefined;
    });

    await revertCheckpoint("cp-1");

    expect(order).toEqual(["checkpoint_staged_task_suggestions", "checkpoint_revert"]);
  });

  /** An older manifest records no ids. Empty means *unrecorded*, so it
   * withdraws nothing rather than guessing at which chips were this turn's. */
  it("withdraws nothing when the manifest recorded no ids", async () => {
    const staged = stage("Predates the recording");
    invokeMock.mockImplementation(async (command: string) =>
      command === "checkpoint_staged_task_suggestions" ? [] : undefined,
    );

    await revertCheckpoint("cp-1");

    expect(statusOf(staged)).toBe("pending");
  });

  /** The revert itself is the operation; a failure to read the chip list must
   * not stop the files being restored. */
  it("still reverts when the staged list cannot be read", async () => {
    invokeMock.mockImplementation(async (command: string) => {
      if (command === "checkpoint_staged_task_suggestions") throw new Error("no manifest");
      return undefined;
    });

    await revertCheckpoint("cp-1");

    expect(invokeMock.mock.calls.map(([command]) => command)).toContain("checkpoint_revert");
  });
});

describe("reapplyCheckpoint", () => {
  /** An undo that cannot itself be undone is data loss with a friendly name. */
  it("puts the withdrawn chips back", async () => {
    const staged = stage("Fix stale README badge");
    invokeMock.mockImplementation(async (command: string) =>
      command === "checkpoint_staged_task_suggestions" ? [staged] : undefined,
    );

    await revertCheckpoint("cp-1");
    expect(statusOf(staged)).toBe("dismissed");

    await reapplyCheckpoint("cp-1");
    expect(statusOf(staged)).toBe("pending");
  });

  /** A chip the user already clicked spun off a real session. Reverting its
   * status would misreport work that actually happened. */
  it("never resurrects a chip the user already started", async () => {
    const staged = stage("Already spun off");
    useTaskSuggestionStore.getState().markStarted(staged, "session-2");
    invokeMock.mockImplementation(async (command: string) =>
      command === "checkpoint_staged_task_suggestions" ? [staged] : undefined,
    );

    await reapplyCheckpoint("cp-1");

    expect(statusOf(staged)).toBe("started");
  });
});
