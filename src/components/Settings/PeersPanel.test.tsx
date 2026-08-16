// @vitest-environment jsdom
/**
 * The Peers panel, driven the way an operator drives it.
 *
 * The load-bearing claims here are about authority, not layout. A pairing must
 * never read as trust: the panel says what a peer may *do*, separately from the
 * fact that it is cryptographically paired, separately again from what that
 * peer merely *asked* for, and it says so out loud when the same credential is
 * also a controller device. Granting and revoking have to reach the typed
 * bridge with exactly what the operator chose — nothing here may quietly widen
 * a grant — and every destructive action has to be deliberate rather than one
 * stray click.
 */
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";

const invoke = vi.fn();
vi.mock("@tauri-apps/api/core", () => ({ invoke: (...args: unknown[]) => invoke(...args) }));

const openDialog = vi.fn();
const saveDialog = vi.fn();
vi.mock("@tauri-apps/plugin-dialog", () => ({
  open: (...args: unknown[]) => openDialog(...args),
  save: (...args: unknown[]) => saveDialog(...args),
}));

import { PeersPanel } from "./PeersPanel";
import type { InboundPeer, OutboundPeer, PeerThread } from "../../lib/peersClient";

const PEER: InboundPeer = {
  device_id: "device-1",
  label: "Studio desktop",
  grants: ["message"],
  advertised_grants: ["message", "task", "artifact"],
  requested_grants: ["task"],
  state: "active",
  peer_only: true,
  last_sequence: 4,
  last_seen_at_ms: 1_700_000_000_000,
  presence: "online",
  secret_generation: 2,
};

const OUTBOUND: OutboundPeer = {
  alias: "studio",
  peer_id: "runner-two",
  peer_url: "https://studio.invalid",
  grants: ["message", "task"],
  advertised_grants: ["message", "task", "artifact"],
  requested_grants: [],
  certificate_sha256: "ab".repeat(32),
  last_seen_at_ms: null,
  presence: "unknown",
  secret_generation: 1,
};

function mock(options: {
  inbound?: InboundPeer[];
  outbound?: OutboundPeer[];
  threads?: PeerThread[];
}) {
  invoke.mockImplementation((command: string) => {
    if (command === "peers_list") {
      return Promise.resolve({
        inbound: options.inbound ?? [PEER],
        outbound: options.outbound ?? [],
      });
    }
    if (command === "peers_threads") {
      return Promise.resolve({ threads: options.threads ?? [], recipe: "peer-task" });
    }
    if (command === "peers_grant") {
      return Promise.resolve({ device_id: "device-1", grants: [] });
    }
    if (command === "peers_revoke" || command === "peers_forget") return Promise.resolve(undefined);
    if (command === "peers_rotate") {
      return Promise.resolve({ device_id: "device-1", secret_generation: 3, output: "/tmp/rot.json" });
    }
    if (command === "peers_accept_rotation") {
      return Promise.resolve({ alias: "studio", secret_generation: 3, certificate_sha256: "cd".repeat(32) });
    }
    if (command === "peers_clear") {
      return Promise.resolve({ device_id: "device-1", threads_removed: 2, grants_cleared: true });
    }
    if (command === "peers_status") {
      return Promise.resolve({
        alias: "studio",
        peer_id: "runner-two",
        last_seen_at_ms: 1_700_000_100_000,
        presence: "online",
      });
    }
    if (command === "peers_invite") {
      return Promise.resolve({
        pairing_id: "pair-1",
        expires_at_ms: 1,
        grants: ["message"],
        output: "/tmp/invite.json",
      });
    }
    return Promise.resolve({});
  });
}

/**
 * The grant checkbox for one fieldset, found by its own label rather than by
 * position: the same grant names also appear as read-only text in the
 * "they support / they asked for / you granted" rows, and indexing into every
 * match silently picks one of those instead.
 */
function grantCheckbox(fieldsetIndex: number, label: string): HTMLInputElement {
  const fieldset = document.querySelectorAll("fieldset")[fieldsetIndex];
  const match = Array.from(fieldset.querySelectorAll("label")).find((element) =>
    element.textContent?.startsWith(label),
  );
  const input = match?.querySelector("input");
  if (!input) throw new Error(`no "${label}" checkbox in fieldset ${fieldsetIndex}`);
  return input as HTMLInputElement;
}

