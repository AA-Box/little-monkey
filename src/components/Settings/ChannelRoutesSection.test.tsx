// @vitest-environment jsdom
/**
 * The route editor, driven the way an operator drives it.
 *
 * Two properties matter here and neither is cosmetic.
 *
 * A rung's ids are all mandatory. The daemon's sender rung is
 * `account + conversation + thread + sender`; a form that renders the thread
 * field but lets Save through without it submits a *different* scope than the
 * one the operator picked — silently a Conversation route, or a sender route
 * that would follow its sender into every thread. The daemon refuses both, so
 * the only question is whether the operator finds out here or after a
 * confusing error.
 *
 * And an edit must carry everything back. `update-route` replaces the target
 * wholesale, so a field this form forgets is a field the operator loses by
 * renaming a recipe.
 */
import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";

const invoke = vi.fn();
vi.mock("@tauri-apps/api/core", () => ({
  invoke: (...args: unknown[]) => invoke(...args),
  isTauri: () => true,
}));

import { ChannelRoutesSection, draftFrom, draftIncomplete, draftOptions } from "./ChannelRoutesSection";
import type { ChannelAccount, ChannelRoute } from "../../lib/channelsClient";

const ACCOUNT: ChannelAccount = {
  account_id: "chan-1",
  kind: "slack",
  label: "Team Slack",
  enabled: true,
  has_credential: true,
  credential_required: true,
  access_policy: { direct: "pairing", group: "allow_list", group_activation: "mention_only" },
  health: "connected",
  health_detail: null,
  last_error: null,
  last_probe_at_ms: 0,
  non_secret_config: {},
  created_at_ms: 0,
  callback_rejections: { count: 0, last_reason: null, last_at_ms: null },
  echo_correlation: "host_adapter",
  reply_policy_restricted: false,
  updated_at_ms: 0,
};

/** Every target field populated, so anything dropped in a round trip shows. */
const FULL_ROUTE: ChannelRoute = {
  route_id: "route-1",
  scope: {
    account_id: "chan-1",
    conversation_id: "C1",
    thread_id: "T1",
    sender_id: "U1",
  },
  target: {
    recipe: "triage",
    params: { focus: "deps", depth: "3" },
    repository: "/work/repo",
    session_scope: "sender",
    priority: 7,
    reply_to_conversation: false,
  },
  enabled: false,
  created_at_ms: 1,
  updated_at_ms: 1,
};

/** One saved task, as `recipes_list` reports it — the editor offers these
 * instead of a free-text name, so a test that picks one has to have it. */
function discovered(name: string) {
  return {
    path: `/tasks/${name}.yml`,
    source: "global",
    error: null,
    recipe: {
      version: 1,
      name,
      target: { ollama: "qwen2.5:7b" },
      permission_mode: "manual",
      prompt: "{{message}}",
      params: { message: "" },
      output: { json: false },
    },
  };
}

function mockRoutes(routes: ChannelRoute[], recipes = ["chat", "triage", "triage-2"]) {
  invoke.mockImplementation((command: string) => {
    if (command === "channels_routes") return Promise.resolve({ routes });
    if (command === "channels_events") return Promise.resolve({ events: [] });
    if (command === "recipes_list") return Promise.resolve(recipes.map(discovered));
    return Promise.resolve(null);
  });
}

afterEach(() => {
  cleanup();
  invoke.mockReset();
});

