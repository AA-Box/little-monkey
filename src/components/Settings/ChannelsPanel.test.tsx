// @vitest-environment jsdom
/**
 * The Channels panel's setup path, driven the way an operator drives it.
 *
 * Three things here are not cosmetic. A provider whose account lives in a
 * helper (Signal, iMessage) must not be shown a credential box — pasting a
 * secret into one that nothing reads is how an operator concludes the app is
 * broken. A provider configured against the operator's own server (Matrix,
 * Mattermost, IRC) must be able to say where that server is without
 * hand-writing JSON. And health has to read as what was last *probed*, never
 * as "a token is saved, so presumably fine".
 */
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";

const invoke = vi.fn();
vi.mock("@tauri-apps/api/core", () => ({ invoke: (...args: unknown[]) => invoke(...args) }));
vi.mock("@tauri-apps/plugin-opener", () => ({ openUrl: vi.fn() }));

import { ChannelsPanel } from "./ChannelsPanel";
import type {
  ChannelAccount,
  ChannelCallback,
  ChannelEvent,
  ChannelRoute,
} from "../../lib/channelsClient";

const BASE: ChannelAccount = {
  account_id: "chan-1",
  kind: "signal",
  label: "Family Signal",
  enabled: false,
  has_credential: false,
  credential_required: false,
  access_policy: { direct: "pairing", group: "allow_list", group_activation: "mention_only" },
  health: "disconnected",
  health_detail: null,
  last_error: null,
  last_probe_at_ms: 0,
  non_secret_config: { helper_path: "/usr/local/bin/signal-cli", account: "+15550000000" },
  created_at_ms: 0,
  updated_at_ms: 0,
};

const ROUTE: ChannelRoute = {
  route_id: "route-1",
  scope: {},
  target: {
    recipe: "chat",
    session_scope: "thread",
    priority: 0,
    reply_to_conversation: true,
  },
  enabled: true,
  created_at_ms: 0,
  updated_at_ms: 0,
};

function mockChannels(options: {
  accounts: ChannelAccount[];
  routes?: ChannelRoute[];
  events?: ChannelEvent[];
  callback?: ChannelCallback;
  onCommand?: (command: string, args: unknown) => unknown;
}) {
  invoke.mockImplementation((command: string, args: unknown) => {
    const custom = options.onCommand?.(command, args);
    if (custom !== undefined) return custom;
    if (command === "channels_list") return Promise.resolve({ accounts: options.accounts });
    if (command === "channels_routes") return Promise.resolve({ routes: options.routes ?? [ROUTE] });
    if (command === "channels_senders") return Promise.resolve({ pending: [] });
    if (command === "channels_events") return Promise.resolve({ events: options.events ?? [] });
    if (command === "channels_callback_url") {
      return Promise.resolve(
        options.callback ?? {
          account_id: "chan-1",
          configured: false,
          url: null,
          path: "/v1/channels/chan-1",
        },
      );
    }
    return Promise.resolve(null);
  });
}

function mockAccounts(accounts: ChannelAccount[]) {
  mockChannels({ accounts });
}

beforeEach(() => {
  invoke.mockReset();
  mockAccounts([BASE]);
});

afterEach(() => {
  cleanup();
  vi.restoreAllMocks();
});

