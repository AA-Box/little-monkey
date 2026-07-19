/**
 * Visual Design Edit Mode (ROADMAP.md Phase 7: "Market-Defining
 * Differentiators" -> "Visual Design Edit Mode").
 *
 * MVP scope, deliberately built on TOP of Browser Workbench's existing
 * primitives rather than a new live-DOM-editing engine (see ROADMAP item's
 * own implementation guidance):
 *
 * - The user picks an element already surfaced by Browser Workbench's own
 *   `annotateBrowser` (selector/tag/role/ariaLabel/text/rect + a live
 *   screenshot) — this module never talks to a rendered page directly.
 * - `findCandidateFiles` locates likely-responsible source files the exact
 *   same way an agent turn would: `tool_grep`/`tool_read_file`, the same
 *   Rust commands `tools.ts` exposes to every chat turn.
 * - `proposeVisualEdit` then makes ONE local-model call (via `resolveTarget`
 *   + `attemptStream`, the same target-resolution/streaming primitives
 *   `sideTaskRunner.ts` uses for its own single-purpose model calls) asking
 *   the model to pick exactly one candidate file and return its full new
 *   contents, and turns the before/after pair into a unified diff.
 * - Nothing here writes to disk. `writeVisualEditToDisk` is a thin wrapper
 *   around `tool_write_file` — the SAME Rust command a normal chat turn's
 *   `write_file` tool call reaches, which itself calls
 *   `permissions::request_permission` before touching disk. Routing through
 *   it (instead of some bespoke visual-edit-only write path) is what makes
 *   an accepted visual edit go through the same permission/risk gate as any
 *   other code change, per the ROADMAP acceptance criterion.
 */
import { invoke } from '@tauri-apps/api/core';
import { resolveTarget } from './agentLoop';
import { attemptStream } from './turnEngine';
import type { ChatMessage } from './llamaClient';
import { parseModelJsonCandidates } from './modelJson';

/** The user-selected element, in the same shape `browserVerification.ts`'s
 * `BrowserAnnotation` already carries — callers pass `annotation.selector`,
 * `.tag`, `.role`, `.ariaLabel`, `.text`, `.rect` straight through. */
export interface VisualEditElement {
  selector: string;
  tag: string;
  role: string;
  ariaLabel: string;
  text: string;
  rect: { x: number; y: number; width: number; height: number };
}

export interface VisualEditRequest {
  element: VisualEditElement;
  /** Plain-text description of the desired change, e.g. "make this button
   * larger" or "change this to blue" — never parsed, just handed to the
   * model as-is. */
  description: string;
  /** The page URL the element was captured from, included only as context
   * for the model (helps it disambiguate between similarly-named
   * components across routes). */
  pageUrl: string;
}

export interface CandidateFile {
  path: string;
  content: string;
}

export interface VisualEditProposal {
  targetFile: string;
  oldContent: string;
  newContent: string;
  unifiedDiff: string;
  summary: string;
}

interface GrepMatch {
  file: string;
  line: number;
  text: string;
}

/** Only these extensions are considered when ranking grep hits — a match in
 * a `.test.ts`, `.md`, or generated file is never the right target for a
 * rendered-element edit. */
const SOURCE_FILE_EXTENSION_RE = /\.(tsx|jsx|vue|svelte)$/;
const MAX_CANDIDATE_FILES = 4;
const MAX_FILE_CHARS = 20_000;
const MAX_SEARCH_TERMS = 6;

function escapeRegExp(value: string): string {
  return value.replace(/[.*+?^${}()|[\]\\]/g, '\\$&');
}

/**
 * Derives grep-able search terms from a selected element: its full visible
 * text (if short enough to be a near-unique match), its aria-label, and its
 * longest individual words — cheap, deterministic, and good enough for the
 * common case of "the JSX literally contains this button's label".
 */
export function extractSearchTerms(element: VisualEditElement): string[] {
  const terms = new Set<string>();

  const text = (element.text ?? '').trim();
  if (text.length > 0 && text.length <= 60) {
    terms.add(text);
  }
  const words = text
    .split(/\s+/)
    .map((word) => word.replace(/[^\p{L}\p{N}_-]/gu, ''))
    .filter((word) => word.length >= 3);
  for (const word of words) {
    if (terms.size >= MAX_SEARCH_TERMS) break;
    terms.add(word);
  }

  const ariaLabel = (element.ariaLabel ?? '').trim();
  if (ariaLabel.length > 0 && terms.size < MAX_SEARCH_TERMS) {
    terms.add(ariaLabel);
  }

  return Array.from(terms).slice(0, MAX_SEARCH_TERMS);
}

