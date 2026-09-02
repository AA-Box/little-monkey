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
vi.mock("@tauri-apps/api/core", () => ({
  invoke: (...args: unknown[]) => invoke(...args),
  isTauri: () => true,
}));
vi.mock("@tauri-apps/plugin-opener", () => ({ openUrl: vi.fn() }));

import { ChannelsPanel } from "./ChannelsPanel";
import type {
  ChannelAccount,
  ChannelCallback,
  ChannelEvent,
  ChannelRoute,
  ChannelSenders,
  ExposureStatus,
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
  callback_rejections: { count: 0, last_reason: null, last_at_ms: null },
  echo_correlation: "host_adapter",
  reply_policy_restricted: false,
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

/** One saved task, as `recipes_list` reports it. */
function savedTask(name: string) {
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

function mockChannels(options: {
  accounts: ChannelAccount[];
  routes?: ChannelRoute[];
  events?: ChannelEvent[];
  callback?: ChannelCallback;
  exposure?: ExposureStatus | null;
  recipes?: string[];
  senders?: ChannelSenders;
  onCommand?: (command: string, args: unknown) => unknown;
}) {
  invoke.mockImplementation((command: string, args: unknown) => {
    const custom = options.onCommand?.(command, args);
    if (custom !== undefined) return custom;
    if (command === "channels_list") return Promise.resolve({ accounts: options.accounts });
    if (command === "channels_exposure_status") {
      // `null` means a daemon that cannot answer, which must leave the card
      // absent rather than taking the panel down.
      return options.exposure === null
        ? Promise.reject(new Error("no daemon"))
        : Promise.resolve(options.exposure ?? MANUAL_EXPOSURE);
    }
    if (command === "channels_routes") return Promise.resolve({ routes: options.routes ?? [ROUTE] });
    if (command === "channels_senders") {
      return Promise.resolve(options.senders ?? { pending: [], approved: [], blocked: [] });
    }
    // The route editor offers saved tasks rather than a typed name, so it
    // lists them on mount like Settings > Tasks does — a route can only name
    // a task that exists.
    if (command === "recipes_list") {
      return Promise.resolve((options.recipes ?? ["chat", "triage", "triage-2"]).map(savedTask));
    }
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

const MANUAL_EXPOSURE: ExposureStatus = {
  mode: "manual",
  state: "not_configured",
  credentialStored: false,
  restarts: 0,
};

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
    // The sender rung is account + conversation + thread + sender, all four:
    // the daemon refuses anything less, so the form cannot submit it either.
    fireEvent.change(screen.getByLabelText("Thread"), { target: { value: "thread-2" } });
    fireEvent.click(screen.getByText("Save route"));

    await waitFor(() =>
      expect(invoke).toHaveBeenCalledWith("channels_add_route", {
        recipe: "triage",
        options: expect.objectContaining({
          account_id: "chan-1",
          conversation_id: "conv-7",
          thread_id: "thread-2",
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
      recipes: ["chat", "chat-2"],
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

describe("ChannelsPanel state matrix", () => {
  it("says there is nothing configured rather than showing an empty list", async () => {
    mockAccounts([]);
    render(<ChannelsPanel />);
    expect(await screen.findByText(/no accounts yet/i)).toBeTruthy();
  });

  it("says it is loading before the first answer, not that there is nothing", async () => {
    let release: (value: unknown) => void = () => {};
    invoke.mockImplementation((command: string) => {
      if (command === "channels_list")
        return new Promise((resolve) => (release = resolve));
      if (command === "recipes_list") return Promise.resolve([]);
      return Promise.resolve(null);
    });
    render(<ChannelsPanel />);
    // The distinction that matters: "still asking" must not read as "none".
    expect(screen.queryByText(/no accounts yet/i)).toBeNull();
    expect(screen.getByText(/loading messaging channels/i)).toBeTruthy();
    release({ accounts: [] });
    await waitFor(() =>
      expect(screen.queryByText(/no accounts yet/i)).toBeTruthy(),
    );
  });

  it("renders each health state as its own word", async () => {
    // The panel's own words, which is the point: five states that render the
    // same sentence are five states an operator cannot tell apart.
    const states: Array<[ChannelAccount["health"], RegExp]> = [
      ["connected", /^Connected$/],
      ["connecting", /^Connecting$/],
      ["degraded", /^Degraded$/],
      ["disconnected", /^Not checked yet$/],
      ["unsupported", /^Not supported here$/],
      ["unconfigured", /^Not set up$/],
      ["error", /^Error$/],
    ];
    for (const [health, shown] of states) {
      cleanup();
      mockAccounts([{ ...BASE, enabled: true, health }]);
      render(<ChannelsPanel />);
      expect(await screen.findByText(shown)).toBeTruthy();
    }
  });

  it("asks for the credential it is missing instead of reporting a failure", async () => {
    mockAccounts([
      {
        ...BASE,
        kind: "telegram",
        label: "Team Telegram",
        enabled: true,
        credential_required: true,
        has_credential: false,
        non_secret_config: {},
      },
    ]);
    render(<ChannelsPanel />);
    fireEvent.click(await screen.findByText("Team Telegram"));

    await waitFor(() =>
      expect(screen.getByText(/No credential saved yet/)).toBeTruthy(),
    );
    // And the box to put one in, so the state is actionable rather than a
    // statement about itself.
    expect(screen.getByText("Save credential")).toBeTruthy();
  });

  it("offers a retry that reaches the provider, not just a red line", async () => {
    mockAccounts([
      {
        ...BASE,
        enabled: true,
        health: "error",
        last_error: "gateway refused",
      },
    ]);
    render(<ChannelsPanel />);
    fireEvent.click(await screen.findByText("Family Signal"));
    expect(await screen.findByText("gateway refused")).toBeTruthy();
    const probe = await screen.findByRole("button", {
      name: /test connection/i,
    });
    fireEvent.click(probe);
    await waitFor(() =>
      expect(
        invoke.mock.calls.some(([command]) => command === "channels_probe"),
      ).toBe(true),
    );
  });

  /**
   * Approval used to be a one-way door: the panel listed who was waiting and
   * nothing else, so an approved person could neither be seen nor shown out.
   * The approved list names them — by the name they arrived with, with the id
   * beside it — and "Revoke access" is the existing block decision, which is
   * what "they can no longer use this" means; forgetting them would hand them a
   * fresh pairing code instead.
   */
  it("lists approved senders by name and revokes one by blocking them", async () => {
    mockChannels({
      accounts: [BASE],
      senders: {
        pending: [],
        approved: [
          {
            sender_id: "931819457",
            state: "approved",
            display_label: "ahmad",
            requested_at_ms: 1,
            expires_at_ms: null,
            model: "managed:qwen2.5-7b",
          },
        ],
        blocked: [],
      },
    });
    render(<ChannelsPanel />);
    fireEvent.click(await screen.findByText("Family Signal"));

    expect(await screen.findByText("ahmad")).toBeTruthy();
    expect(screen.getByText("931819457")).toBeTruthy();
    // The model they picked for themselves rides along, so the operator can
    // see who is answered on what.
    expect(screen.getByText(/managed:qwen2\.5-7b/)).toBeTruthy();
    fireEvent.click(screen.getByRole("button", { name: "Revoke access" }));

    await waitFor(() =>
      expect(invoke).toHaveBeenCalledWith("channels_decide_sender", {
        accountId: "chan-1",
        senderId: "931819457",
        approve: false,
      }),
    );
  });

  /**
   * Forgetting is the other way out, and the opposite of blocking: the row
   * goes, so the person's next message is a stranger's and earns a fresh
   * pairing code. It is what an operator wants for someone they let in by
   * mistake and would let in again — and it is offered on a blocked sender
   * too, which is how a block is undone without approving them outright.
   */
  it("forgets a sender so they can pair again", async () => {
    mockChannels({
      accounts: [BASE],
      senders: {
        pending: [],
        approved: [],
        blocked: [
          {
            sender_id: "555",
            state: "blocked",
            display_label: "Bo",
            requested_at_ms: 1,
            expires_at_ms: null,
            model: null,
          },
        ],
      },
    });
    render(<ChannelsPanel />);
    fireEvent.click(await screen.findByText("Family Signal"));

    expect(await screen.findByText("Bo")).toBeTruthy();
    fireEvent.click(screen.getByRole("button", { name: "Forget" }));

    await waitFor(() =>
      expect(invoke).toHaveBeenCalledWith("channels_forget_sender", {
        accountId: "chan-1",
        senderId: "555",
      }),
    );
  });

  it("offers disable and remove on an account that has one", async () => {
    mockAccounts([{ ...BASE, enabled: true }]);
    render(<ChannelsPanel />);
    fireEvent.click(await screen.findByText("Family Signal"));
    // `Disable` is also a policy value in this panel, so the account's own
    // control is asserted by presence rather than by uniqueness.
    await waitFor(() =>
      expect(
        screen.getAllByRole("button", { name: "Disable" }).length,
      ).toBeGreaterThan(0),
    );
    expect(
      screen.getAllByRole("button", { name: "Remove" }).length,
    ).toBeGreaterThan(0);
  });

  /**
   * The state this PR added, and the reason it had to exist: a provider whose
   * deliveries stop authenticating produces no event, no health change and no
   * error — the messages simply stop. Without this banner the page is honest
   * about everything except the one thing that is wrong.
   */
  it("names a run of refused deliveries and what to check", async () => {
    mockAccounts([
      {
        ...BASE,
        kind: "whatsapp",
        enabled: true,
        health: "connected",
        callback_rejections: {
          count: 4,
          last_reason: "WhatsApp webhook signature verification failed",
          last_at_ms: 1_800_000_000_000,
        },
      },
    ]);
    render(<ChannelsPanel />);
    fireEvent.click(await screen.findByText("Family Signal"));
    expect(await screen.findByText(/4 delivery attempt/i)).toBeTruthy();
    expect(screen.getByText(/signature verification failed/i)).toBeTruthy();
    // Actionable: the banner itself names the two things to compare, rather
    // than only saying that something failed.
    expect(
      screen.getByText(
        /callback URL in the provider's console matches the one shown here/i,
      ),
    ).toBeTruthy();
  });

  it("says nothing about refusals when there have been none", async () => {
    mockAccounts([{ ...BASE, enabled: true, health: "connected" }]);
    render(<ChannelsPanel />);
    fireEvent.click(await screen.findByText("Family Signal"));
    await screen.findByText(/^Connected$/);
    expect(screen.queryByText(/delivery attempt/i)).toBeNull();
  });
});

/**
 * The public-callback card, and what it may never claim.
 *
 * A tunnel that is configured and dead is the exact failure this whole feature
 * exists to make visible, so the interesting assertions are all about the panel
 * refusing to render "fine" — and about the secret having nowhere to appear.
 */
describe("the public callback card", () => {
  const MANAGED: ExposureStatus = {
    mode: "managed_tunnel",
    provider: "cloudflared",
    state: "connected",
    publicBase: "https://monkey.example.com",
    credentialStored: true,
    executable: "/usr/local/bin/cloudflared",
    restarts: 0,
  };

  it("shows the URL in force and the state the daemon reported", async () => {
    mockChannels({ accounts: [BASE], exposure: MANAGED });
    render(<ChannelsPanel />);

    await waitFor(() => expect(screen.getByTestId("exposure")).toBeTruthy());
    const shown = screen.getByTestId("exposure-state").textContent ?? "";
    // The transport, and only the transport. `/ready` says the client holds a
    // connection to its provider's edge; whether a request to the hostname
    // arrives at this machine also needs that hostname's route and origin
    // service to be right, and both are set in the operator's dashboard where
    // nothing here can look. A card that promised the end-to-end path from the
    // half it can see would be the same false confidence as calling a dead
    // tunnel healthy.
    expect(shown).toMatch(/Tunnel connected to its provider's edge/);
    expect(shown).toMatch(/hostname must also point at this machine/);
    expect(shown).not.toMatch(/reach this machine/);
    expect(screen.getByText("https://monkey.example.com")).toBeTruthy();
  });

  it("names each failure as the thing the operator has to fix", async () => {
    const cases: Array<[ExposureStatus["state"], RegExp]> = [
      ["helper_missing", /tunnel client is not at the path/],
      ["credential_missing", /No tunnel token is stored/],
      ["authentication_failed", /rejected the stored token/],
      ["public_url_unavailable", /No hostname is set/],
      ["reconnecting", /Reconnecting after 2 restarts/],
      ["degraded", /has not reported a live connection/],
      ["stopped", /Nothing is exposing this machine/],
      ["connecting", /Starting your tunnel/],
    ];
    for (const [state, expected] of cases) {
      cleanup();
      mockChannels({
        accounts: [BASE],
        exposure: { ...MANAGED, state, restarts: 2 },
      });
      render(<ChannelsPanel />);
      await waitFor(() => expect(screen.getByTestId("exposure-state").textContent).toMatch(expected));
    }
  });

  it("never renders a credential, and never asks React to hold one", async () => {
    mockChannels({
      accounts: [BASE],
      exposure: { ...MANAGED, state: "authentication_failed", lastError: "401 Unauthorized" },
    });
    render(<ChannelsPanel />);

    await waitFor(() => expect(screen.getByTestId("exposure-error").textContent).toBe("401 Unauthorized"));
    // The whole rendered panel, searched for anything token-shaped. The status
    // type has no field for one, and this is the assertion that keeps it that
    // way if somebody adds one.
    expect(document.body.textContent).not.toMatch(/eyJ|token=|secret/i);
  });

  it("does not take the panel down when the background service cannot answer", async () => {
    mockChannels({ accounts: [BASE], exposure: null });
    render(<ChannelsPanel />);

    await waitFor(() => expect(screen.getByText("Family Signal")).toBeTruthy());
    expect(screen.queryByTestId("exposure")).toBeNull();
  });

  it("sends a fixed provider name and nothing that could be a command", async () => {
    const calls: Array<[string, unknown]> = [];
    mockChannels({
      accounts: [BASE],
      exposure: { ...MANAGED, mode: "manual", state: "not_configured" },
      onCommand: (command, args) => {
        if (command.startsWith("channels_exposure_set")) {
          calls.push([command, args]);
          return Promise.resolve(null);
        }
        return undefined;
      },
    });
    render(<ChannelsPanel />);

    await waitFor(() => expect(screen.getByTestId("exposure")).toBeTruthy());
    fireEvent.change(screen.getByLabelText(/Tunnel hostname/), {
      target: { value: "monkey.example.com" },
    });
    fireEvent.change(screen.getByLabelText(/Path to cloudflared/), {
      target: { value: "/usr/local/bin/cloudflared" },
    });
    fireEvent.click(screen.getByText("Connect"));

    await waitFor(() => expect(calls.length).toBe(1));
    expect(calls[0][0]).toBe("channels_exposure_set_tunnel");
    expect(calls[0][1]).toEqual({
      provider: "cloudflared",
      hostname: "monkey.example.com",
      executable: "/usr/local/bin/cloudflared",
      metricsPort: null,
    });
  });
});

/**
 * An extension-backed account that cannot recognise its own echo.
 *
 * The two loop-capable settings are refused by the daemon whatever the panel
 * does; what the panel owes the operator is saying so, rather than rendering a
 * dropdown whose selection is silently narrowed.
 */
describe("an account that cannot recognise its own messages", () => {
  const EXTENSION: ChannelAccount = {
    ...BASE,
    account_id: "chan-ext",
    kind: "extension",
    label: "Fixture channel",
    enabled: true,
    credential_required: false,
    non_secret_config: { extension_id: "dev.example.chat", capability_id: "room" },
    echo_correlation: "unsupported",
    reply_policy_restricted: false,
  };

  it("says why, and refuses the two settings that could loop", async () => {
    mockChannels({ accounts: [EXTENSION] });
    render(<ChannelsPanel />);
    fireEvent.click(await screen.findByText("Fixture channel"));

    await waitFor(() => expect(screen.getByTestId("echo-blind")).toBeTruthy());
    const open = screen.getAllByRole("option", { name: "Anyone" }) as HTMLOptionElement[];
    expect(open.length).toBeGreaterThan(0);
    expect(open.every((option) => option.disabled)).toBe(true);
    const always = screen.getByRole("option", { name: "Every message" }) as HTMLOptionElement;
    expect(always.disabled).toBe(true);
  });

  it("says nothing at all about a built-in provider", async () => {
    mockChannels({ accounts: [{ ...BASE, enabled: true }] });
    render(<ChannelsPanel />);
    fireEvent.click(await screen.findByText("Family Signal"));

    await waitFor(() => expect(screen.getByText(/Direct messages/)).toBeTruthy());
    expect(screen.queryByTestId("echo-blind")).toBeNull();
    const always = screen.getByRole("option", { name: "Every message" }) as HTMLOptionElement;
    expect(always.disabled).toBe(false);
  });
});
