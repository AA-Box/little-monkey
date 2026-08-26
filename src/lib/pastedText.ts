const CHARS_PER_TOKEN_ESTIMATE = 4;

/**
 * Pasting a few paragraphs should stay ordinary textarea input. Once a paste
 * is large enough to make the composer awkward, keep the exact bytes but
 * represent them as an editable local Markdown attachment instead. This is a
 * UI threshold only: no model/tokenizer is called to make the decision.
 */
export const LARGE_PASTE_MIN_CHARS = 8_000;
export const LARGE_PASTE_MIN_LINES = 80;

export interface PastedTextLike {
  path: string;
  label?: string;
  content?: string;
}

export function shouldCollapsePastedText(text: string): boolean {
  if (text.length >= LARGE_PASTE_MIN_CHARS) return true;
  let lines = 1;
  for (let index = 0; index < text.length; index += 1) {
    if (text.charCodeAt(index) === 10) {
      lines += 1;
      if (lines >= LARGE_PASTE_MIN_LINES) return true;
    }
  }
  return false;
}

/**
 * The app supports many providers and local runtimes, so there is no single
 * tokenizer that can run client-side without a model. Use the same 4 chars /
 * token approximation as contextTrimmer.ts for a transparent cost hint.
 */
export function estimatePastedTextTokens(text: string): number {
  return Math.max(1, Math.ceil(text.length / CHARS_PER_TOKEN_ESTIMATE));
}

export function formatEstimatedTokens(text: string): string {
  const tokens = estimatePastedTextTokens(text);
  if (tokens < 1_000) return `~${tokens} tokens`;
  if (tokens < 10_000) return `~${(tokens / 1_000).toFixed(1)}k tokens`;
  return `~${Math.round(tokens / 1_000)}k tokens`;
}

export function formatPastedTextSize(text: string): string {
  const bytes = new TextEncoder().encode(text).byteLength;
  if (bytes < 1_024) return `${bytes} B`;
  if (bytes < 1_024 * 1_024) return `${(bytes / 1_024).toFixed(bytes < 10 * 1_024 ? 1 : 0)} KB`;
  return `${(bytes / (1_024 * 1_024)).toFixed(1)} MB`;
}

export function isPastedTextPath(path: string): boolean {
  return path.startsWith("pasted://");
}

export function nextPastedTextName(existing: readonly PastedTextLike[]): string {
  let max = 0;
  for (const entry of existing) {
    if (!isPastedTextPath(entry.path)) continue;
    const label = entry.label ?? "";
    const match = /^Pasted text \((\d+)\)\.md$/i.exec(label);
    if (match) max = Math.max(max, Number(match[1]));
  }
  return `Pasted text (${max + 1}).md`;
}

export function pastedTextPath(name: string): string {
  const id = typeof crypto !== "undefined" && typeof crypto.randomUUID === "function"
    ? crypto.randomUUID()
    : `${Date.now()}-${Math.random().toString(36).slice(2)}`;
  return `pasted://${id}/${encodeURIComponent(name)}`;
}

/**
 * A collapsed paste is only a composer representation. At submit time its
 * exact text is reconstructed into the user prompt before any model routing,
 * privacy scan, mutation detection, skill parsing, Compare, or Crew logic.
 * This preserves the semantics and token usage of an ordinary paste while
 * keeping the UI usable. Pasted blocks are ordered by attachment order, then
 * followed by any text left visible in the composer.
 */
export function composePromptWithPastedText(
  visibleInput: string,
  attachments: readonly PastedTextLike[],
): string {
  const blocks = attachments
    .filter((attachment) => isPastedTextPath(attachment.path))
    .map((attachment) => attachment.content ?? "")
    .filter((content) => content.length > 0);
  if (visibleInput.length > 0) blocks.push(visibleInput);
  return blocks.join("\n\n");
}
