/** Builds the embed snippet shown/copied by `ApiServerPanel`'s Widget
 * section — pure and side-effect-free so it's directly unit-testable
 * without rendering the panel (this repo has no React component-test
 * convention; see `sdk/widget/chat-widget.js`'s doc comment for what the
 * referenced script actually does).
 *
 * Deliberately never receives a stored token record — only the scoped
 * plaintext returned once by Runtime Hub pairing and manually pasted by
 * the user. Little Monkey never persists or re-reveals that plaintext, so
 * there is no token lookup path here by design. */
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
