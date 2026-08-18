import { beforeEach, describe, expect, it, vi } from "vitest";

vi.mock("@tauri-apps/api/core", () => ({ invoke: vi.fn(async () => []), isTauri: () => false }));
vi.mock("@tauri-apps/api/event", () => ({ listen: vi.fn(async () => () => {}) }));

const runMock = vi.fn();
const cancelMock = vi.fn(async (_jobId: string) => true);
const unloadMock = vi.fn(async () => {});
vi.mock("../lib/studioClient", async (importOriginal) => ({
  ...(await importOriginal<typeof import("../lib/studioClient")>()),
  studioClient: {
    run: (...args: unknown[]) => runMock(...args),
    cancel: (jobId: string) => cancelMock(jobId),
    unloadEngine: () => unloadMock(),
    onProgress: vi.fn(async () => () => {}),
  },
}));

import { MAX_STUDIO_QUEUE, useStudioRunStore } from "./studioRunStore";
import type { GenerationRequest } from "../lib/studioClient";

const request = { modelId: "m", prompt: "p" } as unknown as GenerationRequest;

/** Resolves once every queued microtask/promise chain has run. */
const settle = () => new Promise((resolve) => setTimeout(resolve, 0));

describe("studioRunStore", () => {
  beforeEach(() => {
    runMock.mockReset();
    cancelMock.mockClear();
    unloadMock.mockClear();
    useStudioRunStore.setState({ queue: [], active: null, progress: null, error: null, completions: 0 });
  });

  it("submits queued runs one at a time, in order", async () => {
    const started: string[] = [];
    let release: (() => void) | undefined;
    runMock.mockImplementation((sent: GenerationRequest) => {
      started.push(sent.prompt);
      return new Promise((resolve) => {
        release = () => resolve([]);
      });
    });

    useStudioRunStore.getState().enqueue("first", { ...request, prompt: "first" });
    useStudioRunStore.getState().enqueue("second", { ...request, prompt: "second" });
    await settle();

    // The engine relaunches per run, so only one may be in flight at a time.
    expect(started).toEqual(["first"]);
    expect(useStudioRunStore.getState().active?.label).toBe("first");
    expect(useStudioRunStore.getState().queue).toHaveLength(1);

    release?.();
    await settle();
    expect(started).toEqual(["first", "second"]);
    expect(useStudioRunStore.getState().completions).toBe(1);

    release?.();
    await settle();
    expect(useStudioRunStore.getState().active).toBeNull();
    expect(useStudioRunStore.getState().completions).toBe(2);
  });

  it("drops the rest of the queue when a run fails, but not when the user stops one", async () => {
    runMock.mockRejectedValue(new Error("no such model"));
    useStudioRunStore.getState().enqueue("a", request);
    useStudioRunStore.getState().enqueue("b", request);
    await settle();
    expect(useStudioRunStore.getState().error).toBe("no such model");
    expect(useStudioRunStore.getState().queue).toHaveLength(0);

    useStudioRunStore.setState({ error: null });
    let release: (() => void) | undefined;
    runMock.mockImplementation(
      () => new Promise((_resolve, reject) => {
        release = () => reject(new Error("Generation cancelled"));
      }),
    );
    useStudioRunStore.getState().enqueue("c", request);
    await settle();
    const activeId = useStudioRunStore.getState().active?.id ?? "";
    useStudioRunStore.setState({
      progress: { jobId: "job-1", phase: "running", queuePosition: 0, percent: 10, step: 1, totalSteps: 20 },
    });
    void useStudioRunStore.getState().cancel(activeId);
    release?.();
    await settle();
    expect(cancelMock).toHaveBeenCalledWith("job-1");
    expect(useStudioRunStore.getState().error).toBeNull();
    expect(useStudioRunStore.getState().completions).toBe(0);
  });

  it("removes a queued run without touching the active one, and caps the queue", async () => {
    runMock.mockImplementation(() => new Promise(() => {}));
    const ids = Array.from({ length: MAX_STUDIO_QUEUE + 2 }, (_unused, index) =>
      useStudioRunStore.getState().enqueue(`run-${index}`, request),
    );
    await settle();
    expect(ids.filter((id) => id === null)).toHaveLength(2);
    expect(useStudioRunStore.getState().queue).toHaveLength(MAX_STUDIO_QUEUE - 1);

    const queuedId = useStudioRunStore.getState().queue[1].id;
    await useStudioRunStore.getState().cancel(queuedId);
    expect(useStudioRunStore.getState().queue.some((item) => item.id === queuedId)).toBe(false);
    expect(useStudioRunStore.getState().active?.label).toBe("run-0");
    expect(cancelMock).not.toHaveBeenCalled();
  });
});