beforeEach(() => {
  invoke.mockReset();
  openDialog.mockReset();
  saveDialog.mockReset();
});
afterEach(cleanup);

describe("PeersPanel", () => {
  it("says what a peer may do, separately from the fact that it is paired", async () => {
    mock({});
    render(<PeersPanel />);

    expect(await screen.findByText("Studio desktop")).toBeTruthy();
    expect(screen.getByText("Paired as a peer only")).toBeTruthy();
    // Never the word "trusted", and never a claim of authority the grants do
    // not carry.
    expect(document.body.textContent).not.toContain("Full trust");
    expect(document.body.textContent).not.toContain("Trusted");
  });

  it("keeps what a peer asked for visibly apart from what it was granted", async () => {
    mock({});
    render(<PeersPanel />);
    await screen.findByText("Studio desktop");

    // Three separate rows, so an ask can never be read as an entitlement.
    expect(screen.getAllByText("They support").length).toBeGreaterThan(0);
    expect(screen.getAllByText("They asked for").length).toBeGreaterThan(0);
    expect(screen.getAllByText("You granted").length).toBeGreaterThan(0);
    // The peer asked for task; only message is granted, and only message is
    // ticked.
    expect(grantCheckbox(1, "Request work").checked).toBe(false);
    expect(grantCheckbox(1, "Send messages").checked).toBe(true);
  });

  it("reports presence and key generation from the peer's own record", async () => {
    mock({ inbound: [PEER], outbound: [OUTBOUND] });
    render(<PeersPanel />);
    await screen.findByText("Studio desktop");

    expect(screen.getByText("Answered recently")).toBeTruthy();
    // A peer nobody has reached yet is not "offline" — nothing has been tried.
    expect(screen.getByText("Never in touch")).toBeTruthy();
    expect(document.body.textContent).toContain("key generation 2");
  });

  it("warns when the same credential is also a controller device", async () => {
    mock({ inbound: [{ ...PEER, peer_only: false, grants: ["message", "task"] }] });
    render(<PeersPanel />);

    expect(await screen.findByText("Paired, and also a controller device")).toBeTruthy();
    expect(document.body.textContent).toContain("can do more here than a peer can");
  });

  it("sends exactly the grants the operator ticked", async () => {
    mock({});
    render(<PeersPanel />);
    await screen.findByText("Studio desktop");

    fireEvent.click(grantCheckbox(1, "Request work"));

    await waitFor(() => {
      expect(
        invoke.mock.calls.some(
          ([command, args]) =>
            command === "peers_grant" &&
            (args as { deviceId: string; allow: string[] }).deviceId === "device-1" &&
            JSON.stringify((args as { allow: string[] }).allow) === JSON.stringify(["message", "task"]),
        ),
      ).toBe(true);
    });
  });

  it("makes revoking take two deliberate clicks and says what it destroys", async () => {
    mock({});
    render(<PeersPanel />);
    await screen.findByText("Studio desktop");

    fireEvent.click(screen.getByText("Revoke"));
    expect(screen.getByText("Revoking severs the pairing and deletes its threads.")).toBeTruthy();
    expect(invoke.mock.calls.some(([command]) => command === "peers_revoke")).toBe(false);

    fireEvent.click(screen.getByText("Revoke peer"));
    await waitFor(() => {
      expect(invoke.mock.calls.some(([command]) => command === "peers_revoke")).toBe(true);
    });
  });

  it("makes clearing a peer's history take two deliberate clicks too", async () => {
    mock({});
    render(<PeersPanel />);
    await screen.findByText("Studio desktop");

    fireEvent.click(screen.getByText("Clear history"));
    expect(invoke.mock.calls.some(([command]) => command === "peers_clear")).toBe(false);
    fireEvent.click(screen.getByText("Clear"));
    await waitFor(() => {
      expect(invoke.mock.calls.some(([command]) => command === "peers_clear")).toBe(true);
    });
    expect(await screen.findByRole("status")).toBeTruthy();
  });

  it("writes a rotation bundle only to a path the operator picked", async () => {
    mock({});
    saveDialog.mockResolvedValue(null);
    render(<PeersPanel />);
    await screen.findByText("Studio desktop");

    // Cancelling the file dialog must not rotate anything: the replacement key
    // exists only in that file, so rotating without one would strand the peer.
    fireEvent.click(screen.getByText("Rotate key"));
    await waitFor(() => expect(saveDialog).toHaveBeenCalled());
    expect(invoke.mock.calls.some(([command]) => command === "peers_rotate")).toBe(false);

    saveDialog.mockResolvedValue("/tmp/rot.json");
    fireEvent.click(screen.getByText("Rotate key"));
    await waitFor(() => {
      expect(
        invoke.mock.calls.some(
          ([command, args]) =>
            command === "peers_rotate" &&
            (args as { deviceId: string; output: string }).output === "/tmp/rot.json",
        ),
      ).toBe(true);
    });
  });

  it("checks an outbound peer's reachability on demand and reports what came back", async () => {
    mock({ inbound: [], outbound: [OUTBOUND] });
    render(<PeersPanel />);
    await screen.findByText("studio");

    fireEvent.click(screen.getByText("Check now"));
    await waitFor(() => {
      expect(
        invoke.mock.calls.some(
          ([command, args]) =>
            command === "peers_status" && (args as { alias: string }).alias === "studio",
        ),
      ).toBe(true);
    });
    expect((await screen.findByRole("status")).textContent).toContain("Answered recently");
  });

  it("makes forgetting an outbound peer deliberate and honest about what it does not do", async () => {
    mock({ inbound: [], outbound: [OUTBOUND] });
    render(<PeersPanel />);
    await screen.findByText("studio");

    fireEvent.click(screen.getByText("Forget"));
    // Forgetting is local. Saying so is the difference between this and
    // revoking, which the operator cannot do on someone else's machine.
    expect(document.body.textContent).toContain("that peer's own decision");
    expect(invoke.mock.calls.some(([command]) => command === "peers_forget")).toBe(false);

    fireEvent.click(screen.getByText("Forget peer"));
    await waitFor(() => {
      expect(invoke.mock.calls.some(([command]) => command === "peers_forget")).toBe(true);
    });
  });

  it("shows an outbound peer's whole fingerprint, in readable groups", async () => {
    mock({ inbound: [], outbound: [OUTBOUND] });
    render(<PeersPanel />);

    const fingerprint = await screen.findByText(/abababab/);
    // Whole digest, grouped — a fingerprint you cannot compare completely is
    // decoration. The screen-reader label is stripped; the digest is not.
    expect(
      fingerprint.textContent?.replace("Certificate fingerprint:", "").replace(/\s/g, ""),
    ).toBe("ab".repeat(32));
    expect(document.body.textContent).toContain("They allow this installation to:");
  });

  it("surfaces a refused peer message on the thread that carried it", async () => {
    mock({
      threads: [
        {
          thread_id: "thread-1",
          peer_device_id: "device-1",
          peer_instance_id: "instance-remote",
          session_key: "peer:device-1:thread-1",
          created_at_ms: 1_700_000_000_000,
          last_activity_at_ms: 1_700_000_050_000,
          message_count: 2,
          recent: [
            {
              message_id: "msg-1",
              direction: "inbound",
              kind: "task_request",
              disposition: "rejected",
              rejection: "missing_capability",
              job_id: null,
              correlation_id: "corr-1",
              created_at_ms: 1_700_000_010_000,
            },
          ],
        },
      ],
    });
    render(<PeersPanel />);
    await screen.findByText("Studio desktop");

    fireEvent.click(screen.getByText("Recent threads"));
    expect(await screen.findByText("thread-1")).toBeTruthy();
    expect(document.body.textContent).toContain("something was refused");
    expect(document.body.textContent).toContain("missing_capability");
    // The sender's own handle, so an operator can match a refusal to the
    // request the far side is still waiting on.
    expect(document.body.textContent).toContain("corr-1");
  });

  it("tells an operator with no peers what to do next", async () => {
    mock({ inbound: [], outbound: [] });
    render(<PeersPanel />);

    expect(await screen.findByText("No peer is paired into this installation yet.")).toBeTruthy();
    expect(screen.getByText("This installation has not accepted any peer invitation yet.")).toBeTruthy();
  });

  it("reports a failed load instead of rendering an empty screen", async () => {
    invoke.mockImplementation((command: string) =>
      command === "peers_list"
        ? Promise.reject("the background runner is not installed")
        : Promise.resolve({ threads: [], recipe: "peer-task" }),
    );
    render(<PeersPanel />);

    expect(await screen.findByRole("alert")).toBeTruthy();
    expect(document.body.textContent).toContain("the background runner is not installed");
  });
});