/**
 * Searches the workspace (via `tool_grep`, the same command a chat turn's
 * `grep` tool call reaches) for files that mention the selected element's
 * text/label, ranks them by how many search terms they matched, and reads
 * the top few candidates in full (via `tool_read_file`) so the model has
 * real source to work from instead of guessing blind.
 */
export async function findCandidateFiles(
  element: VisualEditElement,
  maxFiles: number = MAX_CANDIDATE_FILES,
): Promise<CandidateFile[]> {
  const terms = extractSearchTerms(element);
  const scores = new Map<string, number>();

  for (const term of terms) {
    let matches: GrepMatch[];
    try {
      matches = await invoke<GrepMatch[]>('tool_grep', { pattern: escapeRegExp(term) });
    } catch {
      continue; // an invalid/oversized pattern for one term shouldn't sink the whole search
    }
    for (const match of matches) {
      if (!SOURCE_FILE_EXTENSION_RE.test(match.file)) continue;
      scores.set(match.file, (scores.get(match.file) ?? 0) + 1);
    }
  }

  const ranked = Array.from(scores.entries())
    .sort((a, b) => b[1] - a[1])
    .slice(0, maxFiles)
    .map(([file]) => file);

  const files: CandidateFile[] = [];
  for (const path of ranked) {
    try {
      const content = await invoke<string>('tool_read_file', { path });
      files.push({ path, content: content.length > MAX_FILE_CHARS ? content.slice(0, MAX_FILE_CHARS) : content });
    } catch {
      continue; // file vanished/unreadable between grep and read — skip it
    }
  }
  return files;
}

function buildSystemPrompt(): string {
  return [
    'You are a UI code-location and patch-proposal assistant embedded in a desktop code editor.',
    'You are given one selected on-screen element and a short list of candidate source files found by searching the workspace.',
    'Pick the single candidate file that most plausibly renders that element, then rewrite it to satisfy the requested change.',
    'Respond with ONLY a single JSON object — no markdown code fences, no commentary before or after it.',
  ].join(' ');
}

function buildUserPrompt(request: VisualEditRequest, candidates: CandidateFile[]): string {
  const elementLines = [
    `Tag: <${request.element.tag || 'unknown'}>`,
    request.element.role ? `Role: ${request.element.role}` : null,
    request.element.ariaLabel ? `Aria label: ${request.element.ariaLabel}` : null,
    request.element.text ? `Visible text: ${JSON.stringify(request.element.text.slice(0, 200))}` : null,
    `CSS selector: ${request.element.selector}`,
    `Bounding box: ${JSON.stringify(request.element.rect)}`,
    request.pageUrl ? `Page URL: ${request.pageUrl}` : null,
  ].filter((line): line is string => line !== null);

  const filesBlock =
    candidates.length > 0
      ? candidates.map((file) => `--- FILE: ${file.path} ---\n${file.content}`).join('\n\n')
      : '(no candidate files found)';

  return [
    'Selected element:',
    ...elementLines,
    '',
    `Requested change (verbatim, from the user): ${request.description}`,
    '',
    'Candidate source files (most likely match first, ranked by how many times they mention the element\'s text/label):',
    filesBlock,
    '',
    'Reply with ONLY a JSON object of this exact shape:',
    '{"targetFile": "<path exactly as shown in a FILE header above>", "newContent": "<the FULL new file content after applying the requested change>", "summary": "<one line describing the change>"}',
    "newContent must be the file's complete new contents (not a diff or snippet) — every line unrelated to the requested change must be preserved exactly as given.",
    'If none of the candidate files are a plausible match, reply with {"targetFile": null, "newContent": null, "summary": "<why nothing matched>"}.',
  ].join('\n');
}

export interface RawVisualEditProposal {
  targetFile: string | null;
  newContent: string | null;
  summary: string;
}

/** Parses the model response through the shared bounded, string-aware JSON
 * extractor, then applies this feature's own strict schema validation. */
