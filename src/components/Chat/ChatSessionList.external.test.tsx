// @vitest-environment jsdom
/**
 * The sidebar with more than one environment in it.
 *
 * The claims worth holding: a conversation the daemon owns is listed beside
 * this machine's own sessions, opening one hands the main pane over to it
 * (and opening a local session takes it back), and the environment filter
 * actually narrows the list rather than only labelling it.
 */
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";

const conversationsList = vi.fn();
const conversationsShow = vi.fn(() => Promise.resolve({ messages: [] }));
vi.mock("../../lib/conversationsClient", async () => {
  const actual = await vi.importActual<typeof import("../../lib/conversationsClient")>(
    "../../lib/conversationsClient",
  );
  return {
    ...actual,
    conversationsList: () => conversationsList(),
    conversationsShow: () => conversationsShow(),
  };
});
// Stores imported down the tree subscribe to Tauri events at module load;
// outside the shell there is no bridge, so stand in for the two entry points
// they touch. `isTauri` false is what plain-browser dev already reports.
vi.mock("@tauri-apps/api/core", () => ({
  invoke: vi.fn(() => Promise.resolve()),
  isTauri: () => false,
}));

import ChatSessionList from "./ChatSessionList";
import { useSessionStore, type ChatSession } from "../../store/sessionStore";
import { useExternalConversationStore } from "../../store/externalConversationStore";
import { useSessionListViewStore } from "../../store/sessionListViewStore";
import { DEFAULT_SESSION_LIST_PREFS } from "./sessionListView";
import { REMOTE_CONTROL_ENVIRONMENT, SLACK_ENVIRONMENT } from "../../lib/conversationsClient";

const NOW = 1_760_000_000_000;

function session(overrides: Partial<ChatSession> = {}): ChatSession {
  return {
    id: "local-1",
    title: "Local session",
    messages: [{ role: "user", content: "hello" }],
    createdAt: NOW,
    updatedAt: NOW,
    pinned: false,
    unread: false,
    archived: false,
    groupId: null,
    modelTarget: null,
    comparisonBranch: null,
    workspacePath: null,
    personaId: null,
    attachedStackIds: [],
    docChatMode: false,
    subagentRuns: {},
    ...overrides,
  };
}

beforeEach(() => {
  localStorage.clear();
  conversationsList.mockResolvedValue({
    conversations: [
      {
        environment: REMOTE_CONTROL_ENVIRONMENT,
        provider: null,
        id: "phone-1",
        title: "From my phone",
        account_label: null,
        updated_at_ms: NOW + 1_000,
        message_count: 3,
      },
    ],
  });
  useSessionStore.setState({ sessions: [session()], groups: [], activeSessionId: "local-1" });
  useExternalConversationStore.setState({ conversations: [], messages: {}, selected: null, error: null });
  useSessionListViewStore.setState({ prefs: DEFAULT_SESSION_LIST_PREFS });
});

afterEach(() => {
  cleanup();
  vi.clearAllMocks();
});

describe("ChatSessionList across environments", () => {
  it("renders the four session marker states without text badges", () => {
    useSessionStore.setState({
      sessions: [
        session(),
        session({ id: "done", title: "Done" }),
        session({ id: "error", title: "Error" }),
        session({ id: "idle", title: "Idle" }),
      ],
      activeSessionId: "local-1",
      runningTurns: { "local-1": true },
      turnOutcomes: { done: "done", error: "error" },
    });
    render(<ChatSessionList />);

    const working = screen.getByRole("img", { name: "Working" });
    expect(working.querySelector(".animate-pulse")).toBeTruthy();

    const finished = screen.getByRole("img", { name: "Finished" });
    expect(finished.querySelector(".bg-accent")).toBeTruthy();
    expect(finished.querySelector(".animate-pulse")).toBeNull();

    const idle = screen.getByRole("img", { name: "Idle" });
    expect(idle.querySelector(".border-faint")).toBeTruthy();

    expect(screen.getByRole("img", { name: "Failed" }).querySelector("svg")).toBeTruthy();
  });

  it("lists a conversation the daemon owns beside the local sessions", async () => {
    render(<ChatSessionList />);

    expect(await screen.findByText("From my phone")).toBeTruthy();
    expect(screen.getByText("Local session")).toBeTruthy();
  });

  it("hands the main pane to an outside conversation, and takes it back", async () => {
    render(<ChatSessionList />);

    fireEvent.click(await screen.findByText("From my phone"));
    await waitFor(() =>
      expect(useExternalConversationStore.getState().selected).toEqual({
        environment: REMOTE_CONTROL_ENVIRONMENT,
        id: "phone-1",
      }),
    );
    // Selecting one loads its transcript rather than waiting for the pane.
    expect(conversationsShow).toHaveBeenCalled();

    fireEvent.click(screen.getByText("Local session"));
    expect(useExternalConversationStore.getState().selected).toBeNull();
  });

  it("filters the list down to the chosen environment", async () => {
    render(<ChatSessionList />);
    await screen.findByText("From my phone");

    useSessionListViewStore.getState().setPrefs({ environments: [REMOTE_CONTROL_ENVIRONMENT] });
    await waitFor(() => expect(screen.queryByText("Local session")).toBeNull());
    expect(screen.getByText("From my phone")).toBeTruthy();

    // An environment nothing has arrived on empties the list rather than
    // quietly falling back to everything.
    useSessionListViewStore.getState().setPrefs({ environments: [SLACK_ENVIRONMENT] });
    await waitFor(() => expect(screen.queryByText("From my phone")).toBeNull());
    expect(screen.getByText("No sessions match this filter.")).toBeTruthy();
  });

  it("keeps the list usable when the daemon cannot be reached", async () => {
    conversationsList.mockRejectedValue(new Error("daemon is not running"));
    render(<ChatSessionList />);

    expect(await screen.findByText("Local session")).toBeTruthy();
    await waitFor(() =>
      expect(useExternalConversationStore.getState().error).toContain("daemon is not running"),
    );
  });
});
