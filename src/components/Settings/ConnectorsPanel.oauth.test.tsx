// @vitest-environment jsdom
/**
 * The OAuth connect card's two rules that are easy to break and impossible to
 * see in a type check: which providers get a client-secret field, and that the
 * client ID is optional (blank reuses the registration already in the
 * keychain, which is what makes the second account of a provider one click).
 */
import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, render, screen } from "@testing-library/react";

vi.mock("@tauri-apps/api/core", () => ({
  invoke: async () => "http://127.0.0.1:50000/",
  isTauri: () => false,
}));
vi.mock("@tauri-apps/api/event", () => ({ listen: () => Promise.resolve(() => {}) }));

import { OAUTH_PROVIDERS, CONNECTOR_OAUTH_DONE, OAuthConnectForm } from "./ConnectorsPanel";

afterEach(cleanup);

function card(provider: string) {
  const info = OAUTH_PROVIDERS.find((p) => p.provider === provider);
  if (!info) throw new Error(`no OAuth card for ${provider}`);
  render(<OAuthConnectForm info={info} onDone={() => {}} />);
}

describe("OAuth connect card", () => {
  it("offers the client-secret field to every provider that can take one", () => {
    // `optional` providers register confidential clients by default (GitLab
    // does), so a card without the field simply cannot connect them.
    for (const info of OAUTH_PROVIDERS.filter((p) => p.secret !== "never")) {
      cleanup();
      card(info.provider);
      expect(
        document.querySelector('input[type="password"]'),
        `${info.provider} (${info.secret}) must offer a client secret field`,
      ).not.toBeNull();
    }
  });

  it("hides the client-secret field for a provider that registers public clients only", () => {
    const never = OAUTH_PROVIDERS.filter((p) => p.secret === "never");
    expect(never.map((p) => p.provider)).toEqual(["microsoft_graph"]);
    card("microsoft_graph");
    expect(document.querySelector('input[type="password"]')).toBeNull();
  });

  it("enables Connect with a label alone, so a blank client ID reuses the saved app", () => {
    card("linear");
    const connect = screen
      .getAllByRole("button")
      .find((button) => button.textContent?.includes("Connect"));
    expect(connect).toBeDefined();
    expect((connect as HTMLButtonElement).disabled).toBe(true);

    const label = document.querySelectorAll('input[type="text"]')[0] as HTMLInputElement;
    label.focus();
    // React's controlled input: set the value through the native setter so the
    // synthetic change event carries it.
    const setter = Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, "value")?.set;
    setter?.call(label, "Work");
    label.dispatchEvent(new Event("input", { bubbles: true }));
    expect((connect as HTMLButtonElement).disabled).toBe(false);
  });

  it("treats needs_client_id as a terminal phase, so Connect becomes usable again", () => {
    expect(CONNECTOR_OAUTH_DONE).toContain("needs_client_id");
    expect(CONNECTOR_OAUTH_DONE).not.toContain("waiting_for_browser");
  });
});
