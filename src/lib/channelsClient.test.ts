import { describe, expect, it, vi } from "vitest";

const invoke = vi.fn();
vi.mock("@tauri-apps/api/core", () => ({ invoke: (...args: unknown[]) => invoke(...args) }));

import {
  PROVIDER_GUIDES,
  buildProviderConfig,
  channelsAddRoute,
  channelsCallbackUrl,
  channelsSetConfig,
  channelsUpdateRoute,
  configFormValues,
  mergeProviderConfig,
  missingRequiredConfig,
  needsPublicCallback,
  routeSpecificity,
} from "./channelsClient";

describe("channels setup guidance", () => {
  it("only asks for a public callback URL where the provider genuinely calls us", () => {
    expect(needsPublicCallback("telegram")).toBe(false);
    expect(needsPublicCallback("discord")).toBe(false);
    expect(needsPublicCallback("irc")).toBe(false);
    expect(needsPublicCallback("whatsapp")).toBe(true);
    expect(needsPublicCallback("line")).toBe(true);
    expect(needsPublicCallback("teams")).toBe(true);
    expect(needsPublicCallback("google_chat")).toBe(true);
    // SMS is webhook-delivered too, but to the telephony listener: showing
    // the channels path for it would be a URL nothing answers.
    expect(needsPublicCallback("sms")).toBe(false);
    // An unknown provider must not claim a webhook requirement it cannot have.
    expect(needsPublicCallback("nonsense")).toBe(false);
  });

  it("asks the daemon for a callback URL rather than composing one", () => {
    // The frontend cannot know what the daemon is reachable as: a guessed
    // host is how an operator ends up pasting a URL that nothing answers.
    invoke.mockResolvedValueOnce({
      account_id: "chan-abc",
      configured: true,
      url: "https://hooks.example.com/v1/channels/chan-abc",
      path: "/v1/channels/chan-abc",
    });
    void channelsCallbackUrl("chan-abc");
    expect(invoke).toHaveBeenCalledWith("channels_callback_url", { accountId: "chan-abc" });
  });

  it("tells the operator where every credential comes from", () => {
    for (const guide of PROVIDER_GUIDES) {
      expect(guide.credentialLabel.length).toBeGreaterThan(0);
      expect(guide.whereToGetIt.length).toBeGreaterThan(0);
      expect(guide.docsUrl.startsWith("https://")).toBe(true);
    }
  });

  it("never asks for a secret in the non-secret settings field", () => {
    // configKeys land in the account row; secrets belong in the keychain, and
    // the two are collected by different inputs. A secret listed here would be
    // stored in the clear and the adapter would then reject the credential it
    // never received.
    for (const guide of PROVIDER_GUIDES) {
      for (const field of guide.configFields) {
        // A *public* key is the one kind of key that belongs here: it
        // verifies inbound signatures and unlocks nothing.
        expect(field.key).not.toMatch(/secret|token|password|(?<!public_)key/);
      }
    }
  });

  it("gives every non-secret setting a label the operator can act on", () => {
    // The panel renders one input per field, so a field with no label is an
    // unlabelled box, and a required one with no label is an unlabelled box
    // that blocks the form.
    for (const guide of PROVIDER_GUIDES) {
      for (const field of guide.configFields) {
        expect(field.label.length).toBeGreaterThan(0);
      }
    }
  });

  it("asks the helper providers for the helper, not for a credential", () => {
    // Signal and iMessage have no token of ours: signal-cli and the macOS
    // helper own the account. Setup must collect the path instead, and must
    // not imply a secret is missing.
    for (const kind of ["signal", "imessage"]) {
      const guide = PROVIDER_GUIDES.find((entry) => entry.kind === kind);
      expect(guide?.credentialOptional).toBe(true);
      expect(guide?.transport).toBe("helper");
      expect(guide?.configFields.map((field) => field.key)).toContain("helper_path");
    }
    expect(PROVIDER_GUIDES.find((entry) => entry.kind === "imessage")?.requiresPlatform).toBe("macos");
  });

  it("does not make an iMessage helper mandatory", () => {
    // iMessage talks to Messages on this Mac by default. Requiring a helper
    // path would block setup on installing something that most people have
    // no reason to run.
    const fields = PROVIDER_GUIDES.find((guide) => guide.kind === "imessage")!.configFields;
    expect(fields.find((field) => field.key === "helper_path")?.required).toBeFalsy();
    expect(fields.find((field) => field.key === "handle")?.required).toBe(true);
    // Signal is the opposite: signal-cli is the only way in, so its path is
    // required.
    const signal = PROVIDER_GUIDES.find((guide) => guide.kind === "signal")!.configFields;
    expect(signal.find((field) => field.key === "helper_path")?.required).toBe(true);
  });

  it("collects each server's own address rather than a hosted provider's", () => {
    const keys = (kind: string) =>
      (PROVIDER_GUIDES.find((guide) => guide.kind === kind)?.configFields ?? []).map((field) => field.key);
    expect(keys("matrix")).toEqual(["homeserver_url", "user_id"]);
    expect(keys("mattermost")).toEqual(["base_url"]);
    expect(keys("irc")).toEqual(["server", "port", "nick", "channels", "use_sasl"]);
  });
});

