// @vitest-environment jsdom
/**
 * The Peers panel, driven the way an operator drives it.
 *
 * The load-bearing claims here are about authority, not layout. A pairing must
 * never read as trust: the panel says what a peer may *do*, separately from the
 * fact that it is cryptographically paired, and it says so out loud when the
 * same credential is also a controller device. Granting and revoking have to
 * reach the typed bridge with exactly what the operator chose — nothing here
 * may quietly widen a grant — and revoking has to be deliberate rather than one
 * stray click.
 */
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";

const invoke = vi.fn();
vi.mock("@tauri-apps/api/core", () => ({ invoke: (...args: unknown[]) => invoke(...args) }));

import { PeersPanel } from "./PeersPanel";
import type { InboundPeer, OutboundPeer, PeerThread } from "../../lib/peersClient";

const PEER: InboundPeer = {
  device_id: "device-1",
  label: "Studio desktop",
  grants: ["message"],
  state: "active",
  peer_only: true,
  last_sequence: 4,
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
    if (command === "peers_revoke") return Promise.resolve(undefined);
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

beforeEach(() => invoke.mockReset());
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

    const inboundSection = screen.getAllByText("Request work")[1] ?? screen.getByText("Request work");
    const checkbox = inboundSection.closest("label")?.querySelector("input");
    expect(checkbox).toBeTruthy();
    fireEvent.click(checkbox as HTMLInputElement);

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

  it("shows an outbound peer's whole fingerprint, in readable groups", async () => {
    mock({
      inbound: [],
      outbound: [
        {
          alias: "studio",
          peer_id: "runner-two",
          peer_url: "https://studio.invalid",
          grants: ["message", "task"],
          certificate_sha256: "ab".repeat(32),
        },
      ],
    });
    render(<PeersPanel />);

    const fingerprint = await screen.findByText(/abababab/);
    // Whole digest, grouped — a fingerprint you cannot compare completely is
    // decoration.
    expect(fingerprint.textContent?.replace(/\s/g, "")).toBe("ab".repeat(32));
    expect(document.body.textContent).toContain("They allow this installation to:");
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
