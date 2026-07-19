import { beforeEach, describe, expect, it } from "vitest";

import {
  selectArchivedSideTasks,
  selectRunningSideTaskCount,
  selectVisibleSideTasks,
  useSideTaskStore,
  type SideTaskSource,
} from "./sideTaskStore";

const source: SideTaskSource = { kind: "chat_message", label: "Assistant message", excerpt: "explain the auth flow" };

function reset(): void {
  useSideTaskStore.setState({
    tasks: {},
    order: [],
    drawerOpen: false,
    selectedTaskId: null,
    composerSeed: null,
    composerOpen: false,
  });
}

beforeEach(reset);

describe("sideTaskStore / drawer + composer UI state", () => {
  it("openDrawer/closeDrawer/toggleDrawer flip drawerOpen", () => {
    expect(useSideTaskStore.getState().drawerOpen).toBe(false);
    useSideTaskStore.getState().openDrawer();
    expect(useSideTaskStore.getState().drawerOpen).toBe(true);
    useSideTaskStore.getState().closeDrawer();
    expect(useSideTaskStore.getState().drawerOpen).toBe(false);
    useSideTaskStore.getState().toggleDrawer();
    expect(useSideTaskStore.getState().drawerOpen).toBe(true);
  });

  it("openComposer stages a seed, opens the composer, and opens the drawer — mirrors browserWorkbenchStore's stage-then-consume shape", () => {
    useSideTaskStore.getState().openComposer({ title: "t", prompt: "p", profile: "explore", source, sessionId: "s1" });
    const state = useSideTaskStore.getState();
    expect(state.composerOpen).toBe(true);
    expect(state.drawerOpen).toBe(true);
    expect(state.composerSeed).toEqual({ title: "t", prompt: "p", profile: "explore", source, sessionId: "s1" });

    useSideTaskStore.getState().consumeComposerSeed();
    expect(useSideTaskStore.getState().composerSeed).toBeNull();
    // consuming the seed does not itself close the composer form.
    expect(useSideTaskStore.getState().composerOpen).toBe(true);

    useSideTaskStore.getState().closeComposer();
    expect(useSideTaskStore.getState().composerOpen).toBe(false);
  });
});