describe("provider settings", () => {
  const irc = PROVIDER_GUIDES.find((guide) => guide.kind === "irc")!.configFields;

  it("types each value the way the daemon parses it", () => {
    expect(
      buildProviderConfig(irc, {
        server: "irc.libera.chat",
        port: "6697",
        nick: "monkey",
        channels: "#one, #two",
        use_sasl: "true",
      }),
    ).toEqual({
      server: "irc.libera.chat",
      port: 6697,
      nick: "monkey",
      channels: ["#one", "#two"],
      use_sasl: true,
    });
  });

  it("leaves a blank setting out entirely rather than storing an empty one", () => {
    // An absent key produces the adapter's own "is missing X" message; a key
    // holding "" produces a stranger failure further in.
    expect(buildProviderConfig(irc, { server: "irc.example.org", nick: "monkey", port: "  " })).toEqual({
      server: "irc.example.org",
      nick: "monkey",
    });
  });

  it("drops a port that is not a number instead of sending it on", () => {
    expect(buildProviderConfig(irc, { port: "not-a-port" })).toEqual({});
  });

  it("names the required settings that are still blank", () => {
    expect(missingRequiredConfig(irc, { server: "irc.example.org" })).toEqual(["Nickname"]);
    expect(missingRequiredConfig(irc, { server: "irc.example.org", nick: "monkey" })).toEqual([]);
  });

  it("collects every part of a multi-value credential", () => {
    // The four webhook providers each need more than one value, and the
    // adapters parse them as one JSON bundle — so setup has to ask for each
    // part by name rather than hoping the operator types the JSON.
    const parts = (kind: string) =>
      (PROVIDER_GUIDES.find((guide) => guide.kind === kind)?.secretFields ?? []).map((field) => field.key);
    expect(parts("whatsapp")).toEqual(["access_token", "app_secret", "verify_token"]);
    expect(parts("line")).toEqual(["channel_access_token", "channel_secret"]);
    expect(parts("teams")).toEqual(["app_password"]);
    // Google Chat's credential is a key file pasted whole, so there is nothing
    // to split apart.
    expect(parts("google_chat")).toEqual([]);
  });

  it("tells the operator the same Google Chat authentication audience the daemon verifies", () => {
    // The daemon accepts exactly two values here and checks a different `aud`
    // for each. Setup that described one and required the other is how an
    // operator ends up with a callback Google calls and this app refuses.
    const guide = PROVIDER_GUIDES.find((entry) => entry.kind === "google_chat")!;
    const audience = guide.configFields.find((field) => field.key === "auth_audience")!;
    expect(audience.hint).toContain("project_number");
    expect(audience.hint).toContain("app_url");
    expect(guide.whereToGetIt).toContain("Authentication Audience");
  });

  it("offers no Microsoft Teams cloud setting, because only one cloud works", () => {
    // A box for a sovereign-cloud endpoint would imply the rest of the Teams
    // checks follow it. They do not: the issuer, the key document, the token
    // scope and the reply hosts are all the public cloud's.
    const guide = PROVIDER_GUIDES.find((entry) => entry.kind === "teams")!;
    expect(guide.configFields.map((field) => field.key)).toEqual(["app_id", "tenant_id"]);
    expect(guide.whereToGetIt).toContain("public Bot Framework cloud");
  });

  it("has no duplicate providers", () => {
    const kinds = PROVIDER_GUIDES.map((guide) => guide.kind);
    expect(new Set(kinds).size).toBe(kinds.length);
  });
});

