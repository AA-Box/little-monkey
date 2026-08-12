// @vitest-environment jsdom
/**
 * The Telephony panel, driven the way an operator drives it.
 *
 * What is load-bearing here: the panel must say out loud that calls cost money
 * at the operator's own carrier, it must never report a number as connected
 * because a credential was saved, it must keep answering the phone and dialing
 * out as two separate choices, and a carrier credential must go to the keychain
 * command rather than into any argument the panel builds.
 */
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";

const invoke = vi.fn();
vi.mock("@tauri-apps/api/core", () => ({ invoke: (...args: unknown[]) => invoke(...args) }));
vi.mock("@tauri-apps/plugin-opener", () => ({ openUrl: vi.fn() }));

import { TelephonyPanel } from "./TelephonyPanel";
import type { TelecomAccount } from "../../lib/telecomClient";

const BASE: TelecomAccount = {
  account_id: "tel-1",
  kind: "twilio",
  kind_label: "Twilio",
  label: "Support line",
  enabled: true,
  carrier_account_id: "AC123",
  from_number: "+15550000000",
  has_credential: true,
  public_base_url: "https://calls.example.test",
  greeting: "Support line, how can I help?",
  supports_recording: true,
  inbound_policy: "reject",
  outbound_approval: "never",
  limits: {
    max_concurrent_calls: 1,
    ring_timeout_s: 60,
    max_duration_s: 1800,
    recording_enabled: false,
  },
  health: { state: "disconnected", detail: null, last_error: null, probed_at_ms: 0 },
  updated_at_ms: 0,
};

function mockAccounts(accounts: TelecomAccount[]) {
  invoke.mockImplementation((command: string) => {
    if (command === "telecom_list") return Promise.resolve(accounts);
    if (command === "telecom_calls") return Promise.resolve([]);
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

describe("TelephonyPanel", () => {
  it("warns that the operator's carrier bills for this", async () => {
    render(<TelephonyPanel />);

    expect(await screen.findByText(/billed by your carrier/i)).toBeTruthy();
  });

  it("reports a saved-but-unprobed number as unchecked, not as connected", async () => {
    render(<TelephonyPanel />);

    expect(await screen.findByText("Not checked yet")).toBeTruthy();
    expect(screen.queryByText("Connected")).toBeNull();
  });

  it("keeps answering the phone and calling out as two separate choices", async () => {
    render(<TelephonyPanel />);
    fireEvent.click(await screen.findByText("+15550000000"));

    const inbound = (await screen.findByLabelText(/When this number rings/i)) as HTMLSelectElement;
    fireEvent.change(inbound, { target: { value: "answer" } });

    await waitFor(() => {
      expect(
        invoke.mock.calls.some(
          ([command, args]) =>
            command === "telecom_set_policy" &&
            (args as { inbound: string | null; outbound: string | null }).inbound === "answer" &&
            (args as { inbound: string | null; outbound: string | null }).outbound === null,
        ),
      ).toBe(true);
    });
  });

  it("sends a carrier credential to the keychain command and nowhere else", async () => {
    render(<TelephonyPanel />);
    fireEvent.click(await screen.findByText("+15550000000"));

    fireEvent.change(await screen.findByLabelText(/Auth token/i), {
      target: { value: "super-secret-token" },
    });
    fireEvent.click(screen.getByText("Save credential"));

    await waitFor(() => {
      expect(
        invoke.mock.calls.some(
          ([command, args]) =>
            command === "telecom_set_credential" &&
            (args as { secret: string }).secret === "super-secret-token",
        ),
      ).toBe(true);
    });
    // The secret must not have ridden along on any other command.
    for (const [command, args] of invoke.mock.calls) {
      if (command === "telecom_set_credential") continue;
      expect(JSON.stringify(args ?? {})).not.toContain("super-secret-token");
    }
  });

  it("shows the callback URL a carrier has to be pointed at", async () => {
    render(<TelephonyPanel />);
    fireEvent.click(await screen.findByText("+15550000000"));

    expect(
      await screen.findByText("https://calls.example.test/v1/telecom/tel-1"),
    ).toBeTruthy();
  });

  it("says what is missing when there is nowhere for the carrier to deliver", async () => {
    mockAccounts([{ ...BASE, public_base_url: null }]);
    render(<TelephonyPanel />);
    fireEvent.click(await screen.findByText("+15550000000"));

    expect(await screen.findByText(/nowhere to deliver/i)).toBeTruthy();
  });

  it("warns when a number answers calls without saying anything first", async () => {
    mockAccounts([{ ...BASE, inbound_policy: "answer", greeting: null }]);
    render(<TelephonyPanel />);
    fireEvent.click(await screen.findByText("+15550000000"));

    expect(await screen.findByText(/hears silence/i)).toBeTruthy();
  });

  it("will not offer recording on a carrier that cannot record a streamed call", async () => {
    mockAccounts([{ ...BASE, kind: "plivo", kind_label: "Plivo", supports_recording: false }]);
    render(<TelephonyPanel />);
    fireEvent.click(await screen.findByText("+15550000000"));

    const toggle = (await screen.findByLabelText("Record calls")) as HTMLInputElement;
    expect(toggle.disabled).toBe(true);
    expect(screen.getByText(/cannot record a call it is also streaming/i)).toBeTruthy();
  });

  it("offers no test carrier to configure", async () => {
    render(<TelephonyPanel />);

    const carrier = (await screen.findByLabelText("Carrier")) as HTMLSelectElement;
    const options = Array.from(carrier.options).map((option) => option.value);
    expect(options).toEqual(["twilio", "telnyx", "plivo"]);
  });
});
