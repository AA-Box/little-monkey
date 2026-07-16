import { beforeEach, describe, expect, it } from "vitest";
import { useBrowserWorkbenchStore } from "./browserWorkbenchStore";

describe("browser workbench chat evidence queue", () => {
  beforeEach(() => useBrowserWorkbenchStore.setState({ pendingBySession: {} }));

  it("keeps evidence scoped to the target chat session", () => {
    useBrowserWorkbenchStore.getState().queueForChat("session-a", {
      id: "evidence-1",
      summary: "bounded evidence",
      screenshot: null,
    });
    expect(useBrowserWorkbenchStore.getState().pendingBySession["session-a"]?.summary).toBe("bounded evidence");
    expect(useBrowserWorkbenchStore.getState().pendingBySession["session-b"]).toBeUndefined();
  });

  it("will not consume a newer evidence item with a stale id", () => {
    const store = useBrowserWorkbenchStore.getState();
    store.queueForChat("session-a", { id: "new", summary: "latest", screenshot: null });
    store.consumeForChat("session-a", "old");
    expect(useBrowserWorkbenchStore.getState().pendingBySession["session-a"]?.id).toBe("new");
    store.consumeForChat("session-a", "new");
    expect(useBrowserWorkbenchStore.getState().pendingBySession["session-a"]).toBeUndefined();
  });
});
