/**
 * Pure helpers behind an assistant answer's footer actions (see
 * `MessageActions.tsx`) — kept out of the component so both the chapter
 * title derivation and the timestamp formatting are unit-testable without
 * rendering React.
 */

/** Max length, in characters, of a chapter title derived from an answer. */
const CHAPTER_TITLE_MAX_LENGTH = 48;

/**
 * Derives the label a pinned chapter is shown with from the answer's own
 * text: its first non-empty line, stripped of the Markdown that would
 * otherwise leak into a plain-text divider (heading hashes, list bullets,
 * emphasis, inline code, link syntax) and truncated.
 *
 * Falls back to "Chapter" for an answer with no usable text at all (an
 * image-only or whitespace-only message) rather than pinning a blank label.
 */
export function chapterTitle(text: string): string {
  const line = text
    .split("\n")
    .map((candidate) => candidate.trim())
    .find((candidate) => candidate.length > 0);
  if (!line) return "Chapter";

  const plain = line
    .replace(/^#{1,6}\s*/, "")
    .replace(/^[-*+]\s+/, "")
    .replace(/^>\s+/, "")
    .replace(/`([^`]*)`/g, "$1")
    .replace(/\*\*([^*]+)\*\*/g, "$1")
    .replace(/[*_]([^*_]+)[*_]/g, "$1")
    .replace(/\[([^\]]*)\]\([^)]*\)/g, "$1")
    .trim();

  if (!plain) return "Chapter";
  return plain.length > CHAPTER_TITLE_MAX_LENGTH ? `${plain.slice(0, CHAPTER_TITLE_MAX_LENGTH).trimEnd()}…` : plain;
}

const MINUTE_MS = 60_000;
const HOUR_MS = 60 * MINUTE_MS;
const DAY_MS = 24 * HOUR_MS;

/**
 * Relative label for a message's timestamp ("15 minutes ago", "yesterday"),
 * matching how the footer reads at a glance; anything a week or more old
 * shows its date instead, since "51 days ago" is harder to place than
 * "Jun 19". The absolute form always stays available as the element's
 * tooltip (see `MessageActions.tsx`).
 *
 * `now` is a parameter rather than a `Date.now()` call so the label is a
 * pure function of its inputs.
 */
export function formatMessageTime(at: number, now: number, locale?: string): string {
  const elapsed = now - at;
  // A clock adjustment (or a transcript synced from a machine running
  // slightly ahead) can put a message marginally in the future; treat that
  // as "just now" rather than rendering "in 3 seconds".
  if (elapsed < MINUTE_MS) return "just now";

  const relative = new Intl.RelativeTimeFormat(locale, { numeric: "auto" });
  if (elapsed < HOUR_MS) return relative.format(-Math.floor(elapsed / MINUTE_MS), "minute");
  if (elapsed < DAY_MS) return relative.format(-Math.floor(elapsed / HOUR_MS), "hour");
  if (elapsed < 7 * DAY_MS) return relative.format(-Math.floor(elapsed / DAY_MS), "day");
  return new Date(at).toLocaleDateString(locale, { month: "short", day: "numeric" });
}

/** Full date/time shown as the timestamp's tooltip, named zone included so a
 * transcript read on another machine is unambiguous. */
export function formatMessageTimestamp(at: number, locale?: string): string {
  return new Date(at).toLocaleString(locale, {
    year: "numeric",
    month: "short",
    day: "numeric",
    hour: "numeric",
    minute: "2-digit",
    timeZoneName: "short",
  });
}