describe("ChannelsPanel", () => {
  it("offers no credential box for a provider whose helper holds the account", async () => {
    render(<ChannelsPanel />);
    fireEvent.click(await screen.findByText("Family Signal"));

    await waitFor(() => expect(screen.getByText(/helper you installed holds the account/)).toBeTruthy());
    expect(screen.queryByText("Save credential")).toBeNull();
    // And it must not claim a credential is missing, which is what the
    // generic warning would have said.
    expect(screen.queryByText(/No credential saved yet/)).toBeNull();
  });

  it("shows what the last probe reported, not what is configured", async () => {
    mockAccounts([
      {
        ...BASE,
        kind: "matrix",
        label: "Home Matrix",
        credential_required: true,
        has_credential: true,
        health: "degraded",
        health_detail: "@you:example.org · 3 encrypted events skipped",
        last_error: "sync timed out",
        non_secret_config: { homeserver_url: "https://matrix.example.org", user_id: "@you:example.org" },
      },
    ]);
    render(<ChannelsPanel />);
    fireEvent.click(await screen.findByText("Home Matrix"));

    await waitFor(() =>
      expect(screen.getByText("@you:example.org · 3 encrypted events skipped")).toBeTruthy(),
    );
    expect(screen.getByText("sync timed out")).toBeTruthy();
    // The homeserver it is configured against is shown by its label, so the
    // operator can see which server this account talks to.
    expect(screen.getByText("Homeserver")).toBeTruthy();
    expect(screen.getByText("https://matrix.example.org")).toBeTruthy();
  });

  it("collects a server's settings as typed fields and sends them as one object", async () => {
    render(<ChannelsPanel />);
    await screen.findByText("Family Signal");

    fireEvent.change(screen.getByLabelText("Provider"), { target: { value: "irc" } });
    fireEvent.change(screen.getByLabelText("Name"), { target: { value: "Libera" } });
    fireEvent.change(screen.getByLabelText(/^Server/), { target: { value: "irc.libera.chat" } });
    fireEvent.change(screen.getByLabelText(/^Port/), { target: { value: "6697" } });
    fireEvent.change(screen.getByLabelText(/^Nickname/), { target: { value: "monkey" } });
    fireEvent.change(screen.getByLabelText("Channels to join"), { target: { value: "#one, #two" } });
    fireEvent.click(screen.getByText("Add account"));

    await waitFor(() =>
      expect(invoke).toHaveBeenCalledWith("channels_add", {
        kind: "irc",
        label: "Libera",
        config: JSON.stringify({
          server: "irc.libera.chat",
          port: 6697,
          nick: "monkey",
          channels: ["#one", "#two"],
        }),
      }),
    );
  });

  it("will not add an account whose required settings are blank", async () => {
    render(<ChannelsPanel />);
    await screen.findByText("Family Signal");

    fireEvent.change(screen.getByLabelText("Provider"), { target: { value: "mattermost" } });
    fireEvent.change(screen.getByLabelText("Name"), { target: { value: "Work" } });
    // The server URL is what the adapter refuses to build without, so the form
    // says so here rather than letting the daemon fail after the fact.
    expect(screen.getByText(/Still needed:/)).toBeTruthy();
    fireEvent.click(screen.getByText("Add account"));
    expect(invoke).not.toHaveBeenCalledWith("channels_add", expect.anything());
  });

  it("says iMessage is macOS-only while it is being set up", async () => {
    render(<ChannelsPanel />);
    await screen.findByText("Family Signal");

    fireEvent.change(screen.getByLabelText("Provider"), { target: { value: "imessage" } });
    expect(screen.getByText(/macOS only/)).toBeTruthy();
  });

  it("edits an existing account's settings without touching its credential", async () => {
    mockAccounts([
      {
        ...BASE,
        kind: "mattermost",
        label: "Work",
        credential_required: true,
        has_credential: true,
        health: "connected",
        // A per-account attachment knob set from the terminal: the panel now
        // has a typed input for it, and an unrelated edit must carry it over.
        non_secret_config: { base_url: "https://old.example.com", max_attachment_bytes: 2048 },
      },
    ]);
    render(<ChannelsPanel />);
    fireEvent.click(await screen.findByText("Work"));

    fireEvent.click(await screen.findByText("Edit settings"));
    fireEvent.change(await screen.findByLabelText(/^Server URL/), {
      target: { value: "https://new.example.com" },
    });
    fireEvent.click(screen.getByText("Save settings"));

    await waitFor(() =>
      expect(invoke).toHaveBeenCalledWith("channels_set_config", {
        accountId: "chan-1",
        // The already-configured limit survives a wholesale replacement, and
        // no secret travels in either direction.
        config: JSON.stringify({ base_url: "https://new.example.com", max_attachment_bytes: 2048 }),
        label: "Work",
      }),
    );
  });

  it("edits the per-account attachment limits as typed fields, never as JSON", async () => {
    mockAccounts([
      {
        ...BASE,
        kind: "mattermost",
        label: "Work",
        credential_required: true,
        has_credential: true,
        non_secret_config: { base_url: "https://chat.example.com" },
      },
    ]);
    render(<ChannelsPanel />);
    fireEvent.click(await screen.findByText("Work"));

    fireEvent.click(await screen.findByText("Edit settings"));
    fireEvent.change(await screen.findByLabelText(/^Max attachment size/), {
      target: { value: "1048576" },
    });
    fireEvent.click(screen.getByText("Save settings"));

    await waitFor(() =>
      expect(invoke).toHaveBeenCalledWith("channels_set_config", {
        accountId: "chan-1",
        // Typed by the field's declared kind: the daemon parses a number, so
        // a number is what travels.
        config: JSON.stringify({ base_url: "https://chat.example.com", max_attachment_bytes: 1048576 }),
        label: "Work",
      }),
    );
  });

  it("shows a webhook provider's complete callback URL, as the daemon composed it", async () => {
    mockChannels({
      accounts: [{ ...BASE, kind: "whatsapp", label: "Support", credential_required: true }],
      callback: {
        account_id: "chan-1",
        configured: true,
        url: "https://hooks.example.com/v1/channels/chan-1",
        path: "/v1/channels/chan-1",
      },
    });
    render(<ChannelsPanel />);
    fireEvent.click(await screen.findByText("Support"));

    await waitFor(() =>
      expect(screen.getByText("https://hooks.example.com/v1/channels/chan-1")).toBeTruthy(),
    );
    expect(screen.getByText("Copy")).toBeTruthy();
  });

  it("says plainly when no public URL is configured instead of showing half of one", async () => {
    mockChannels({
      accounts: [{ ...BASE, kind: "whatsapp", label: "Support", credential_required: true }],
    });
    render(<ChannelsPanel />);
    fireEvent.click(await screen.findByText("Support"));

    await waitFor(() => expect(screen.getByText(/No public URL is configured/)).toBeTruthy());
    fireEvent.change(screen.getByLabelText("Public base URL"), {
      target: { value: "https://hooks.example.com" },
    });
    fireEvent.click(screen.getByText("Save"));
    await waitFor(() =>
      expect(invoke).toHaveBeenCalledWith("channels_set_public_url", {
        url: "https://hooks.example.com",
      }),
    );
  });
});

