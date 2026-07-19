/**
 * Pure helpers for "@"-mention / attachment reference expansion, extracted
 * from `agentLoop.ts` so they can be unit-tested without pulling in the
 * Tauri- and store-dependent agent loop. `agentLoop.ts` owns the async
 * resolution (file reads via Tauri commands); everything here is pure
 * string/array work.
 */

import { wrapUntrustedContent } from './untrustedContent';

/** Matches "@"-mention tokens in raw user text, e.g. "@src/lib/tools.ts". */
const MENTION_REGEX = /@([^\s]+)/g;

/** Cap on how much of a single referenced file's content is inlined into the
 * outgoing wire payload, mirroring the FileTree preview's truncation cap. */
export const MAX_MENTION_CONTENT_CHARS = 20_000;

/** A single text reference (text "@"-mention or explicit non-image attachment) that was successfully resolved. */
export interface ResolvedTextReference {
  path: string;
  isDir: boolean;
  /** File content (isDir false) or a newline-joined directory listing (isDir true). */
  content: string;
  /** Inline terminal evidence is still untrusted context, but it is not a
   * workspace file and should be labeled accurately in the model payload. */
  source?: 'workspace' | 'terminal';
}

/** Shape returned per-entry by the Rust `tool_list_dir` command. */
export interface DirEntry {
  name: string;
  is_dir: boolean;
  size: number;
}

/**
 * Extracts the unique set of candidate paths from "@"-mention tokens in raw
 * user text (e.g. "check @src/lib/tools.ts and @README.md"), stripping a
 * single trailing punctuation character (comma, period, or closing paren)
 * that's almost always sentence punctuation rather than part of the path.
 */
export function extractMentionPaths(text: string): string[] {
  const paths: string[] = [];
  const seen = new Set<string>();

  for (const match of text.matchAll(MENTION_REGEX)) {
    let raw = match[1];
    if (/[,.)]$/.test(raw)) {
      raw = raw.slice(0, -1);
    }
    if (raw && !seen.has(raw)) {
      seen.add(raw);
      paths.push(raw);
    }
  }

  return paths;
}

/**
 * Formats a directory's immediate entries as a simple newline-joined list —
 * "- name/" for subdirectories, "- name" for files — sorted directories-first
 * (then alphabetically within each group), for inlining into the wire
 * payload in place of a fenced file-content block.
 */
export function formatDirListing(entries: DirEntry[]): string {
  const sorted = [...entries].sort((a, b) => {
    if (a.is_dir !== b.is_dir) return a.is_dir ? -1 : 1;
    return a.name.localeCompare(b.name);
  });
  return sorted.map((entry) => `- ${entry.name}${entry.is_dir ? '/' : ''}`).join('\n');
}

/** Caps a referenced file's content, same pattern as the FileTree preview. */
export function truncateMentionContent(content: string): string {
  if (content.length <= MAX_MENTION_CONTENT_CHARS) return content;
  return `${content.slice(0, MAX_MENTION_CONTENT_CHARS)}\n\n[Truncated — file is larger than ${MAX_MENTION_CONTENT_CHARS} characters]`;
}

/**
 * Expands `userText` with a "Referenced files:" header and one "### path"
 * section per resolved text reference (text "@"-mentions and/or explicit
 * non-image attachments), or returns it verbatim when there are none.
 */
export function composeReferencedText(userText: string, textRefs: ResolvedTextReference[]): string {
  if (textRefs.length === 0) return userText;
  const sections = textRefs.map(({ path, isDir, content, source }) => {
    const bounded = isDir ? content : truncateMentionContent(content);
    const rendered = isDir ? bounded : `\`\`\`\n${bounded}\n\`\`\``;
    const sourceDescription = source === 'terminal'
      ? `terminal evidence ${path}`
      : isDir
        ? `workspace directory ${path}`
        : `workspace file ${path}`;
    return `### ${path}\n${wrapUntrustedContent(sourceDescription, rendered)}`;
  });
  const heading = textRefs.some((reference) => reference.source === 'terminal')
    ? 'Referenced context:'
    : 'Referenced files:';
  return `${heading}\n\n${sections.join('\n\n')}\n\n---\n\n${userText}`;
}