export function parseProposalResponse(raw: string): RawVisualEditProposal | null {
  for (const obj of parseModelJsonCandidates(raw, 'object')) {
    const targetFile = typeof obj.targetFile === 'string' ? obj.targetFile : null;
    const newContent = typeof obj.newContent === 'string' ? obj.newContent : null;
    const summary = typeof obj.summary === 'string' ? obj.summary : '';
    return { targetFile, newContent, summary };
  }
  return null;
}

// ---------------------------------------------------------------------------
// Unified diff generation — a small, self-contained line-based diff (same
// LCS technique `DiffViewer.tsx` uses for its side-by-side view) formatted
// as a single standard unified-diff hunk. One hunk is intentionally enough
// for this feature's use case: a visual edit is by definition one localized
// change to one element, never a sprawling multi-region rewrite.
// ---------------------------------------------------------------------------

interface DiffOpLine {
  type: 'unchanged' | 'added' | 'removed';
  text: string;
  oldLineNo: number | null;
  newLineNo: number | null;
}

const DIFF_LCS_CELL_BUDGET = 2_000_000;

function splitLines(text: string): string[] {
  if (text === '') return [];
  return text.replace(/\r\n/g, '\n').split('\n');
}

function lcsDiffLines(a: string[], b: string[]): DiffOpLine[] {
  const n = a.length;
  const m = b.length;
  const dp: number[][] = Array.from({ length: n + 1 }, () => new Array<number>(m + 1).fill(0));

  for (let i = n - 1; i >= 0; i--) {
    for (let j = m - 1; j >= 0; j--) {
      dp[i][j] = a[i] === b[j] ? dp[i + 1][j + 1] + 1 : Math.max(dp[i + 1][j], dp[i][j + 1]);
    }
  }

  const result: DiffOpLine[] = [];
  let i = 0;
  let j = 0;
  let oldNo = 1;
  let newNo = 1;

  while (i < n && j < m) {
    if (a[i] === b[j]) {
      result.push({ type: 'unchanged', oldLineNo: oldNo++, newLineNo: newNo++, text: a[i] });
      i++;
      j++;
    } else if (dp[i + 1][j] >= dp[i][j + 1]) {
      result.push({ type: 'removed', oldLineNo: oldNo++, newLineNo: null, text: a[i] });
      i++;
    } else {
      result.push({ type: 'added', oldLineNo: null, newLineNo: newNo++, text: b[j] });
      j++;
    }
  }
  while (i < n) {
    result.push({ type: 'removed', oldLineNo: oldNo++, newLineNo: null, text: a[i] });
    i++;
  }
  while (j < m) {
    result.push({ type: 'added', oldLineNo: null, newLineNo: newNo++, text: b[j] });
    j++;
  }
  return result;
}

/** Cheap O(n+m) fallback for pathologically large files — trims the common
 * prefix/suffix and treats the middle as one wholesale removal + addition. */
function naiveDiffLines(a: string[], b: string[]): DiffOpLine[] {
  const n = a.length;
  const m = b.length;
  const maxPrefix = Math.min(n, m);

  let prefix = 0;
  while (prefix < maxPrefix && a[prefix] === b[prefix]) prefix++;

  let suffix = 0;
  const maxSuffix = maxPrefix - prefix;
  while (suffix < maxSuffix && a[n - 1 - suffix] === b[m - 1 - suffix]) suffix++;

  const result: DiffOpLine[] = [];
  for (let k = 0; k < prefix; k++) {
    result.push({ type: 'unchanged', oldLineNo: k + 1, newLineNo: k + 1, text: a[k] });
  }
  for (let k = prefix; k < n - suffix; k++) {
    result.push({ type: 'removed', oldLineNo: k + 1, newLineNo: null, text: a[k] });
  }
  for (let k = prefix; k < m - suffix; k++) {
    result.push({ type: 'added', oldLineNo: null, newLineNo: k + 1, text: b[k] });
  }
  for (let k = 0; k < suffix; k++) {
    const oldIdx = n - suffix + k;
    const newIdx = m - suffix + k;
    result.push({ type: 'unchanged', oldLineNo: oldIdx + 1, newLineNo: newIdx + 1, text: a[oldIdx] });
  }
  return result;
}

/** Builds a standard `--- a/<file>` / `+++ b/<file>` / `@@ ... @@` unified
 * diff of `oldContent` -> `newContent`. Returns `""` when the two are
 * identical (no hunk to show). */