describe("editing an existing account's settings", () => {
  const irc = PROVIDER_GUIDES.find((guide) => guide.kind === "irc")!.configFields;

  it("starts the form from what is actually stored", () => {
    expect(
      configFormValues(irc, {
        server: "irc.libera.chat",
        port: 6697,
        channels: ["#one", "#two"],
        use_sasl: true,
      }),
    ).toEqual({
      server: "irc.libera.chat",
      port: "6697",
      nick: "",
      channels: "#one, #two",
      use_sasl: "true",
    });
  });

  it("carries across settings the panel has no input for", () => {
    // `set-config` replaces the object wholesale, and an account configured
    // from the terminal can hold keys no guide describes. Losing them because
    // someone edited an unrelated field would be a silent downgrade.
    const merged = mergeProviderConfig(
      { server: "irc.old.example", nick: "monkey", max_attachment_bytes: 1024 },
      irc,
      { server: "irc.new.example", nick: "monkey" },
    );
    expect(merged).toEqual({
      server: "irc.new.example",
      nick: "monkey",
      max_attachment_bytes: 1024,
    });
  });

  it("clears a setting the operator emptied rather than resurrecting it", () => {
    const merged = mergeProviderConfig({ server: "irc.example.org", nick: "monkey" }, irc, {
      server: "irc.example.org",
      nick: "",
    });
    expect(merged).toEqual({ server: "irc.example.org" });
  });

  it("sends settings and label without ever carrying a credential", () => {
    invoke.mockResolvedValueOnce({});
    void channelsSetConfig("chan-1", '{"server":"irc.example.org"}', "Team IRC");
    expect(invoke).toHaveBeenCalledWith("channels_set_config", {
      accountId: "chan-1",
      config: '{"server":"irc.example.org"}',
      label: "Team IRC",
    });
    const [, args] = invoke.mock.calls[invoke.mock.calls.length - 1];
    expect(Object.keys(args as object)).toEqual(["accountId", "config", "label"]);
  });
});

describe("the routing ladder", () => {
  it("reads a scope's rung the way the daemon does", () => {
    expect(routeSpecificity({})).toBe("global_default");
    expect(routeSpecificity({ kind: "telegram" })).toBe("channel_default");
    expect(routeSpecificity({ account_id: "chan-1" })).toBe("account");
    expect(routeSpecificity({ account_id: "chan-1", conversation_id: "c" })).toBe("conversation");
    expect(routeSpecificity({ account_id: "chan-1", conversation_id: "c", thread_id: "t" })).toBe("thread");
    expect(
      routeSpecificity({ account_id: "chan-1", conversation_id: "c", thread_id: "t", sender_id: "s" }),
    ).toBe("sender");
  });

  it("passes the whole scope to the daemon as one options object", () => {
    invoke.mockResolvedValueOnce({ route: {} });
    void channelsAddRoute("chat", {
      account_id: "chan-1",
      conversation_id: "-100123",
      thread_id: "42",
      sender_id: "user-7",
      session_scope: "conversation",
      priority: 5,
      reply: false,
      enabled: true,
    });
    expect(invoke).toHaveBeenCalledWith("channels_add_route", {
      recipe: "chat",
      options: {
        account_id: "chan-1",
        conversation_id: "-100123",
        thread_id: "42",
        sender_id: "user-7",
        session_scope: "conversation",
        priority: 5,
        reply: false,
        enabled: true,
      },
    });
  });

  it("edits a route in place rather than replacing its identity", () => {
    invoke.mockResolvedValueOnce({ route: {} });
    void channelsUpdateRoute("route-1", "triage", { account_id: "chan-1" });
    expect(invoke).toHaveBeenCalledWith("channels_update_route", {
      routeId: "route-1",
      recipe: "triage",
      options: { account_id: "chan-1" },
    });
  });
});
