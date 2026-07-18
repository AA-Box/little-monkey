/** Builds the embed snippet shown/copied by `ApiServerPanel`'s Widget
 * section — pure and side-effect-free so it's directly unit-testable
 * without rendering the panel (this repo has no React component-test
 * convention; see `sdk/widget/chat-widget.js`'s doc comment for what the
 * referenced script actually does).
 *
 * Deliberately never receives a `TokenEntry` — only ever the plaintext the
 * caller already has in hand (freshly minted, or manually pasted by the
 * user) — because Little Monkey never persists or re-reveals a token's
 * plaintext after creation (see `server.rs`'s `TokenEntry::sha256` doc
 * comment). There is no way to "look up" a token's plaintext to embed here,
 * by design. */
export interface WidgetEmbedOptions {
  /** e.g. "http://127.0.0.1:1234/v1" — the same value the Connection
   * section's "Base URL" field shows. */
  baseUrl: string;
  /** The plaintext token to embed. Never derived from a `TokenEntry` — see
   * the module doc comment above. */
  token: string;
  model?: string;
  title?: string;
  systemPrompt?: string;
}

function jsString(value: string): string {
  return JSON.stringify(value);
}

/** Returns the two-`<script>`-tag snippet documented in
 * `sdk/widget/README.md` — a config block plus a `<script src="./chat-widget.js">`
 * the user copies alongside the actual widget file from `sdk/widget/`. */
export function buildWidgetEmbedSnippet(options: WidgetEmbedOptions): string {
  const configLines: string[] = [`  baseUrl: ${jsString(options.baseUrl)},`];
  if (options.token) {
    configLines.push(`  token: ${jsString(options.token)},`);
  }
  if (options.model) {
    configLines.push(`  model: ${jsString(options.model)},`);
  }
  if (options.title) {
    configLines.push(`  title: ${jsString(options.title)},`);
  }
  if (options.systemPrompt) {
    configLines.push(`  systemPrompt: ${jsString(options.systemPrompt)},`);
  }

  return [
    "<script>",
    "  window.LMK_CHAT_WIDGET_CONFIG = {",
    ...configLines,
    "  };",
    "</script>",
    '<script src="./chat-widget.js"></script>',
  ].join("\n");
}

/** Millisecond expiry presets offered by the create-token form's "Expires"
 * select. `"never"` maps to `null` (`TokenEntry::expires_at`'s "never
 * expires" value). */
export type TokenExpiryPreset = "never" | "1h" | "1d" | "7d" | "30d" | "90d";

const EXPIRY_PRESET_MS: Record<Exclude<TokenExpiryPreset, "never">, number> = {
  "1h": 60 * 60 * 1000,
  "1d": 24 * 60 * 60 * 1000,
  "7d": 7 * 24 * 60 * 60 * 1000,
  "30d": 30 * 24 * 60 * 60 * 1000,
  "90d": 90 * 24 * 60 * 60 * 1000,
};

/** Resolves a preset to an absolute epoch-millisecond `expiresAt`, or `null`
 * for `"never"` — `now` is injectable so this stays deterministic in tests. */
export function resolveExpiryPreset(preset: TokenExpiryPreset, now: number = Date.now()): number | null {
  if (preset === "never") return null;
  return now + EXPIRY_PRESET_MS[preset];
}
