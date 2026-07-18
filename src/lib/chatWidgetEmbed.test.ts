import { describe, expect, it } from "vitest";
import { buildWidgetEmbedSnippet, resolveExpiryPreset } from "./chatWidgetEmbed";

describe("buildWidgetEmbedSnippet", () => {
  it("emits the config script and the chat-widget.js include with baseUrl and token", () => {
    const snippet = buildWidgetEmbedSnippet({
      baseUrl: "http://127.0.0.1:1234/v1",
      token: "lmk-abcdef0123456789abcdef0123456789",
    });

    expect(snippet).toBe(
      [
        "<script>",
        "  window.LMK_CHAT_WIDGET_CONFIG = {",
        '  baseUrl: "http://127.0.0.1:1234/v1",',
        '  token: "lmk-abcdef0123456789abcdef0123456789",',
        "  };",
        "</script>",
        '<script src="./chat-widget.js"></script>',
      ].join("\n"),
    );
  });

  it("includes model/title/systemPrompt only when provided, in that order", () => {
    const snippet = buildWidgetEmbedSnippet({
      baseUrl: "http://127.0.0.1:1234/v1",
      token: "lmk-x",
      model: "qwen2.5-7b-instruct",
      title: "Ask the homelab",
      systemPrompt: "Be concise.",
    });

    const lines = snippet.split("\n");
    expect(lines).toContain('  model: "qwen2.5-7b-instruct",');
    expect(lines).toContain('  title: "Ask the homelab",');
    expect(lines).toContain('  systemPrompt: "Be concise.",');
    expect(lines.indexOf('  model: "qwen2.5-7b-instruct",')).toBeLessThan(
      lines.indexOf('  title: "Ask the homelab",'),
    );
    expect(lines.indexOf('  title: "Ask the homelab",')).toBeLessThan(
      lines.indexOf('  systemPrompt: "Be concise.",'),
    );
  });

  it("omits the token line entirely when no token is supplied", () => {
    const snippet = buildWidgetEmbedSnippet({ baseUrl: "http://127.0.0.1:1234/v1", token: "" });
    expect(snippet).not.toContain("token:");
  });

  it("escapes a token/title containing quotes or special characters safely via JSON.stringify", () => {
    const snippet = buildWidgetEmbedSnippet({
      baseUrl: "http://127.0.0.1:1234/v1",
      token: 'lmk-"><script>alert(1)</script>',
      title: 'A "quoted" title',
    });
    // JSON.stringify escapes embedded quotes — the config block stays valid JS
    // and a token/title cannot break out of its string literal.
    expect(snippet).toContain('token: "lmk-\\"><script>alert(1)</script>"');
    expect(snippet).toContain('title: "A \\"quoted\\" title"');
  });
});

describe("resolveExpiryPreset", () => {
  const now = 1_700_000_000_000;

  it("resolves 'never' to null", () => {
    expect(resolveExpiryPreset("never", now)).toBeNull();
  });

  it("resolves each preset to now plus the expected duration", () => {
    expect(resolveExpiryPreset("1h", now)).toBe(now + 60 * 60 * 1000);
    expect(resolveExpiryPreset("1d", now)).toBe(now + 24 * 60 * 60 * 1000);
    expect(resolveExpiryPreset("7d", now)).toBe(now + 7 * 24 * 60 * 60 * 1000);
    expect(resolveExpiryPreset("30d", now)).toBe(now + 30 * 24 * 60 * 60 * 1000);
    expect(resolveExpiryPreset("90d", now)).toBe(now + 90 * 24 * 60 * 60 * 1000);
  });

  it("defaults `now` to the current time when omitted", () => {
    const before = Date.now();
    const resolved = resolveExpiryPreset("1d");
    const after = Date.now();
    expect(resolved).not.toBeNull();
    expect(resolved as number).toBeGreaterThanOrEqual(before + 24 * 60 * 60 * 1000);
    expect(resolved as number).toBeLessThanOrEqual(after + 24 * 60 * 60 * 1000);
  });
});
