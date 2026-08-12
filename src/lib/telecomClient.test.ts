import { describe, expect, it } from "vitest";
import {
  CARRIER_GUIDES,
  callbackPath,
  canAnswerCalls,
  setupGaps,
  type TelecomAccount,
} from "./telecomClient";

function account(overrides: Partial<TelecomAccount> = {}): TelecomAccount {
  return {
    account_id: "tel-1",
    kind: "twilio",
    kind_label: "Twilio",
    label: "Support line",
    enabled: true,
    carrier_account_id: "AC123",
    from_number: "+15550000000",
    has_credential: true,
    public_base_url: "https://calls.example.test",
    inbound_policy: "answer",
    outbound_approval: "approval",
    limits: {
      max_concurrent_calls: 1,
      ring_timeout_s: 60,
      max_duration_s: 1800,
      recording_enabled: false,
    },
    health: { state: "connected", detail: null, last_error: null, probed_at_ms: 1 },
    updated_at_ms: 1,
    ...overrides,
  };
}

describe("telephony setup guidance", () => {
  it("tells the operator where every carrier credential comes from", () => {
    for (const guide of CARRIER_GUIDES) {
      expect(guide.credentialLabel.length).toBeGreaterThan(0);
      expect(guide.accountIdLabel.length).toBeGreaterThan(0);
      expect(guide.whereToGetIt.length).toBeGreaterThan(0);
      expect(guide.docsUrl.startsWith("https://")).toBe(true);
    }
  });

  it("asks Telnyx for the public key its callbacks are verified with", () => {
    const telnyx = CARRIER_GUIDES.find((guide) => guide.kind === "telnyx");
    expect(telnyx?.configKeys).toContain("webhook_public_key");
  });

  it("offers no test carrier in the app", () => {
    // The mock exists so CI never places a real call. Offering it in setup
    // would let an operator configure a number that silently does nothing.
    expect(CARRIER_GUIDES.map((guide) => guide.kind)).not.toContain("mock");
  });

  it("points a carrier at the account's own callback path", () => {
    expect(callbackPath("tel-abc")).toBe("/v1/telecom/tel-abc");
  });
});

describe("what an account still needs", () => {
  it("is ready only when every half of answering a call exists", () => {
    expect(canAnswerCalls(account())).toBe(true);
    expect(setupGaps(account())).toEqual([]);
  });

  it("will not claim it can answer without somewhere for the carrier to post", () => {
    const withoutUrl = account({ public_base_url: null });
    expect(canAnswerCalls(withoutUrl)).toBe(false);
    expect(setupGaps(withoutUrl)).toContain("public_url");
  });

  it("will not claim it can answer when the policy is to reject", () => {
    expect(canAnswerCalls(account({ inbound_policy: "reject" }))).toBe(false);
  });

  it("counts a saved credential that has never been probed as unfinished", () => {
    const unprobed = account({
      health: { state: "unconfigured", detail: null, last_error: null, probed_at_ms: 0 },
    });
    expect(setupGaps(unprobed)).toContain("probe");
  });

  it("lists what is missing in the order it should be fixed", () => {
    const fresh = account({
      has_credential: false,
      public_base_url: null,
      enabled: false,
      health: { state: "unconfigured", detail: null, last_error: null, probed_at_ms: 0 },
    });
    expect(setupGaps(fresh)).toEqual(["credential", "public_url", "enabled", "probe"]);
  });
});