describe("sideTaskStore / lifecycle", () => {
  it("create seeds a queued task with a one-message transcript and selects it", () => {
    const record = useSideTaskStore.getState().create({
      title: "Explain auth",
      prompt: "Explain the auth flow",
      profile: "explore",
      source,
      sessionId: "s1",
      modelLabel: "local · llama",
    });

    expect(record.status).toBe("queued");
    expect(record.messages).toEqual([{ role: "user", content: "Explain the auth flow" }]);
    expect(record.retryOf).toBeNull();
    expect(record.archivedAt).toBeNull();
    expect(record.promotedAt).toBeNull();
    expect(record.turnId).not.toBe(record.id); // independent ids — see SideTaskRecord.turnId's doc comment
    expect(useSideTaskStore.getState().selectedTaskId).toBe(record.id);
    expect(useSideTaskStore.getState().order[0]).toBe(record.id);
  });

  it("two tasks created back to back get distinct ids and turnIds", () => {
    const a = useSideTaskStore.getState().create({ title: "A", prompt: "a", profile: "explore", source, sessionId: "s1", modelLabel: "m" });
    const b = useSideTaskStore.getState().create({ title: "B", prompt: "b", profile: "explore", source, sessionId: "s1", modelLabel: "m" });
    expect(a.id).not.toBe(b.id);
    expect(a.turnId).not.toBe(b.turnId);
    // newest first
    expect(useSideTaskStore.getState().order).toEqual([b.id, a.id]);
  });

  it("markRunning transitions status and stamps startedAt exactly once", () => {
    const record = useSideTaskStore.getState().create({ title: "A", prompt: "a", profile: "explore", source, sessionId: "s1", modelLabel: "m" });
    useSideTaskStore.getState().markRunning(record.id);
    const startedAt = useSideTaskStore.getState().tasks[record.id].startedAt;
    expect(useSideTaskStore.getState().tasks[record.id].status).toBe("running");
    expect(startedAt).not.toBeNull();

    useSideTaskStore.getState().markRunning(record.id);
    expect(useSideTaskStore.getState().tasks[record.id].startedAt).toBe(startedAt);
  });

  it("appendMessage grows the transcript without mutating the previous array", () => {
    const record = useSideTaskStore.getState().create({ title: "A", prompt: "a", profile: "explore", source, sessionId: "s1", modelLabel: "m" });
    const before = useSideTaskStore.getState().tasks[record.id].messages;
    useSideTaskStore.getState().appendMessage(record.id, { role: "assistant", content: "done" });
    const after = useSideTaskStore.getState().tasks[record.id].messages;
    expect(after).toEqual([{ role: "user", content: "a" }, { role: "assistant", content: "done" }]);
    expect(before).not.toBe(after);
  });

  it("recordToolProposed then recordToolFinished updates the same evidence row by tool_call id", () => {
    const record = useSideTaskStore.getState().create({ title: "A", prompt: "a", profile: "code", source, sessionId: "s1", modelLabel: "m" });
    useSideTaskStore.getState().recordToolProposed(record.id, {
      id: "call-1",
      name: "write_file",
      argsPreview: "path: a.ts",
      resultPreview: "",
      outcome: "pending",
      startedAt: 1,
      finishedAt: null,
    });
    expect(useSideTaskStore.getState().tasks[record.id].toolEvidence).toHaveLength(1);
    expect(useSideTaskStore.getState().tasks[record.id].toolEvidence[0].outcome).toBe("pending");

    useSideTaskStore.getState().recordToolFinished(record.id, "call-1", "succeeded", "ok");
    const evidence = useSideTaskStore.getState().tasks[record.id].toolEvidence[0];
    expect(evidence.outcome).toBe("succeeded");
    expect(evidence.resultPreview).toBe("ok");
    expect(evidence.finishedAt).not.toBeNull();
  });

  it("addUsage sums across multiple attempts, mirroring subagentStore's accumulateUsage", () => {
    const record = useSideTaskStore.getState().create({ title: "A", prompt: "a", profile: "explore", source, sessionId: "s1", modelLabel: "m" });
    useSideTaskStore.getState().addUsage(record.id, { promptTokens: 10, completionTokens: 5, totalTokens: 15 });
    useSideTaskStore.getState().addUsage(record.id, { promptTokens: 3, completionTokens: 1, totalTokens: 4 });
    expect(useSideTaskStore.getState().tasks[record.id].usage).toEqual({ promptTokens: 13, completionTokens: 6, totalTokens: 19 });
  });

  it("finish sets a terminal status, finalReport/error, and finishedAt", () => {
    const record = useSideTaskStore.getState().create({ title: "A", prompt: "a", profile: "explore", source, sessionId: "s1", modelLabel: "m" });
    useSideTaskStore.getState().finish(record.id, "completed", "All done.", null);
    const task = useSideTaskStore.getState().tasks[record.id];
    expect(task.status).toBe("completed");
    expect(task.finalReport).toBe("All done.");
    expect(task.error).toBeNull();
    expect(task.finishedAt).not.toBeNull();
  });

  it("pause only takes effect from running, and resume only from paused", () => {
    const record = useSideTaskStore.getState().create({ title: "A", prompt: "a", profile: "explore", source, sessionId: "s1", modelLabel: "m" });

    // Still queued — pause is a no-op.
    useSideTaskStore.getState().pause(record.id);
    expect(useSideTaskStore.getState().tasks[record.id].status).toBe("queued");

    useSideTaskStore.getState().markRunning(record.id);
    useSideTaskStore.getState().pause(record.id);
    expect(useSideTaskStore.getState().tasks[record.id].status).toBe("paused");

    // Already paused — resume flips it back.
    useSideTaskStore.getState().resume(record.id);
    expect(useSideTaskStore.getState().tasks[record.id].status).toBe("running");

    // Not paused — resume is a no-op.
    useSideTaskStore.getState().resume(record.id);
    expect(useSideTaskStore.getState().tasks[record.id].status).toBe("running");
  });

  it("archive/unarchive toggle archivedAt without touching status", () => {
    const record = useSideTaskStore.getState().create({ title: "A", prompt: "a", profile: "explore", source, sessionId: "s1", modelLabel: "m" });
    useSideTaskStore.getState().finish(record.id, "completed", "done", null);

    useSideTaskStore.getState().archive(record.id);
    expect(useSideTaskStore.getState().tasks[record.id].archivedAt).not.toBeNull();
    expect(useSideTaskStore.getState().tasks[record.id].status).toBe("completed");

    useSideTaskStore.getState().unarchive(record.id);
    expect(useSideTaskStore.getState().tasks[record.id].archivedAt).toBeNull();
  });

  it("markPromoted stamps promotedAt", () => {
    const record = useSideTaskStore.getState().create({ title: "A", prompt: "a", profile: "explore", source, sessionId: "s1", modelLabel: "m" });
    expect(useSideTaskStore.getState().tasks[record.id].promotedAt).toBeNull();
    useSideTaskStore.getState().markPromoted(record.id);
    expect(useSideTaskStore.getState().tasks[record.id].promotedAt).not.toBeNull();
  });

  it("remove drops the task and clears selection only if it was selected", () => {
    const a = useSideTaskStore.getState().create({ title: "A", prompt: "a", profile: "explore", source, sessionId: "s1", modelLabel: "m" });
    const b = useSideTaskStore.getState().create({ title: "B", prompt: "b", profile: "explore", source, sessionId: "s1", modelLabel: "m" });
    useSideTaskStore.getState().selectTask(a.id);

    useSideTaskStore.getState().remove(b.id);
    expect(useSideTaskStore.getState().tasks[b.id]).toBeUndefined();
    expect(useSideTaskStore.getState().selectedTaskId).toBe(a.id);

    useSideTaskStore.getState().remove(a.id);
    expect(useSideTaskStore.getState().selectedTaskId).toBeNull();
  });

  it("every mutator no-ops for an unregistered task id", () => {
    useSideTaskStore.getState().markRunning("missing");
    useSideTaskStore.getState().appendMessage("missing", { role: "user", content: "x" });
    useSideTaskStore.getState().recordToolProposed("missing", {
      id: "c",
      name: "read_file",
      argsPreview: "",
      resultPreview: "",
      outcome: "pending",
      startedAt: 0,
      finishedAt: null,
    });
    useSideTaskStore.getState().recordToolFinished("missing", "c", "succeeded", "ok");
    useSideTaskStore.getState().addUsage("missing", { promptTokens: 1, completionTokens: 1, totalTokens: 2 });
    useSideTaskStore.getState().finish("missing", "completed", "x", null);
    useSideTaskStore.getState().archive("missing");
    useSideTaskStore.getState().unarchive("missing");
    useSideTaskStore.getState().markPromoted("missing");
    expect(useSideTaskStore.getState().tasks).toEqual({});
  });
});

