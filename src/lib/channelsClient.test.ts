import { describe, expect, it } from "vitest";
import { PROVIDER_GUIDES, callbackPath, needsPublicCallback } from "./channelsClient";

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
      for (const key of guide.configKeys) {
        expect(key).not.toMatch(/secret|token|password|key/);
      }
    }
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
