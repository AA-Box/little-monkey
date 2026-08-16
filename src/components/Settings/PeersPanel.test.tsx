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
import type {
  InboundPeer,
  OutboundPeer,
  PeerOutboundMessage,
  PeerThread,
} from "../../lib/peersClient";

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

const SENT: PeerOutboundMessage = {
  alias: "studio",
  message_id: "pmsg-1",
  thread_id: "thread-out",
  correlation_id: "corr-123",
  kind: "task_request",
  state: "queued",
  result_text: null,
  sent_at_ms: 1_700_000_000_000,
  checked_at_ms: null,
};

function mock(options: {
  inbound?: InboundPeer[];
  outbound?: OutboundPeer[];
  threads?: PeerThread[];
  sent?: PeerOutboundMessage[];
  /** A command that must reject, to prove a failed mutation reaches the operator. */
  failing?: string;
}) {
  invoke.mockImplementation((command: string) => {
    if (command === options.failing) {
      return Promise.reject("the peer refused the request");
    }
    if (command === "peers_outbound") {
      return Promise.resolve({ messages: options.sent ?? [] });
    }
    if (command === "peers_remote_thread") {
      return Promise.resolve({
        messages: [
          { ...SENT, state: "succeeded", result_text: "the build is red", checked_at_ms: 1 },
        ],
      });
    }
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

  it("writes an invitation with exactly the grants the operator ticked", async () => {
    mock({ inbound: [], outbound: [] });
    saveDialog.mockResolvedValue("/tmp/invite.json");
    render(<PeersPanel />);
    await screen.findByText("No peer is paired into this installation yet.");

    fireEvent.change(screen.getByLabelText("Peer name"), { target: { value: "Studio desktop" } });
    // "Send messages" is on by default; add task, and leave artifact alone.
    fireEvent.click(grantCheckbox(0, "Request work"));
    fireEvent.click(screen.getByText("Write invitation"));

    await waitFor(() => {
      const call = invoke.mock.calls.find(([command]) => command === "peers_invite");
      expect(call).toBeTruthy();
      const args = call?.[1] as {
        label: string;
        allow: string[];
        expiresMinutes: number;
        output: string;
      };
      expect(args.label).toBe("Studio desktop");
      // Exactly what was ticked — an invitation that quietly widened a grant
      // would be the pairing an operator never agreed to.
      expect(args.allow).toEqual(["message", "task"]);
      expect(args.allow).not.toContain("artifact");
      expect(args.expiresMinutes).toBe(60);
      expect(args.output).toBe("/tmp/invite.json");
    });
    expect((await screen.findByRole("status")).textContent).toContain("/tmp/invite.json");
  });

  it("creates nothing when the operator cancels the save dialog", async () => {
    mock({ inbound: [], outbound: [] });
    saveDialog.mockResolvedValue(null);
    render(<PeersPanel />);
    await screen.findByText("No peer is paired into this installation yet.");

    fireEvent.change(screen.getByLabelText("Peer name"), { target: { value: "Studio desktop" } });
    fireEvent.click(screen.getByText("Write invitation"));

    await waitFor(() => expect(saveDialog).toHaveBeenCalled());
    // The invitation is a credential; not writing the file must mean not
    // minting one, or the pairing exists with nobody holding it.
    expect(invoke.mock.calls.some(([command]) => command === "peers_invite")).toBe(false);
  });

  it("accepts an invitation from the file the operator chose and shows the fingerprint", async () => {
    mock({ inbound: [], outbound: [] });
    invoke.mockImplementation((command: string) => {
      if (command === "peers_list") return Promise.resolve({ inbound: [], outbound: [] });
      if (command === "peers_threads") return Promise.resolve({ threads: [], recipe: "peer-task" });
      if (command === "peers_outbound") return Promise.resolve({ messages: [] });
      if (command === "peers_accept") {
        return Promise.resolve({
          alias: "studio",
          peer_id: "runner-two",
          peer_url: "https://studio.invalid",
          grants: ["message"],
          certificate_sha256: "ab".repeat(32),
        });
      }
      return Promise.resolve({});
    });
    openDialog.mockResolvedValue("/tmp/invite.json");
    render(<PeersPanel />);
    await screen.findByText("No peer is paired into this installation yet.");

    fireEvent.change(screen.getByLabelText("Local name"), { target: { value: "studio" } });
    fireEvent.click(screen.getByText("Choose file…"));
    await waitFor(() => expect(openDialog).toHaveBeenCalled());
    fireEvent.click(screen.getByText("Accept invitation"));

    await waitFor(() => {
      expect(
        invoke.mock.calls.some(
          ([command, args]) =>
            command === "peers_accept" &&
            (args as { invitation: string; alias: string }).invitation === "/tmp/invite.json" &&
            (args as { alias: string }).alias === "studio",
        ),
      ).toBe(true);
    });
    // The fingerprint has to be readable, because comparing it out of band is
    // the only thing that proves who was paired with.
    const notice = await screen.findByRole("status");
    expect(notice.textContent?.replace(/\s/g, "")).toContain("ab".repeat(32));
    // Refreshed from the backend rather than guessed at locally.
    expect(invoke.mock.calls.filter(([command]) => command === "peers_list").length).toBe(2);
  });

  it("does nothing when the operator cancels the invitation file chooser", async () => {
    mock({ inbound: [], outbound: [] });
    openDialog.mockResolvedValue(null);
    render(<PeersPanel />);
    await screen.findByText("No peer is paired into this installation yet.");

    fireEvent.click(screen.getByText("Choose file…"));
    await waitFor(() => expect(openDialog).toHaveBeenCalled());
    expect(invoke.mock.calls.some(([command]) => command === "peers_accept")).toBe(false);
  });

  it("lets an operator take every grant away without severing the pairing", async () => {
    mock({});
    render(<PeersPanel />);
    await screen.findByText("Studio desktop");

    // The peer holds only "message"; unticking it leaves an empty list.
    fireEvent.click(grantCheckbox(1, "Send messages"));

    await waitFor(() => {
      expect(
        invoke.mock.calls.some(
          ([command, args]) =>
            command === "peers_grant" &&
            JSON.stringify((args as { allow: string[] }).allow) === "[]",
        ),
      ).toBe(true);
    });
    expect(invoke.mock.calls.some(([command]) => command === "peers_revoke")).toBe(false);
  });

  it("takes up a rotated key only from a bundle the operator chose", async () => {
    mock({ inbound: [], outbound: [OUTBOUND] });
    openDialog.mockResolvedValue(null);
    render(<PeersPanel />);
    await screen.findByText("studio");

    fireEvent.click(screen.getByText("Accept new key"));
    await waitFor(() => expect(openDialog).toHaveBeenCalled());
    expect(invoke.mock.calls.some(([command]) => command === "peers_accept_rotation")).toBe(false);

    openDialog.mockResolvedValue("/tmp/rotation.json");
    fireEvent.click(screen.getByText("Accept new key"));
    await waitFor(() => {
      expect(
        invoke.mock.calls.some(
          ([command, args]) =>
            command === "peers_accept_rotation" &&
            (args as { bundle: string; alias: string }).bundle === "/tmp/rotation.json" &&
            (args as { alias: string }).alias === "studio",
        ),
      ).toBe(true);
    });
    expect((await screen.findByRole("status")).textContent).toContain("key generation 3");
  });

  it("shows what this installation sent to a peer, and asks for the result on demand", async () => {
    mock({ inbound: [], outbound: [OUTBOUND], sent: [SENT] });
    render(<PeersPanel />);
    await screen.findByText("studio");

    fireEvent.click(screen.getByText("What you sent (1)"));
    expect(await screen.findByText("thread-out")).toBeTruthy();
    // A task nobody has asked about yet reads as waiting, not as finished.
    expect(document.body.textContent).toContain("Waiting on the peer");
    expect(document.body.textContent).toContain("corr-123");

    fireEvent.click(screen.getByText("Ask for the result"));
    await waitFor(() => {
      expect(
        invoke.mock.calls.some(
          ([command, args]) =>
            command === "peers_remote_thread" &&
            (args as { alias: string; threadId: string }).alias === "studio" &&
            (args as { threadId: string }).threadId === "thread-out",
        ),
      ).toBe(true);
    });
    // No URL, no host, no route: an alias and a thread id this side minted.
    const call = invoke.mock.calls.find(([command]) => command === "peers_remote_thread");
    expect(Object.keys(call?.[1] as object).sort()).toEqual(["alias", "threadId"]);
  });

  it("says so when the backend refuses a mutation instead of looking like it worked", async () => {
    mock({ failing: "peers_grant" });
    render(<PeersPanel />);
    await screen.findByText("Studio desktop");

    fireEvent.click(grantCheckbox(1, "Request work"));

    expect((await screen.findByRole("alert")).textContent).toContain("the peer refused the request");
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