describe("sideTaskStore / selectors", () => {
  it("selectVisibleSideTasks excludes archived tasks; selectArchivedSideTasks includes only them", () => {
    const a = useSideTaskStore.getState().create({ title: "A", prompt: "a", profile: "explore", source, sessionId: "s1", modelLabel: "m" });
    const b = useSideTaskStore.getState().create({ title: "B", prompt: "b", profile: "explore", source, sessionId: "s1", modelLabel: "m" });
    useSideTaskStore.getState().finish(b.id, "completed", "done", null);
    useSideTaskStore.getState().archive(b.id);

    const visible = selectVisibleSideTasks(useSideTaskStore.getState());
    const archived = selectArchivedSideTasks(useSideTaskStore.getState());
    expect(visible.map((t) => t.id)).toEqual([a.id]);
    expect(archived.map((t) => t.id)).toEqual([b.id]);
  });

  it("selectRunningSideTaskCount counts queued/running/paused tasks, not finished ones", () => {
    const a = useSideTaskStore.getState().create({ title: "A", prompt: "a", profile: "explore", source, sessionId: "s1", modelLabel: "m" });
    const b = useSideTaskStore.getState().create({ title: "B", prompt: "b", profile: "explore", source, sessionId: "s1", modelLabel: "m" });
    const c = useSideTaskStore.getState().create({ title: "C", prompt: "c", profile: "explore", source, sessionId: "s1", modelLabel: "m" });
    const d = useSideTaskStore.getState().create({ title: "D", prompt: "d", profile: "explore", source, sessionId: "s1", modelLabel: "m" });
    useSideTaskStore.getState().markRunning(a.id);
    useSideTaskStore.getState().markRunning(b.id);
    useSideTaskStore.getState().finish(b.id, "completed", "done", null);
    useSideTaskStore.getState().markRunning(d.id);
    useSideTaskStore.getState().pause(d.id);
    // c stays "queued".

    expect(selectRunningSideTaskCount(useSideTaskStore.getState())).toBe(3); // a (running) + c (queued) + d (paused)
    expect(useSideTaskStore.getState().tasks[c.id].status).toBe("queued");
    expect(useSideTaskStore.getState().tasks[d.id].status).toBe("paused");
  });
});