describe("route management", () => {
  it("configures the most specific rung of the ladder from observed ids", async () => {
    mockChannels({
      accounts: [{ ...BASE, label: "Family Signal" }],
      routes: [ROUTE],
      events: [
        {
          event_id: "evt-1",
          direction: "inbound",
          conversation_id: "conv-7",
          thread_id: "thread-2",
          sender_id: "+15550000000",
          disposition: "accepted",
          ignore_reason: null,
          job_id: null,
          received_at_ms: 0,
        },
      ],
    });
    render(<ChannelsPanel />);
    // The default route already there is listed with the rung it sits on.
    expect(await screen.findByText("Everything")).toBeTruthy();

    fireEvent.click(screen.getByText("Add route"));
    fireEvent.change(screen.getByLabelText("Task"), { target: { value: "triage" } });
    fireEvent.change(screen.getByLabelText("Applies to"), { target: { value: "sender" } });
    fireEvent.change(await screen.findByLabelText("Account"), { target: { value: "chan-1" } });
    fireEvent.change(await screen.findByLabelText("Conversation"), { target: { value: "conv-7" } });
    fireEvent.change(screen.getByLabelText("Sender"), { target: { value: "+15550000000" } });
    fireEvent.click(screen.getByText("Save route"));

    await waitFor(() =>
      expect(invoke).toHaveBeenCalledWith("channels_add_route", {
        recipe: "triage",
        options: expect.objectContaining({
          account_id: "chan-1",
          conversation_id: "conv-7",
          sender_id: "+15550000000",
          kind: null,
        }),
      }),
    );
  });

  it("shows the daemon's ambiguity refusal rather than inventing its own rule", async () => {
    mockChannels({
      accounts: [{ ...BASE, label: "Family Signal" }],
      onCommand: (command) =>
        command === "channels_add_route"
          ? Promise.reject(
              "Route 'route-1' already owns this scope: two routes at the same specificity would be ambiguous for any message matching both",
            )
          : undefined,
    });
    render(<ChannelsPanel />);
    fireEvent.click(await screen.findByText("Add route"));
    fireEvent.change(screen.getByLabelText("Task"), { target: { value: "chat" } });
    fireEvent.click(screen.getByText("Save route"));

    await waitFor(() => expect(screen.getByText(/already owns this scope/)).toBeTruthy());
  });

  it("sends a route's parameters exactly as typed, one name=value each", async () => {
    mockChannels({ accounts: [{ ...BASE, label: "Family Signal" }] });
    render(<ChannelsPanel />);
    fireEvent.click(await screen.findByText("Add route"));
    fireEvent.change(screen.getByLabelText("Task"), { target: { value: "triage" } });

    fireEvent.click(screen.getByText("Add parameter"));
    fireEvent.change(screen.getByLabelText("Parameter"), { target: { value: "focus" } });
    fireEvent.change(screen.getByLabelText("Value"), { target: { value: "deps" } });
    fireEvent.click(screen.getByText("Save route"));

    await waitFor(() =>
      expect(invoke).toHaveBeenCalledWith("channels_add_route", {
        recipe: "triage",
        options: expect.objectContaining({ params: ["focus=deps"] }),
      }),
    );
  });

  it("refuses a parameter with no name while the operator is still looking at it", async () => {
    mockChannels({ accounts: [{ ...BASE, label: "Family Signal" }] });
    render(<ChannelsPanel />);
    fireEvent.click(await screen.findByText("Add route"));
    fireEvent.change(screen.getByLabelText("Task"), { target: { value: "triage" } });

    fireEvent.click(screen.getByText("Add parameter"));
    expect(await screen.findByText("Every parameter needs a name.")).toBeTruthy();
    fireEvent.click(screen.getByText("Save route"));
    expect(invoke).not.toHaveBeenCalledWith("channels_add_route", expect.anything());
  });

  it("refuses two parameters with the same name instead of silently keeping one", async () => {
    mockChannels({ accounts: [{ ...BASE, label: "Family Signal" }] });
    render(<ChannelsPanel />);
    fireEvent.click(await screen.findByText("Add route"));
    fireEvent.change(screen.getByLabelText("Task"), { target: { value: "triage" } });

    fireEvent.click(screen.getByText("Add parameter"));
    fireEvent.click(screen.getByText("Add parameter"));
    const names = screen.getAllByLabelText("Parameter");
    fireEvent.change(names[0], { target: { value: "focus" } });
    fireEvent.change(names[1], { target: { value: "focus" } });
    expect(await screen.findByText("Two parameters have the same name.")).toBeTruthy();
    fireEvent.click(screen.getByText("Save route"));
    expect(invoke).not.toHaveBeenCalledWith("channels_add_route", expect.anything());
  });

  it("loads an existing route's parameters and carries them through an unrelated edit", async () => {
    mockChannels({
      accounts: [{ ...BASE, label: "Family Signal" }],
      routes: [
        {
          ...ROUTE,
          target: { ...ROUTE.target, params: { focus: "deps", depth: "3" } },
        },
      ],
    });
    render(<ChannelsPanel />);
    await screen.findByText("Everything");

    fireEvent.click(screen.getAllByText("Edit")[0]);
    // Both stored parameters are on screen, loaded from the route.
    const names = screen.getAllByLabelText("Parameter") as HTMLInputElement[];
    expect(names.map((input) => input.value).sort()).toEqual(["depth", "focus"]);

    // An edit that never touches the parameters still sends them all back:
    // the daemon replaces the target wholesale, so what the form forgets
    // would be gone.
    fireEvent.change(screen.getByLabelText("Task"), { target: { value: "chat-2" } });
    fireEvent.click(screen.getByText("Save route"));
    await waitFor(() =>
      expect(invoke).toHaveBeenCalledWith("channels_update_route", {
        routeId: "route-1",
        recipe: "chat-2",
        options: expect.objectContaining({
          params: expect.arrayContaining(["focus=deps", "depth=3"]),
        }),
      }),
    );
  });

  it("removes exactly the parameter whose row was deleted", async () => {
    mockChannels({
      accounts: [{ ...BASE, label: "Family Signal" }],
      routes: [
        {
          ...ROUTE,
          target: { ...ROUTE.target, params: { focus: "deps", depth: "3" } },
        },
      ],
    });
    render(<ChannelsPanel />);
    await screen.findByText("Everything");

    fireEvent.click(screen.getAllByText("Edit")[0]);
    const removeButtons = await screen.findAllByLabelText("Remove parameter");
    const names = screen.getAllByLabelText("Parameter") as HTMLInputElement[];
    const removeAt = names.findIndex((input) => input.value === "focus");
    fireEvent.click(removeButtons[removeAt]);
    fireEvent.click(screen.getByText("Save route"));

    await waitFor(() =>
      expect(invoke).toHaveBeenCalledWith("channels_update_route", {
        routeId: "route-1",
        recipe: "chat",
        options: expect.objectContaining({ params: ["depth=3"] }),
      }),
    );
  });

  it("turns a route off without editing what it routes to", async () => {
    mockChannels({ accounts: [{ ...BASE, label: "Family Signal" }] });
    render(<ChannelsPanel />);
    await screen.findByText("Everything");

    fireEvent.click(screen.getAllByText("Disable")[0]);
    await waitFor(() =>
      expect(invoke).toHaveBeenCalledWith("channels_enable_route", {
        routeId: "route-1",
        enabled: false,
      }),
    );
  });
});
