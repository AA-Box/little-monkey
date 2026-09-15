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
import { useConnectorsStore } from "../../store/connectorsStore";

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

  it("hides the client-secret field for the providers connected as public clients", () => {
    // Airtable is here because its confidential flow wants the secret in an
    // HTTP Basic header, which this app never sends — offering the field
    // would take a secret it would then silently drop.
    const never = OAUTH_PROVIDERS.filter((p) => p.secret === "never");
    expect(never.map((p) => p.provider)).toEqual(["microsoft_graph", "airtable"]);
    for (const provider of never) {
      card(provider.provider);
      expect(document.querySelector('input[type="password"]')).toBeNull();
    }
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

  it("keeps Connect disabled until a provider with no default host has one", () => {
    // Zendesk's row is `ApiHost { default: None }` in connector_oauth.rs, so a
    // blank host is refused by the backend. Catching it in the form keeps the
    // failure out of the red status pill.
    const required = OAUTH_PROVIDERS.filter((p) => p.hostRequired).map((p) => p.provider);
    expect(required).toEqual(["zendesk"]);
    card("zendesk");
    const connect = screen
      .getAllByRole("button")
      .find((button) => button.textContent?.includes("Connect")) as HTMLButtonElement;
    const setter = Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, "value")?.set;
    const type = (input: HTMLInputElement, value: string) => {
      setter?.call(input, value);
      input.dispatchEvent(new Event("input", { bubbles: true }));
    };
    const inputs = document.querySelectorAll('input[type="text"]');
    type(inputs[0] as HTMLInputElement, "Support");
    expect(connect.disabled).toBe(true);
    type(inputs[1] as HTMLInputElement, "acme.zendesk.com");
    expect(connect.disabled).toBe(false);
  });

  it("labels every field for a screen reader, not only with a placeholder", () => {
    card("gitlab");
    for (const input of document.querySelectorAll("input")) {
      expect(input.getAttribute("aria-label"), input.getAttribute("placeholder") ?? "").toBeTruthy();
    }
  });

  it("treats needs_client_id as a terminal phase, so Connect becomes usable again", () => {
    expect(CONNECTOR_OAUTH_DONE).toContain("needs_client_id");
    expect(CONNECTOR_OAUTH_DONE).not.toContain("waiting_for_browser");
  });

  it("does not show an earlier attempt's status on a freshly opened card", () => {
    // `oauthStatus` is keyed by provider and outlives the form. Reopening the
    // card to add a *second* Linear account must not greet the user with the
    // first one's green "Connected" pill or its red error line.
    useConnectorsStore.setState({
      oauthStatus: { linear: { phase: "error", error: "consent was denied" } },
    });
    card("linear");
    expect(document.body.textContent).not.toContain("consent was denied");
    useConnectorsStore.setState({ oauthStatus: {} });
  });
});
