// @vitest-environment jsdom
import { beforeEach, describe, expect, it, vi } from "vitest";

import {
  SESSION_LIST_VIEW_STORAGE_KEY,
  useSessionListViewStore,
} from "./sessionListViewStore";
import { DEFAULT_SESSION_LIST_PREFS } from "../components/Chat/sessionListView";

const KNOWN = ["local", "remote_control", "channel:slack"];

function stored() {
  const raw = localStorage.getItem(SESSION_LIST_VIEW_STORAGE_KEY);
  return raw ? (JSON.parse(raw) as { environments: string[] }) : null;
}

describe("sessionListViewStore", () => {
  beforeEach(() => {
    localStorage.clear();
    useSessionListViewStore.setState({ prefs: DEFAULT_SESSION_LIST_PREFS });
  });

  it("turns the first toggle into 'every environment except that one'", () => {
    useSessionListViewStore.getState().toggleEnvironment("channel:slack", KNOWN);

    expect(useSessionListViewStore.getState().prefs.environments).toEqual([
      "local",
      "remote_control",
    ]);
    expect(stored()?.environments).toEqual(["local", "remote_control"]);
  });

  it("collapses back to 'all' once nothing is excluded again", () => {
    const { toggleEnvironment } = useSessionListViewStore.getState();
    toggleEnvironment("channel:slack", KNOWN);
    toggleEnvironment("channel:slack", KNOWN);

    // Not ["local","remote_control","channel:slack"]: an enumeration would
    // silently exclude an environment that appears later.
    expect(useSessionListViewStore.getState().prefs.environments).toEqual([]);
  });

  it("reads 'nothing selected' as 'all' rather than an empty sidebar", () => {
    const { toggleEnvironment } = useSessionListViewStore.getState();
    for (const environment of KNOWN) toggleEnvironment(environment, KNOWN);

    expect(useSessionListViewStore.getState().prefs.environments).toEqual([]);
  });

  it("ignores a stored value it cannot render", async () => {
    localStorage.setItem(
      SESSION_LIST_VIEW_STORAGE_KEY,
      JSON.stringify({ status: "invented", environments: [7], groupBy: "date", sortBy: "nope" }),
    );
    // Re-import to exercise hydrate() rather than asserting on state a test
    // set up by hand.
    vi.resetModules();
    const fresh = await import("./sessionListViewStore");
    const prefs = fresh.useSessionListViewStore.getState().prefs;

    expect(prefs.status).toBe(DEFAULT_SESSION_LIST_PREFS.status);
    expect(prefs.sortBy).toBe(DEFAULT_SESSION_LIST_PREFS.sortBy);
    expect(prefs.environments).toEqual([]);
    // A value it *can* render survives.
    expect(prefs.groupBy).toBe("date");
  });
});
