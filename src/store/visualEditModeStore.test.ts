import { beforeEach, describe, expect, it, vi } from "vitest";

const proposeVisualEditMock = vi.fn();
const writeVisualEditToDiskMock = vi.fn();

vi.mock("../lib/visualEditMode", () => ({
  proposeVisualEdit: (...args: unknown[]) => proposeVisualEditMock(...args),
  writeVisualEditToDisk: (...args: unknown[]) => writeVisualEditToDiskMock(...args),
}));

import { useVisualEditModeStore, type StartVisualEditParams } from "./visualEditModeStore";

const baseParams: StartVisualEditParams = {
  sessionId: "browser-session-1",
  pageUrl: "http://localhost:3000/",
  description: "make this button larger",
  element: {
    selector: "button.cta",
    tag: "button",
    role: "button",
    ariaLabel: "",
    text: "Get started",
    rect: { x: 0, y: 0, width: 10, height: 10 },
  },
  beforeScreenshot: { path: "browser-evidence://abc.png", dataUrl: "data:image/png;base64,AAA" },
};

const proposal = {
  targetFile: "src/components/Cta.tsx",
  oldContent: "old",
  newContent: "new",
  unifiedDiff: "--- a/src/components/Cta.tsx\n+++ b/src/components/Cta.tsx\n@@ -1,1 +1,1 @@\n-old\n+new",
  summary: "Made the button larger",
};

beforeEach(() => {
  proposeVisualEditMock.mockReset();
  writeVisualEditToDiskMock.mockReset();
  useVisualEditModeStore.setState({ edits: {}, order: [] });
});

describe("visualEditModeStore", () => {
  it("start() creates a generating edit immediately and resolves to pending on a successful proposal", async () => {
    let resolveProposal!: (value: typeof proposal) => void;
    proposeVisualEditMock.mockReturnValue(new Promise((resolve) => (resolveProposal = resolve)));

    const id = useVisualEditModeStore.getState().start(baseParams);

    const created = useVisualEditModeStore.getState().edits[id];
    expect(created.status).toBe("generating");
    expect(created.description).toBe(baseParams.description);
    expect(useVisualEditModeStore.getState().order[0]).toBe(id);

    resolveProposal(proposal);
    await vi.waitFor(() => expect(useVisualEditModeStore.getState().edits[id].status).toBe("pending"));

    const settled = useVisualEditModeStore.getState().edits[id];
    expect(settled.targetFile).toBe(proposal.targetFile);
    expect(settled.newContent).toBe(proposal.newContent);
    expect(settled.unifiedDiff).toBe(proposal.unifiedDiff);
    expect(settled.error).toBeNull();
  });

  it("start() moves an edit to error status when the proposal rejects", async () => {
    proposeVisualEditMock.mockRejectedValue(new Error("could not locate a source file"));

    const id = useVisualEditModeStore.getState().start(baseParams);
    await vi.waitFor(() => expect(useVisualEditModeStore.getState().edits[id].status).toBe("error"));

    expect(useVisualEditModeStore.getState().edits[id].error).toBe("could not locate a source file");
  });

  it("accept() writes the file and marks the edit accepted", async () => {
    proposeVisualEditMock.mockResolvedValue(proposal);
    writeVisualEditToDiskMock.mockResolvedValue(undefined);

    const id = useVisualEditModeStore.getState().start(baseParams);
    await vi.waitFor(() => expect(useVisualEditModeStore.getState().edits[id].status).toBe("pending"));

    await useVisualEditModeStore.getState().accept(id);

    expect(writeVisualEditToDiskMock).toHaveBeenCalledWith(proposal.targetFile, proposal.newContent);
    expect(useVisualEditModeStore.getState().edits[id].status).toBe("accepted");
  });

  it("accept() leaves the edit pending with an error message when the write fails", async () => {
    proposeVisualEditMock.mockResolvedValue(proposal);
    writeVisualEditToDiskMock.mockRejectedValue(new Error("Permission denied by user"));

    const id = useVisualEditModeStore.getState().start(baseParams);
    await vi.waitFor(() => expect(useVisualEditModeStore.getState().edits[id].status).toBe("pending"));

    await expect(useVisualEditModeStore.getState().accept(id)).rejects.toThrow("Permission denied by user");

    const after = useVisualEditModeStore.getState().edits[id];
    expect(after.status).toBe("pending");
    expect(after.error).toBe("Permission denied by user");
  });

  it("accept() is a no-op for an edit that isn't pending", async () => {
    proposeVisualEditMock.mockReturnValue(new Promise(() => {})); // never resolves — stays "generating"
    const id = useVisualEditModeStore.getState().start(baseParams);

    await useVisualEditModeStore.getState().accept(id);

    expect(writeVisualEditToDiskMock).not.toHaveBeenCalled();
    expect(useVisualEditModeStore.getState().edits[id].status).toBe("generating");
  });

  it("reject() marks the edit rejected without touching disk", async () => {
    proposeVisualEditMock.mockResolvedValue(proposal);
    const id = useVisualEditModeStore.getState().start(baseParams);
    await vi.waitFor(() => expect(useVisualEditModeStore.getState().edits[id].status).toBe("pending"));

    useVisualEditModeStore.getState().reject(id);

    expect(useVisualEditModeStore.getState().edits[id].status).toBe("rejected");
    expect(writeVisualEditToDiskMock).not.toHaveBeenCalled();
  });

  it("replay() resets an edit back to generating and re-runs the proposal", async () => {
    proposeVisualEditMock.mockResolvedValueOnce(proposal);
    const id = useVisualEditModeStore.getState().start(baseParams);
    await vi.waitFor(() => expect(useVisualEditModeStore.getState().edits[id].status).toBe("pending"));

    const secondProposal = { ...proposal, summary: "Replayed change" };
    proposeVisualEditMock.mockResolvedValueOnce(secondProposal);

    await useVisualEditModeStore.getState().replay(id);

    expect(proposeVisualEditMock).toHaveBeenCalledTimes(2);
    expect(useVisualEditModeStore.getState().edits[id].summary).toBe("Replayed change");
    expect(useVisualEditModeStore.getState().edits[id].status).toBe("pending");
  });

  it("setAfterScreenshot() attaches the post-accept screenshot", () => {
    proposeVisualEditMock.mockReturnValue(new Promise(() => {}));
    const id = useVisualEditModeStore.getState().start(baseParams);

    useVisualEditModeStore.getState().setAfterScreenshot(id, { path: "p", dataUrl: "d" });

    expect(useVisualEditModeStore.getState().edits[id].afterScreenshot).toEqual({ path: "p", dataUrl: "d" });
  });

  it("remove() deletes the edit and its order entry", () => {
    proposeVisualEditMock.mockReturnValue(new Promise(() => {}));
    const id = useVisualEditModeStore.getState().start(baseParams);

    useVisualEditModeStore.getState().remove(id);

    expect(useVisualEditModeStore.getState().edits[id]).toBeUndefined();
    expect(useVisualEditModeStore.getState().order).not.toContain(id);
  });

  it("clear() empties both edits and order", () => {
    proposeVisualEditMock.mockReturnValue(new Promise(() => {}));
    useVisualEditModeStore.getState().start(baseParams);
    useVisualEditModeStore.getState().start({ ...baseParams, description: "second" });

    useVisualEditModeStore.getState().clear();

    expect(useVisualEditModeStore.getState().edits).toEqual({});
    expect(useVisualEditModeStore.getState().order).toEqual([]);
  });
});
