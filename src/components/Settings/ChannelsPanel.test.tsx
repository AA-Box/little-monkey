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
import type { ChannelAccount } from "../../lib/channelsClient";

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

function mockAccounts(accounts: ChannelAccount[]) {
  invoke.mockImplementation((command: string) => {
    if (command === "channels_list") return Promise.resolve({ accounts });
    if (command === "channels_routes") return Promise.resolve({ routes: [{}] });
    if (command === "channels_senders") return Promise.resolve({ pending: [] });
    if (command === "channels_events") return Promise.resolve({ events: [] });
    return Promise.resolve(null);
  });
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
});
