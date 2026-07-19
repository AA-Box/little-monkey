import { afterEach, describe, expect, it, vi } from "vitest";

import {
  cancelRegisteredRun,
  clearRunCancellationRegistryForTests,
  hasRegisteredRun,
  registerRunCancellation,
} from "./runCancellationRegistry";

afterEach(clearRunCancellationRegistryForTests);

describe("run cancellation registry", () => {
  it("routes a stop to the exact run and disposes ownership safely", () => {
    const first = vi.fn();
    const second = vi.fn();
    const disposeFirst = registerRunCancellation("run-a", first);
    registerRunCancellation("run-b", second);

    expect(cancelRegisteredRun("run-a")).toBe(true);
    expect(first).toHaveBeenCalledOnce();
    expect(second).not.toHaveBeenCalled();
    disposeFirst();
    expect(hasRegisteredRun("run-a")).toBe(false);
    expect(cancelRegisteredRun("missing")).toBe(false);
  });

  it("does not let an old disposer remove a newer owner", () => {
    const disposeOld = registerRunCancellation("run-a", vi.fn());
    const newest = vi.fn();
    registerRunCancellation("run-a", newest);
    disposeOld();
    expect(cancelRegisteredRun("run-a")).toBe(true);
    expect(newest).toHaveBeenCalledOnce();
  });
});
