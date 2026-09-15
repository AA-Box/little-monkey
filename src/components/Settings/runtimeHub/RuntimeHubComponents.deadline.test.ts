/**
 * The deadline behind "Check for new versions".
 *
 * The button is disabled while a sync runs, so a call that never settles takes
 * the retry with it: the panel sits on a spinner, no notice ever appears, and
 * every later click is swallowed — which is exactly what a sync starved by a
 * busy main thread looked like from the outside.
 */
import { describe, expect, it } from "vitest";

import { withDeadline } from "./RuntimeHubComponents";

describe("withDeadline", () => {
  it("passes a settled result straight through", async () => {
    await expect(withDeadline(Promise.resolve("adopted"), 10_000)).resolves.toBe("adopted");
    await expect(withDeadline(Promise.reject(new Error("offline")), 10_000)).rejects.toThrow(
      "offline",
    );
  });

  it("rejects a call that never settles, naming the budget it blew", async () => {
    await expect(withDeadline(new Promise(() => {}), 5)).rejects.toThrow(
      /did not finish within 0 seconds|did not finish within/,
    );
  });
});