describe("the route draft", () => {
  it("is incomplete until every id its rung carries is named", () => {
    const base = {
      routeId: null,
      recipe: "chat",
      kind: "slack",
      accountId: "chan-1",
      conversationId: "C1",
      threadId: "",
      senderId: "",
      repository: "",
      params: [],
      sessionScope: "thread" as const,
      priority: "",
      reply: true,
      enabled: true,
    };

    expect(draftIncomplete({ ...base, level: "global" })).toBe(false);
    expect(draftIncomplete({ ...base, level: "provider" })).toBe(false);
    expect(draftIncomplete({ ...base, level: "account" })).toBe(false);
    expect(draftIncomplete({ ...base, level: "conversation" })).toBe(false);
    // The two rungs that carry a thread require one.
    expect(draftIncomplete({ ...base, level: "thread" })).toBe(true);
    expect(draftIncomplete({ ...base, level: "thread", threadId: "  " })).toBe(true);
    expect(draftIncomplete({ ...base, level: "thread", threadId: "T1" })).toBe(false);
    expect(draftIncomplete({ ...base, level: "sender", senderId: "U1" })).toBe(true);
    expect(draftIncomplete({ ...base, level: "sender", threadId: "T1" })).toBe(true);
    expect(draftIncomplete({ ...base, level: "sender", threadId: "T1", senderId: "U1" })).toBe(false);
    // And the rest of the rung's ids are still required.
    expect(draftIncomplete({ ...base, level: "account", accountId: "" })).toBe(true);
    expect(draftIncomplete({ ...base, level: "conversation", conversationId: "" })).toBe(true);
    expect(draftIncomplete({ ...base, level: "global", recipe: " " })).toBe(true);
  });

  it("round-trips every scope and target field through the editor", () => {
    const options = draftOptions(draftFrom(FULL_ROUTE));
    expect(options).toEqual({
      account_id: "chan-1",
      conversation_id: "C1",
      thread_id: "T1",
      sender_id: "U1",
      kind: null,
      repository: "/work/repo",
      params: ["focus=deps", "depth=3"],
      session_scope: "sender",
      priority: 7,
      reply: false,
      enabled: false,
    });
  });

  it("sends only the ids its rung carries, so a leftover value cannot widen a scope", () => {
    // Opened on a sender route, moved down to Account: the conversation,
    // thread and sender fields still hold their old values, and none of them
    // may be sent.
    const options = draftOptions({ ...draftFrom(FULL_ROUTE), level: "account" });
    expect(options.conversation_id).toBeNull();
    expect(options.thread_id).toBeNull();
    expect(options.sender_id).toBeNull();
    expect(options.account_id).toBe("chan-1");
  });
});

