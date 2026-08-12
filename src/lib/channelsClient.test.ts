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

  it("has no duplicate providers", () => {
    const kinds = PROVIDER_GUIDES.map((guide) => guide.kind);
    expect(new Set(kinds).size).toBe(kinds.length);
  });
});
