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
import { cleanup, fireEvent, render, screen, waitFor, within } from "@testing-library/react";

const conversationsList = vi.fn();
const conversationsShow = vi.fn(() => Promise.resolve({ messages: [] }));
const conversationsDelete = vi.fn((_environment: string, _id: string) => Promise.resolve());
vi.mock("../../lib/conversationsClient", async () => {
  const actual = await vi.importActual<typeof import("../../lib/conversationsClient")>(
    "../../lib/conversationsClient",
  );
  return {
    ...actual,
    conversationsList: () => conversationsList(),
    conversationsShow: () => conversationsShow(),
    conversationsDelete: (environment: string, id: string) => conversationsDelete(environment, id),
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
import { useExternalConversationMetaStore } from "../../store/externalConversationMetaStore";
import { useSessionListViewStore } from "../../store/sessionListViewStore";
import { DEFAULT_SESSION_LIST_PREFS } from "./sessionListView";
import { REMOTE_CONTROL_ENVIRONMENT, SLACK_ENVIRONMENT } from "../../lib/conversationsClient";

const NOW = 1_760_000_000_000;

/** A Telegram DM as the daemon lists it: titled by the person in it. */
const TELEGRAM_DM_KEY = "channel:telegram telegram:acct-1:931819457";
const TELEGRAM_DM = {
  environment: "channel:telegram",
  provider: "telegram",
  id: "telegram:acct-1:931819457",
  title: "ahmad",
  account_label: "Little",
  updated_at_ms: NOW + 2_000,
  message_count: 4,
};

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
  useExternalConversationMetaStore.setState({ meta: {} });
  useSessionListViewStore.setState({ prefs: DEFAULT_SESSION_LIST_PREFS });
});

/** The sidebar row carrying `title`, for scoping queries to one row. */
function rowOf(title: string): HTMLElement {
  return screen.getByText(title).closest("[role=button]") as HTMLElement;
}

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

  it("exposes pin and archive actions on a local chat row", async () => {
    render(<ChatSessionList />);
    await screen.findByText("From my phone");
    // Every row offers these now, so the query is scoped to the local one.
    const row = rowOf("Local session");

    fireEvent.click(within(row).getByRole("button", { name: "Pin" }));
    expect(useSessionStore.getState().sessions[0]?.pinned).toBe(true);

    fireEvent.click(within(rowOf("Local session")).getByRole("button", { name: "Archive" }));
    expect(useSessionStore.getState().sessions[0]?.archived).toBe(true);
  });

  it("only reveals session shortcuts while the primary modifier is held", () => {
    useSessionStore.setState({
      sessions: [session(), session({ id: "second", title: "Second session" })],
      activeSessionId: "local-1",
    });
    render(<ChatSessionList />);

    expect(screen.queryByText(/⌘1|Ctrl\+1/)).toBeNull();
    fireEvent.keyDown(window, { key: "Control", ctrlKey: true });
    expect(screen.getByText(/⌘1|Ctrl\+1/)).toBeTruthy();
    const row = screen.getByText("Local session").closest("[role=button]")!;
    fireEvent.pointerEnter(row);
    expect(screen.queryByText(/⌘1|Ctrl\+1/)).toBeNull();
    expect(screen.getByText(/⌘2|Ctrl\+2/)).toBeTruthy();
    fireEvent.pointerLeave(row);
    fireEvent.keyUp(window, { key: "Control", ctrlKey: false });
    expect(screen.queryByText(/⌘1|Ctrl\+1/)).toBeNull();
  });

  it("shows the workspace after a short dwell for chats without Git context", async () => {
    useSessionStore.setState({
      sessions: [session({ workspacePath: "/work/newApp" })],
      activeSessionId: "local-1",
    });
    render(<ChatSessionList />);

    fireEvent.pointerEnter(screen.getByText("Local session").closest("[role=button]")!);
    expect(await screen.findByText("newApp")).toBeTruthy();
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

  it("gives an outside conversation the same row actions a local chat has", async () => {
    conversationsList.mockResolvedValue({ conversations: [TELEGRAM_DM] });
    render(<ChatSessionList />);
    await screen.findByText("ahmad");

    // Pin: the row moves under its own heading, and stays selected-free.
    fireEvent.click(within(rowOf("ahmad")).getByRole("button", { name: "Pin" }));
    expect(screen.getByText("Pinned")).toBeTruthy();
    expect(useExternalConversationMetaStore.getState().meta[TELEGRAM_DM_KEY]?.pinned).toBe(true);
    expect(useExternalConversationStore.getState().selected).toBeNull();

    // Archive un-pins and files it behind the collapsed footer.
    fireEvent.click(within(rowOf("ahmad")).getByRole("button", { name: "Archive" }));
    expect(screen.queryByText("Pinned")).toBeNull();
    expect(screen.getByText("Archived (1)")).toBeTruthy();
    fireEvent.click(screen.getByText("Archived (1)"));
    fireEvent.click(within(rowOf("ahmad")).getByRole("button", { name: "Unarchive" }));
    expect(screen.queryByText(/Archived \(/)).toBeNull();

    // Rename, from the menu, is this desktop's own name for it.
    fireEvent.click(within(rowOf("ahmad")).getByRole("button", { name: "Session menu" }));
    fireEvent.click(screen.getByRole("menu").querySelector("button:nth-of-type(3)")!);
    const input = screen.getByDisplayValue("ahmad");
    fireEvent.change(input, { target: { value: "Ahmad (Telegram)" } });
    fireEvent.keyDown(input, { key: "Enter" });
    expect(await screen.findByText("Ahmad (Telegram)")).toBeTruthy();
    expect(useExternalConversationMetaStore.getState().meta[TELEGRAM_DM_KEY]?.title).toBe(
      "Ahmad (Telegram)",
    );
  });

  it("marks an outside conversation unread until it is opened", async () => {
    conversationsList.mockResolvedValue({ conversations: [TELEGRAM_DM] });
    useExternalConversationMetaStore.getState().update(TELEGRAM_DM_KEY, { unread: true });
    render(<ChatSessionList />);

    const title = await screen.findByText("ahmad");
    expect(title.closest("span.font-semibold")).toBeTruthy();
    fireEvent.click(title);
    await waitFor(() =>
      expect(useExternalConversationMetaStore.getState().meta[TELEGRAM_DM_KEY]?.unread).toBeFalsy(),
    );
  });

  it("deletes an outside conversation from the menu, after asking", async () => {
    conversationsList.mockResolvedValue({ conversations: [TELEGRAM_DM] });
    useExternalConversationMetaStore.getState().update(TELEGRAM_DM_KEY, { pinned: true });
    const confirm = vi.spyOn(window, "confirm").mockReturnValue(true);
    render(<ChatSessionList />);
    await screen.findByText("ahmad");

    fireEvent.click(within(rowOf("ahmad")).getByRole("button", { name: "Session menu" }));
    // The listing the refetch sees after the daemon erased the row.
    conversationsList.mockResolvedValue({ conversations: [] });
    fireEvent.click(within(screen.getByRole("menu")).getByText("Delete"));

    expect(confirm).toHaveBeenCalledOnce();
    expect(confirm.mock.calls[0]?.[0]).toContain("ahmad");
    await waitFor(() =>
      expect(conversationsDelete).toHaveBeenCalledWith("channel:telegram", "telegram:acct-1:931819457"),
    );
    await waitFor(() => expect(screen.queryByText("ahmad")).toBeNull());
    // Deleting is not selecting, and the desktop's notes about it go with it.
    expect(useExternalConversationStore.getState().selected).toBeNull();
    await waitFor(() =>
      expect(useExternalConversationMetaStore.getState().meta[TELEGRAM_DM_KEY]).toBeUndefined(),
    );
    confirm.mockRestore();
  });

  it("does nothing when deleting is declined", async () => {
    conversationsList.mockResolvedValue({ conversations: [TELEGRAM_DM] });
    const confirm = vi.spyOn(window, "confirm").mockReturnValue(false);
    render(<ChatSessionList />);
    await screen.findByText("ahmad");

    fireEvent.click(within(rowOf("ahmad")).getByRole("button", { name: "Session menu" }));
    fireEvent.click(within(screen.getByRole("menu")).getByText("Delete"));

    expect(conversationsDelete).not.toHaveBeenCalled();
    expect(screen.getByText("ahmad")).toBeTruthy();
    confirm.mockRestore();
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
