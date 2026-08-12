import { describe, expect, it } from "vitest";
import {
  PROVIDER_GUIDES,
  buildProviderConfig,
  callbackPath,
  missingRequiredConfig,
  needsPublicCallback,
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
    // An unknown provider must not claim a webhook requirement it cannot have.
    expect(needsPublicCallback("nonsense")).toBe(false);
  });

  it("points a webhook provider at the account's own path", () => {
    expect(callbackPath("chan-abc")).toBe("/v1/channels/chan-abc");
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
        expect(field.key).not.toMatch(/secret|token|password|key/);
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

  it("has no duplicate providers", () => {
    const kinds = PROVIDER_GUIDES.map((guide) => guide.kind);
    expect(new Set(kinds).size).toBe(kinds.length);
  });
});