export function computeUnifiedDiff(
  oldContent: string,
  newContent: string,
  filePath: string,
  contextLines: number = 3,
): string {
  const a = splitLines(oldContent);
  const b = splitLines(newContent);
  const diffLines = a.length * b.length > DIFF_LCS_CELL_BUDGET ? naiveDiffLines(a, b) : lcsDiffLines(a, b);

  const changedIndexes: number[] = [];
  diffLines.forEach((line, idx) => {
    if (line.type !== 'unchanged') changedIndexes.push(idx);
  });
  if (changedIndexes.length === 0) return '';

  const first = Math.max(0, changedIndexes[0] - contextLines);
  const last = Math.min(diffLines.length - 1, changedIndexes[changedIndexes.length - 1] + contextLines);
  const hunkLines = diffLines.slice(first, last + 1);

  const oldStart = hunkLines.find((line) => line.oldLineNo !== null)?.oldLineNo ?? 1;
  const newStart = hunkLines.find((line) => line.newLineNo !== null)?.newLineNo ?? 1;
  const oldCount = hunkLines.filter((line) => line.type !== 'added').length;
  const newCount = hunkLines.filter((line) => line.type !== 'removed').length;

  const header = [`--- a/${filePath}`, `+++ b/${filePath}`, `@@ -${oldStart},${oldCount} +${newStart},${newCount} @@`];
  const body = hunkLines.map(
    (line) => `${line.type === 'added' ? '+' : line.type === 'removed' ? '-' : ' '}${line.text}`,
  );
  return [...header, ...body].join('\n');
}

/**
 * Orchestrates one full visual-edit proposal: search the workspace for
 * candidate source files, ask the resolved model to pick one and rewrite
 * it, and turn the result into a unified diff. Throws a plain `Error` with
 * a user-facing message on any failure path (no candidate files, model
 * declined to match, model named a file it wasn't given, or the "new"
 * content is byte-identical to disk) — callers (the store) catch this and
 * surface `.message` directly.
 */
export async function proposeVisualEdit(
  request: VisualEditRequest,
  signal?: AbortSignal,
): Promise<VisualEditProposal> {
  const candidates = await findCandidateFiles(request.element);
  if (candidates.length === 0) {
    throw new Error(
      'Could not find a source file mentioning this element\'s text or label — try a more specific description.',
    );
  }

  const target = await resolveTarget();
  const messages: ChatMessage[] = [
    { role: 'system', content: buildSystemPrompt() },
    { role: 'user', content: buildUserPrompt(request, candidates) },
  ];

  // `recordUsage: false` — this single-shot classification call must not
  // clobber any real chat session's own context-usage ring, same reasoning
  // `sideTaskRunner.ts`/`subagent.ts` already document for their own
  // model calls that aren't the user's main turn.
  const attempt = await attemptStream(target, messages, [], signal, undefined, 'visual-edit-mode', undefined, false);
  if (attempt.streamError !== null) {
    throw new Error(attempt.streamError);
  }

  const parsed = parseProposalResponse(attempt.content);
  if (!parsed || !parsed.targetFile || parsed.newContent === null) {
    throw new Error(parsed?.summary || 'The model could not confidently map this element to a source file.');
  }

  const match = candidates.find((file) => file.path === parsed.targetFile);
  if (!match) {
    throw new Error(`The model proposed "${parsed.targetFile}", which wasn't among the searched candidate files.`);
  }

  const unifiedDiff = computeUnifiedDiff(match.content, parsed.newContent, match.path);
  if (unifiedDiff === '') {
    throw new Error('The proposed content is identical to what is already on disk — nothing to change.');
  }

  return {
    targetFile: match.path,
    oldContent: match.content,
    newContent: parsed.newContent,
    unifiedDiff,
    summary: parsed.summary.trim() || 'Visual edit',
  };
}

/**
 * Writes an accepted visual edit to disk via `tool_write_file` — the exact
 * same Rust command a chat turn's `write_file` tool call reaches, which
 * itself calls `permissions::request_permission` before writing. Nothing in
 * this module bypasses that gate.
 */
export async function writeVisualEditToDisk(targetFile: string, newContent: string): Promise<void> {
  await invoke('tool_write_file', { path: targetFile, content: newContent });
}
