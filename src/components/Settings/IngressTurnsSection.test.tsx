// @vitest-environment jsdom
/**
 * The one listing that spans every conversational origin.
 *
 * Two properties are worth a test rather than a look. The panel must be able
 * to show all six origins the durable contract defines — a turn that arrives
 * from a surface the UI cannot label is a turn an operator cannot act on — and
 * it must never show what was said, because the whole point of the backend
 * listing carrying no message text is undone if the panel goes looking for it.
 */
import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, render, screen, waitFor } from "@testing-library/react";

const invoke = vi.fn();
vi.mock("@tauri-apps/api/core", () => ({ invoke: (...args: unknown[]) => invoke(...args) }));

import { IngressTurnsSection } from "./IngressTurnsSection";
import type { ConversationSource, IngressTurn } from "../../lib/ingressClient";

function turn(source: ConversationSource, overrides: Partial<IngressTurn> = {}): IngressTurn {
  return {
    ingress_id: `ingr-${source}`,
    source,
    source_account_id: `${source}-acct`,
    account_label: null,
    source_event_id: `${source}-event`,
    session_key: `${source}:session`,
    state: "queued",
    attempts: 1,
    last_error: null,
    execution_version: 1,
    execution_digest: "abcdef0123456789".repeat(4),
    mutation_required: false,
    mutation_state: null,
    mutation_detail: null,
    parent_ingress_id: null,
    continuation_kind: null,
    continuation_attempt: 0,
    job_id: `ingress-${source}`,
    run_id: `run-${source}`,
    run_state: "running",
    run_error: null,
    created_at_ms: 1_700_000_000_000,
    updated_at_ms: 1_700_000_000_000,
    ...overrides,
  };
}

const ALL: ConversationSource[] = [
  "desktop",
  "mobile",
  "messaging_channel",
  "peer",
  "voice",
  "telephone",
];

afterEach(() => {
  cleanup();
  invoke.mockReset();
});

describe("the conversation turn listing", () => {
  it("labels every origin the durable contract defines", async () => {
    invoke.mockResolvedValue({ turns: ALL.map((source) => turn(source)) });
    render(<IngressTurnsSection />);

    await waitFor(() => expect(screen.getAllByText("Desktop").length).toBeGreaterThan(0));
    for (const label of [
      "Desktop",
      "Mobile device",
      "Messaging channel",
      "Peer node",
      "Voice",
      "Phone",
    ]) {
      // Twice: once as the row's origin, once as an option in the filter, so
      // an operator can also narrow the list to it.
      expect(screen.getAllByText(label)).toHaveLength(2);
    }
  });

  it("shows what an operator needs to diagnose a turn, including the frozen config", async () => {
    invoke.mockResolvedValue({ turns: [turn("peer", { attempts: 3 })] });
    render(<IngressTurnsSection />);

    await waitFor(() => expect(screen.getByText("peer-event")).toBeTruthy());
    expect(screen.getByText("ingress-peer")).toBeTruthy();
    expect(screen.getByText("run-peer")).toBeTruthy();
    expect(screen.getByText("3")).toBeTruthy();
    expect(screen.getByText("v1 abcdef012345")).toBeTruthy();
  });

  it("reports a parked turn's reason and prefers the run's own error", async () => {
    invoke.mockResolvedValue({
      turns: [
        turn("telephone", {
          state: "failed",
          last_error: "the queue is unavailable",
          run_state: "failed",
          run_error: "the model credential was deleted",
        }),
      ],
    });
    render(<IngressTurnsSection />);

    await waitFor(() =>
      expect(screen.getByText("the model credential was deleted")).toBeTruthy(),
    );
    expect(screen.getByText("Failed")).toBeTruthy();
  });

  it("says so plainly when the daemon cannot be read, rather than showing an empty list", async () => {
    invoke.mockRejectedValue(new Error("The background runner is not installed"));
    render(<IngressTurnsSection />);

    await waitFor(() => expect(screen.getByRole("alert")).toBeTruthy());
    expect(screen.getByRole("alert").textContent).toContain("not installed");
  });

  it("has nowhere to render message text, because the listing carries none", async () => {
    invoke.mockResolvedValue({ turns: [turn("messaging_channel")] });
    const { container } = render(<IngressTurnsSection />);

    await waitFor(() => expect(screen.getByText("Messaging channel")).toBeTruthy());
    const asked = invoke.mock.calls[0];
    expect(asked[0]).toBe("ingress_turns");
    expect(container.textContent).not.toContain("messaging_channel:session");
  });
});
