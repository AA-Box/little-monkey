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

/**
 * A pasted card occupies a zero-width anchor in the visible composer. `offset`
 * is measured in UTF-16 code units, matching textarea selectionStart/End and
 * String.slice. `order` disambiguates multiple consecutive pastes at the same
 * zero-width anchor without adding synthetic separator text to the prompt.
 */
export interface PastedTextPlacement {
  path: string;
  offset: number;
  order: number;
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
  return Math.ceil(text.length / CHARS_PER_TOKEN_ESTIMATE);
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

export function nextPastedTextOrder(existing: readonly PastedTextPlacement[]): number {
  return existing.reduce((max, placement) => Math.max(max, placement.order), -1) + 1;
}

export function pastedTextPath(name: string): string {
  const id = typeof crypto !== "undefined" && typeof crypto.randomUUID === "function"
    ? crypto.randomUUID()
    : `${Date.now()}-${Math.random().toString(36).slice(2)}`;
  return `pasted://${id}/${encodeURIComponent(name)}`;
}

/**
 * Rebase zero-width pasted-card anchors after one textarea edit. Browser
 * textarea edits are contiguous replacements, so a longest-common-prefix /
 * suffix diff gives the exact edited range without a model or tokenizer.
 *
 * For a pure insertion exactly at an anchor, the anchor stays before the new
 * text. This matches the important post-paste behavior: after a paste is
 * collapsed, the caret remains at the anchor and newly typed text follows the
 * pasted block just as it would after a normal native paste.
 */
export function rebasePastedTextPlacements(
  previousText: string,
  nextText: string,
  placements: readonly PastedTextPlacement[],
): PastedTextPlacement[] {
  if (placements.length === 0 || previousText === nextText) return [...placements];

  let start = 0;
  const sharedLength = Math.min(previousText.length, nextText.length);
  while (start < sharedLength && previousText[start] === nextText[start]) start += 1;

  let previousEnd = previousText.length;
  let nextEnd = nextText.length;
  while (
    previousEnd > start &&
    nextEnd > start &&
    previousText[previousEnd - 1] === nextText[nextEnd - 1]
  ) {
    previousEnd -= 1;
    nextEnd -= 1;
  }

  const delta = (nextEnd - start) - (previousEnd - start);
  const pureInsertion = previousEnd === start;

  return placements.map((placement) => {
    const offset = Math.max(0, Math.min(previousText.length, placement.offset));
    if (pureInsertion) {
      if (offset <= start) return { ...placement, offset };
      return { ...placement, offset: offset + delta };
    }
    if (offset < start) return { ...placement, offset };
    if (offset >= previousEnd) return { ...placement, offset: offset + delta };
    return { ...placement, offset: start };
  });
}

/**
 * Reconstruct the semantic user message from the visible textarea plus its
 * zero-width pasted-card anchors. No headings, separators, trimming, or other
 * synthetic text are introduced: when placements are present the result is
 * exactly what a native textarea would contain if those pastes had remained
 * expanded in place.
 *
 * A missing placement is fail-safe rather than lossy: the pasted block is
 * appended at the end in attachment order. Composer-created cards always have
 * placements; this fallback only protects stale/incomplete in-memory state.
 */
export function composePromptWithPastedText(
  visibleInput: string,
  attachments: readonly PastedTextLike[],
  placements: readonly PastedTextPlacement[] = [],
): string {
  const placementByPath = new Map(placements.map((placement) => [placement.path, placement]));
  const pasted = attachments
    .map((attachment, attachmentIndex) => ({ attachment, attachmentIndex }))
    .filter(({ attachment }) => isPastedTextPath(attachment.path) && attachment.content !== undefined)
    .map(({ attachment, attachmentIndex }) => {
      const placement = placementByPath.get(attachment.path);
      return {
        path: attachment.path,
        content: attachment.content ?? "",
        attachmentIndex,
        offset: Math.max(0, Math.min(visibleInput.length, placement?.offset ?? visibleInput.length)),
        order: placement?.order ?? Number.MAX_SAFE_INTEGER,
      };
    })
    .filter(({ content }) => content.length > 0)
    .sort((left, right) =>
      left.offset - right.offset || left.order - right.order || left.attachmentIndex - right.attachmentIndex
    );

  if (pasted.length === 0) return visibleInput;

  let cursor = 0;
  let result = "";
  for (const block of pasted) {
    if (block.offset > cursor) {
      result += visibleInput.slice(cursor, block.offset);
      cursor = block.offset;
    }
    result += block.content;
  }
  result += visibleInput.slice(cursor);
  return result;
}
