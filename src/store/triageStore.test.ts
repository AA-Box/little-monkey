import { beforeEach, describe, expect, it, vi } from "vitest";

const invokeMock = vi.fn();
vi.mock("@tauri-apps/api/core", () => ({
  invoke: (...args: unknown[]) => invokeMock(...args),
}));

import { useTriageStore, type TriageItem } from "./triageStore";

const item: TriageItem = {
  id: "github:acme/widgets#1",
  source: "github",
  title: "Fix the widget",
  summary: "issue #1 — stale for 3.0d",
  rank_score: 3,
  url: "https://github.com/acme/widgets/issues/1",
  staleness_days: 3,
  suggested_action: { kind: "comment", draft_text: "", target: "acme/widgets#1" },
  connector_account_id: null,
};

beforeEach(() => {
  invokeMock.mockReset();
  useTriageStore.setState({ items: [], loading: false, error: null });
});

describe("triageStore", () => {
  it("loads the persisted queue without refreshing any source", async () => {
    invokeMock.mockResolvedValueOnce([item]);
    await useTriageStore.getState().list();
    expect(invokeMock).toHaveBeenCalledWith("triage_list");
    expect(useTriageStore.getState().items).toEqual([item]);
  });

  it("refreshes the requested sources and replaces the queue", async () => {
    invokeMock.mockResolvedValueOnce({ items: [item], errors: [] });
    await useTriageStore.getState().refresh([{ kind: "github", owner: "acme", repo: "widgets" }]);
    expect(invokeMock).toHaveBeenCalledWith("triage_refresh", {
      sources: [{ kind: "github", owner: "acme", repo: "widgets" }],
    });
    expect(useTriageStore.getState().items).toEqual([item]);
    expect(useTriageStore.getState().error).toBeNull();
  });

  it("surfaces a refresh error without throwing", async () => {
    invokeMock.mockRejectedValueOnce(new Error("boom"));
    await useTriageStore.getState().refresh([]);
    expect(useTriageStore.getState().error).toBe("boom");
    expect(useTriageStore.getState().loading).toBe(false);
  });

  it("keeps the successfully-fetched items and surfaces per-source errors on a partial refresh failure", async () => {
    invokeMock.mockResolvedValueOnce({ items: [item], errors: ["slack:C123: invalid_auth"] });
    await useTriageStore.getState().refresh([
      { kind: "github", owner: "acme", repo: "widgets" },
      { kind: "slack", connector_account_id: "acct-1", channel_id: "C123" },
    ]);
    expect(useTriageStore.getState().items).toEqual([item]);
    expect(useTriageStore.getState().error).toBe("slack:C123: invalid_auth");
    expect(useTriageStore.getState().loading).toBe(false);
  });

  it("generates a draft and patches only the matching item in place", async () => {
    useTriageStore.setState({ items: [item, { ...item, id: "jira:PROJ-1" }] });
    const updated: TriageItem = {
      ...item,
      suggested_action: { kind: "comment", draft_text: "Thanks, looking into it.", target: "acme/widgets#1" },
    };
    invokeMock.mockResolvedValueOnce(updated);

    const result = await useTriageStore.getState().generateDraft("github:acme/widgets#1", "anthropic", "claude-x", "high");

    expect(invokeMock).toHaveBeenCalledWith("triage_generate_draft", {
      itemId: "github:acme/widgets#1",
      providerId: "anthropic",
      model: "claude-x",
      effort: "high",
    });
    expect(result).toEqual(updated);
    const items = useTriageStore.getState().items;
    expect(items.find((i) => i.id === "github:acme/widgets#1")).toEqual(updated);
    expect(items).toHaveLength(2);
  });

  it("omits effort as null when not given", async () => {
    invokeMock.mockResolvedValueOnce(item);
    await useTriageStore.getState().generateDraft("github:acme/widgets#1", "anthropic", "claude-x");
    expect(invokeMock).toHaveBeenCalledWith("triage_generate_draft", {
      itemId: "github:acme/widgets#1",
      providerId: "anthropic",
      model: "claude-x",
      effort: null,
    });
  });

  it("sends a draft then removes it from the local queue on success", async () => {
    useTriageStore.setState({ items: [item] });
    invokeMock.mockResolvedValueOnce(undefined);
    await useTriageStore.getState().sendDraft("github:acme/widgets#1");
    expect(invokeMock).toHaveBeenCalledWith("triage_send_draft", { itemId: "github:acme/widgets#1" });
    expect(useTriageStore.getState().items).toEqual([]);
  });

  it("leaves the queue untouched when sendDraft is denied permission", async () => {
    useTriageStore.setState({ items: [item] });
    invokeMock.mockRejectedValueOnce(new Error("Permission denied"));
    await expect(useTriageStore.getState().sendDraft("github:acme/widgets#1")).rejects.toThrow("Permission denied");
    expect(useTriageStore.getState().items).toEqual([item]);
  });
});