describe("the routes section", () => {
  it("keeps Save disabled until a Thread route names its thread", async () => {
    // A route already exists, so the empty state's automatic first route does
    // not run underneath the form this test drives.
    mockRoutes([FULL_ROUTE]);
    render(<ChannelRoutesSection accounts={[ACCOUNT]} />);
    fireEvent.click(await screen.findByText("Add route"));

    fireEvent.change(screen.getByLabelText("Task"), { target: { value: "chat" } });
    fireEvent.change(screen.getByLabelText("Applies to"), { target: { value: "thread" } });
    fireEvent.change(screen.getByLabelText("Account"), { target: { value: "chan-1" } });
    fireEvent.change(screen.getByLabelText("Conversation"), { target: { value: "C1" } });

    const save = screen.getByText("Save route").closest("button") as HTMLButtonElement;
    expect(save.disabled).toBe(true);
    fireEvent.change(screen.getByLabelText("Thread"), { target: { value: "T1" } });
    expect(save.disabled).toBe(false);
  });

  it("keeps Save disabled until a Sender route names both its thread and its sender", async () => {
    mockRoutes([FULL_ROUTE]);
    render(<ChannelRoutesSection accounts={[ACCOUNT]} />);
    fireEvent.click(await screen.findByText("Add route"));

    fireEvent.change(screen.getByLabelText("Task"), { target: { value: "chat" } });
    fireEvent.change(screen.getByLabelText("Applies to"), { target: { value: "sender" } });
    fireEvent.change(screen.getByLabelText("Account"), { target: { value: "chan-1" } });
    fireEvent.change(screen.getByLabelText("Conversation"), { target: { value: "C1" } });
    fireEvent.change(screen.getByLabelText("Sender"), { target: { value: "U1" } });

    const save = screen.getByText("Save route").closest("button") as HTMLButtonElement;
    expect(save.disabled).toBe(true);
    fireEvent.change(screen.getByLabelText("Thread"), { target: { value: "T1" } });
    expect(save.disabled).toBe(false);
  });

  it("carries every field back when an unrelated one is edited", async () => {
    mockRoutes([FULL_ROUTE]);
    render(<ChannelRoutesSection accounts={[ACCOUNT]} />);
    fireEvent.click(await screen.findByText("Edit"));

    fireEvent.change(screen.getByLabelText("Task"), { target: { value: "triage-2" } });
    fireEvent.click(screen.getByText("Save route"));

    await waitFor(() =>
      expect(invoke).toHaveBeenCalledWith("channels_update_route", {
        routeId: "route-1",
        recipe: "triage-2",
        options: {
          account_id: "chan-1",
          conversation_id: "C1",
          thread_id: "T1",
          sender_id: "U1",
          kind: null,
          repository: "/work/repo",
          params: ["focus=deps", "depth=3"],
          session_scope: "sender",
          priority: 7,
          reply: false,
          enabled: false,
        },
      }),
    );
  });

  it("sets the first route up on its own for an account that is already connected", async () => {
    mockRoutes([], ["channel-chat"]);
    render(<ChannelRoutesSection accounts={[ACCOUNT]} />);

    // Nobody clicked anything: an account with a credential is already
    // accepting messages, and a message with no route runs nothing.
    await waitFor(() =>
      expect(invoke).toHaveBeenCalledWith("channels_add_route", {
        recipe: "channel-chat",
        options: {},
      }),
    );
  });

  it("leaves an account that cannot receive anything alone", async () => {
    mockRoutes([], ["channel-chat"]);
    render(<ChannelRoutesSection accounts={[{ ...ACCOUNT, enabled: false }]} />);

    await waitFor(() => expect(invoke).toHaveBeenCalledWith("channels_routes"));
    expect(invoke.mock.calls.some(([command]) => command === "channels_add_route")).toBe(false);
  });

  it("names an existing starter task when setting the first route up in one click", async () => {
    mockRoutes([], ["channel-chat"]);
    render(<ChannelRoutesSection accounts={[ACCOUNT]} />);

    fireEvent.click(await screen.findByText("Set up a starter task and route"));
    // The global default: no scope flags, so every account reaches it.
    await waitFor(() =>
      expect(invoke).toHaveBeenCalledWith("channels_add_route", {
        recipe: "channel-chat",
        options: {},
      }),
    );
    // The task already existed, so nothing wrote a second copy over it.
    expect(invoke.mock.calls.some(([command]) => command === "recipes_save")).toBe(false);
  });

  it("refuses to save a route naming a task that is not there", async () => {
    mockRoutes([FULL_ROUTE], ["chat"]);
    render(<ChannelRoutesSection accounts={[ACCOUNT]} />);
    fireEvent.click(await screen.findByText("Edit"));

    // The route points at "triage", which is not saved any more: the editor
    // says so rather than reading as some other task.
    expect(screen.getByText(/No task by this name is saved/)).toBeTruthy();
    const picker = screen.getByLabelText("Task") as HTMLSelectElement;
    expect(picker.value).toBe("triage");
    expect([...picker.options].map((option) => option.value)).toContain("chat");
  });

  it("turns a route off and on and removes it by id", async () => {
    mockRoutes([{ ...FULL_ROUTE, enabled: true }]);
    render(<ChannelRoutesSection accounts={[ACCOUNT]} />);

    fireEvent.click(await screen.findByText("Disable"));
    await waitFor(() =>
      expect(invoke).toHaveBeenCalledWith("channels_enable_route", {
        routeId: "route-1",
        enabled: false,
      }),
    );

    mockRoutes([{ ...FULL_ROUTE, enabled: false }]);
    cleanup();
    render(<ChannelRoutesSection accounts={[ACCOUNT]} />);
    fireEvent.click(await screen.findByText("Enable"));
    await waitFor(() =>
      expect(invoke).toHaveBeenCalledWith("channels_enable_route", {
        routeId: "route-1",
        enabled: true,
      }),
    );

    fireEvent.click(screen.getByText("Remove"));
    await waitFor(() =>
      expect(invoke).toHaveBeenCalledWith("channels_remove_route", { routeId: "route-1" }),
    );
  });
});
