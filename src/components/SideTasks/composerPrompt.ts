/**
 * Pure text helpers for the side-task composer (`SideTaskComposer.tsx`).
 *
 * The composer is a CHAT box, not a form: the user types instructions and
 * hits Enter, exactly like the main chat's composer, so the two things the
 * old form asked for separately — a title, and any files to look at — have
 * to be derived from what they typed plus what they attached. Kept out of
 * the component (same split as `Chat/activityTimeline.ts`) so the derivation
 * rules are testable without rendering anything.
 */

export interface SideTaskAttachment {
  path: string;
  isDir: boolean;
}

const MAX_TITLE_LENGTH = 60;

/**
 * Tab-strip title for a task started from free-typed instructions: the first
 * non-empty line, stripped of the markdown lead-ins people open a prompt
 * with, collapsed to one line, and cut on a word boundary.
 *
 * A seeded task (started from a message/file/terminal action) already has a
 * title its source built — callers pass that through instead of calling this.
 */
export function deriveSideTaskTitle(prompt: string): string {
  const firstLine = prompt
    .split("\n")
    .map((line) => line.trim())
    .find((line) => line.length > 0);
  if (!firstLine) return "Side task";

  const cleaned = firstLine
    // Leading markdown heading marks, list bullets, and blockquote arrows —
    // noise in a tab chip, never meaning.
    .replace(/^[#>\s]*[-*+]?\s*/, "")
    .replace(/\s+/g, " ")
    .trim();
  if (!cleaned) return "Side task";
  if (cleaned.length <= MAX_TITLE_LENGTH) return cleaned;

  const cut = cleaned.slice(0, MAX_TITLE_LENGTH);
  const lastSpace = cut.lastIndexOf(" ");
  // Only honour the word boundary when it isn't so early that the title
  // becomes a single truncated word.
  const head = lastSpace > MAX_TITLE_LENGTH / 2 ? cut.slice(0, lastSpace) : cut;
  return `${head.trimEnd()}…`;
}

/**
 * Appends attached paths to the typed instructions as a plain list. A side
 * task's tools already include `read_file`/`list_dir` under either profile,
 * so naming the paths is all the run needs — nothing is read at attach time,
 * unlike the main composer's image attachments.
 */
export function appendAttachmentContext(prompt: string, attachments: readonly SideTaskAttachment[]): string {
  if (attachments.length === 0) return prompt;
  const lines = attachments.map(
    (attachment) => `- ${attachment.path}${attachment.isDir ? " (directory)" : ""}`,
  );
  const block = `Files in scope:\n${lines.join("\n")}`;
  const body = prompt.trim();
  return body ? `${body}\n\n${block}` : block;
}

/** Label for the `selected_files` source a manual task picks up once the
 * user attaches paths to it, so its card reads the same as one started from
 * the file tree's own "Start side task" action. */
export function attachmentSourceLabel(attachments: readonly SideTaskAttachment[]): string {
  return attachments.length === 1 ? "1 attached path" : `${attachments.length} attached paths`;
}
