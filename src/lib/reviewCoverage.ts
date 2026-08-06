/**
 * Review criteria coverage — answers "which of this change's acceptance
 * criteria does the diff actually satisfy, and which does nothing touch?"
 * for the Review panel (`components/Workspace/ReviewPanel.tsx`), on top of
 * the `git_review` payload that panel already loads. No new Tauri command and
 * no new dependency: every fact here is derived from `git_review`'s
 * before/after content through `DiffViewer.ts`'s existing `computeDiff`.
 *
 * The mapping itself is model output, so this module keeps the two halves
 * apart *in the type system* rather than by convention:
 *
 * - [`ReviewFacts`] is what this app computed from git output. Hunk ids, line
 *   ranges, counts, and the criteria list are all app-owned.
 * - [`CheckedCriterionClaim`] is what a model said, already checked against
 *   those facts. A claim citing a hunk id that does not exist is `rejected`
 *   mechanically — not shown as coverage, not softened — and a claim of
 *   coverage carrying no surviving citation is `unsupported`. This is the
 *   only thing standing between "the model summarized the diff" and "the
 *   model asserted something about a diff it may not have read".
 * - The headline numbers ([`ReviewCoverageReport.uncoveredCriterionIds`] and
 *   `uncitedHunkIds`) are computed by set difference over accepted claims, so
 *   the count a reviewer acts on is never a number a model chose.
 *
 * Honest limits, stated where a reader can see them: symbol detection is a
 * TEXT match over changed declaration lines (see [`ReviewFileFact.changedSymbols`]),
 * not a type-resolved reference graph — this repo is half Rust, so a
 * TypeScript-only graph would be confidently wrong about half of it. Binary
 * and oversized files reach the panel with no content at all (`git.rs`'s
 * `MAX_REVIEW_FILE_BYTES`), so they contribute no citable hunks and a
 * criterion satisfied only by one of them can only come back `unsupported`.
 */
import { resolveTarget } from './agentLoop';
import { attemptStream, describeUsageTarget } from './turnEngine';
import { effortForTarget } from '../store/modelStore';
import { computeDiff, type DiffLine } from '../components/Workspace/DiffViewer';
import { wrapUntrustedContent } from './untrustedContent';
import type { ChatMessage } from './llamaClient';

/** Mirrors Rust `ReviewFilePayload` in src-tauri/src/git.rs — the subset this
 * module reads, so callers can pass the panel's payload straight through. */
export interface ReviewCoverageFile {
  path: string;
  old_content: string;
  new_content: string;
  added: number;
  deleted: number;
  binary: boolean;
}

/** The `git_review` fields this module needs. Mirrors Rust `ReviewPayload`. */
export interface ReviewCoverageInput {
  branch: string | null;
  target: string | null;
  total_added: number;
  total_deleted: number;
  files: ReviewCoverageFile[];
}

/** Which base the reviewed diff was taken against, carried into the report so
 * a saved report can never be read as being about the other one. */
export type ReviewBaseMode = 'branch' | 'working';

/** Safety cap on criteria per run — a pasted document is a mistake, not a
 * criteria list, and is rejected rather than silently truncated. */
export const MAX_REVIEW_CRITERIA = 24;
/** Cap on citable hunks. Past this the change is too large for one coverage
 * pass to be meaningful; the caller is told rather than given a partial map. */
export const MAX_REVIEW_HUNKS = 200;
/**
 * Mirrors `MAX_REVIEW_FILES` in src-tauri/src/git.rs:451. `git_review` skips
 * files past that cap with a bare `continue` *before* parsing them, so their
 * line counts never reach the totals and the payload carries no flag saying it
 * was capped. A coverage map that reported "C4 not covered" over a diff whose
 * tail it never saw would be exactly the failure this feature exists to catch,
 * so a payload at the cap is treated as possibly incomplete and said so.
 */
export const REVIEW_FILE_CAP = 300;
export const MAX_CRITERION_CHARS = 400;
export const MAX_RATIONALE_CHARS = 600;
/** Excerpt bounds per hunk, keeping the prompt bounded on a large diff. */
const MAX_EXCERPT_LINES = 8;
const MAX_EXCERPT_LINE_CHARS = 200;
/** Exported/public symbols reported per file. */
const MAX_SYMBOLS_PER_FILE = 12;

/**
 * One contiguous changed region of one file: a maximal run of non-unchanged
 * lines from [`computeDiff`]. This is the unit a model must cite — it is
 * app-assigned and app-numbered, so a fabricated reference is detectable by
 * lookup instead of by judgement.
 */
export interface ReviewHunkFact {
  /** `H1`, `H2`, … assigned in path-sorted, then file-order sequence. */
  hunkId: string;
  path: string;
  /** 1-based inclusive line range on the new side, or `null` for a hunk that
   * only removed lines (nothing of it remains in the new file). */
  newStart: number | null;
  newEnd: number | null;
  /** 1-based inclusive range on the old side, `null` for a pure addition. */
  oldStart: number | null;
  oldEnd: number | null;
  added: number;
  removed: number;
  /** Bounded `+`/`-` prefixed excerpt — what the model is shown for this hunk. */
  excerpt: string;
  /** True when the real hunk is longer than [`MAX_EXCERPT_LINES`], so the UI
   * can say the excerpt is partial instead of implying it is the whole hunk. */
  excerptTruncated: boolean;
}

export interface ReviewFileFact {
  path: string;
  added: number;
  removed: number;
  /** Binary or oversized: no content arrived, so no citable hunks exist. */
  binary: boolean;
  hunkIds: string[];
  /**
   * Names on added or removed `export …` / `pub …` declaration lines. Found by
   * TEXT MATCH on the changed lines themselves — this is not a type-resolved
   * symbol table and knows nothing about references, re-exports, or callers.
   * Shown as a hint about what the change exposes, never as impact analysis.
   */
  changedSymbols: string[];
}

/** One acceptance criterion, id assigned by this app so the model can only
 * ever refer to criteria that were actually given to it. */
export interface ReviewCriterion {
  /** `C1`, `C2`, … in the order the user supplied them. */
  criterionId: string;
  text: string;
}

/**
 * Everything this app computed itself. Nothing in here came from a model.
 */
export interface ReviewFacts {
  mode: ReviewBaseMode;
  branch: string | null;
  target: string | null;
  totalAdded: number;
  totalRemoved: number;
  files: ReviewFileFact[];
  hunks: ReviewHunkFact[];
  criteria: ReviewCriterion[];
  /** Files listed with no citable hunk (binary/oversized), kept explicit so
   * the gap is visible rather than looking like "nothing changed there". */
  uncitableFilePaths: string[];
  /** The payload sat at `git_review`'s file cap, so the diff may have a tail
   * this report never saw — every "not covered" verdict is unreliable while
   * this is true, and the UI says so instead of implying completeness. */
  filesPossiblyTruncated: boolean;
  /** More citable hunks existed than [`MAX_REVIEW_HUNKS`] allowed. */
  hunksPossiblyTruncated: boolean;
  /**
   * Deterministic digest of the facts above. A report whose digest no longer
   * matches a freshly computed one is about a different diff and is shown as
   * stale rather than re-interpreted against the new one. Change detection
   * only — not a security primitive.
   */
  digest: string;
}

/** What the model is allowed to say about a criterion. */
export type CriterionVerdict = 'covered' | 'partial' | 'uncovered';

/** A model claim after envelope validation but before citation checking. */
export interface RawCriterionClaim {
  criterionId: string;
  verdict: CriterionVerdict;
  citedHunkIds: string[];
  rationale: string;
}

/**
 * What happened to a claim once it was checked against [`ReviewFacts`]:
 * - `rejected` — it cited at least one hunk id that does not exist. The claim
 *   is not evidence of anything; a model that invents a reference here has
 *   demonstrated it is not reading the facts it was given.
 * - `unsupported` — it claimed `covered`/`partial` but no valid citation
 *   survived, so there is nothing to show a reviewer.
 * - `accepted` — `covered`/`partial` with at least one real citation, or a
 *   plain `uncovered` verdict, which needs no evidence to be actionable.
 */
export type ClaimStatus = 'accepted' | 'unsupported' | 'rejected';

export interface CheckedCriterionClaim {
  criterionId: string;
  claimed: CriterionVerdict;
  /** Cited ids that exist in the facts. */
  validCitations: string[];
  /** Cited ids that do not exist — the fabrication evidence itself, kept so
   * the UI can show what was invented rather than just hiding the claim. */
  invalidCitations: string[];
  rationale: string;
  status: ClaimStatus;
}

export interface ReviewCoverageReport {
  /** What the app computed. */
  computed: ReviewFacts;
  /** What the model claimed, each already checked against `computed`. */
  claims: CheckedCriterionClaim[];
  /**
   * COMPUTED, not claimed: criteria with no accepted `covered` claim. The
   * point of the whole feature — an agent that quietly skipped a criterion
   * shows up here even when its own summary said otherwise.
   */
  uncoveredCriterionIds: string[];
  /** COMPUTED: criteria whose only claims were rejected or unsupported. */
  unverifiedCriterionIds: string[];
  /** COMPUTED: hunks no accepted claim cites — changes no criterion asked for. */
  uncitedHunkIds: string[];
  modelLabel: string;
  createdAtMs: number;
}

function clamp(value: string, max: number): string {
  const trimmed = value.trim();
  return trimmed.length > max ? `${trimmed.slice(0, max)}…` : trimmed;
}

/** Matches `crewRunner.ts`'s `stripJsonFence` — the model is asked for bare
 * JSON but wraps it in a ```json fence often enough that every JSON-envelope
 * caller in this codebase strips it defensively. */
function stripJsonFence(raw: string): string {
  const trimmed = raw.trim();
  const fenced = trimmed.match(/^```(?:json)?\s*([\s\S]*?)\s*```$/i);
  return (fenced?.[1] ?? trimmed).trim();
}

/**
 * FNV-1a over the canonical fact string. Deliberately not a crypto hash: this
 * only has to change when the diff changes, and it must stay synchronous so
 * facts can be computed during a render pass.
 */
function digestFacts(canonical: string): string {
  let hash = 0x811c9dc5;
  for (let index = 0; index < canonical.length; index += 1) {
    hash ^= canonical.charCodeAt(index);
    hash = Math.imul(hash, 0x01000193) >>> 0;
  }
  return hash.toString(16).padStart(8, '0');
}

/** TypeScript/TSX exported declarations. */
const TS_EXPORT_DECL = /^\s*export\s+(?:default\s+)?(?:declare\s+)?(?:abstract\s+)?(?:async\s+)?(?:function\*?|class|const|let|var|interface|type|enum|namespace)\s+([A-Za-z_$][\w$]*)/;
/** Rust public items. `pub(crate)`/`pub(super)` count — they are still the
 * file's declared surface, and this is a hint, not a visibility analysis. */
const RUST_PUB_DECL = /^\s*pub(?:\s*\([^)]*\))?\s+(?:async\s+)?(?:unsafe\s+)?(?:extern\s+"[^"]*"\s+)?(?:fn|struct|enum|trait|union|const|static|type|mod)\s+([A-Za-z_]\w*)/;

/** Names declared on changed lines only — an unchanged declaration nearby is
 * deliberately not reported, since nothing about it changed. */
function changedSymbolsIn(lines: DiffLine[]): string[] {
  const names: string[] = [];
  for (const line of lines) {
    if (line.type === 'unchanged') continue;
    const match = TS_EXPORT_DECL.exec(line.text) ?? RUST_PUB_DECL.exec(line.text);
    const name = match?.[1];
    if (name && !names.includes(name)) {
      names.push(name);
      if (names.length >= MAX_SYMBOLS_PER_FILE) break;
    }
  }
  return names;
}

/** Maximal runs of non-unchanged lines: one run is one citable hunk. */
function hunkRuns(lines: DiffLine[]): DiffLine[][] {
  const runs: DiffLine[][] = [];
  let index = 0;
  while (index < lines.length) {
    if (lines[index].type === 'unchanged') {
      index += 1;
      continue;
    }
    const start = index;
    while (index < lines.length && lines[index].type !== 'unchanged') index += 1;
    runs.push(lines.slice(start, index));
  }
  return runs;
}

function excerptOf(run: DiffLine[]): { excerpt: string; truncated: boolean } {
  const shown = run.slice(0, MAX_EXCERPT_LINES);
  const excerpt = shown
    .map((line) => `${line.type === 'added' ? '+' : '-'}${clamp(line.text, MAX_EXCERPT_LINE_CHARS)}`)
    .join('\n');
  return { excerpt, truncated: run.length > shown.length };
}

/**
 * Normalizes a free-text criteria block (one per line, `-`/`*`/`1.` bullets
 * tolerated) into app-owned criteria. Blank lines are dropped; the caller is
 * told when there are too many rather than having the tail silently cut.
 */
export function parseCriteriaInput(raw: string): ReviewCriterion[] {
  const lines = raw
    .split('\n')
    .map((line) => line.replace(/^\s*(?:[-*•]|\d+[.)])\s*/, '').trim())
    .filter((line) => line.length > 0);
  if (lines.length > MAX_REVIEW_CRITERIA) {
    throw new Error(`Coverage runs over at most ${MAX_REVIEW_CRITERIA} criteria — ${lines.length} were given.`);
  }
  return lines.map((text, index) => ({
    criterionId: `C${index + 1}`,
    text: clamp(text, MAX_CRITERION_CHARS),
  }));
}

/**
 * Derives every citable fact from a `git_review` payload. Pure and
 * deterministic: the same payload and criteria always produce byte-identical
 * facts, including the digest, which is what makes a stored report checkable
 * against a fresh read later.
 *
 * Not called during normal panel rendering — a coverage pass is explicit, and
 * this re-diffs every file, which is real work on a large review.
 */
export function computeReviewFacts(
  review: ReviewCoverageInput,
  criteria: ReviewCriterion[],
  mode: ReviewBaseMode,
): ReviewFacts {
  const files: ReviewFileFact[] = [];
  const hunks: ReviewHunkFact[] = [];
  const uncitableFilePaths: string[] = [];
  /** Digest input. Every CHANGED line's text goes in, not just the bounded
   * excerpt: an edit that replaces a line in place leaves ranges and counts
   * identical, and a digest built from those alone would call a stale report
   * fresh. Changed lines plus their ranges pin the diff exactly, and their
   * volume is by definition the size of the diff, not of the files. */
  const canonicalParts: string[] = [mode, review.branch ?? '', review.target ?? ''];
  let hunkNumber = 0;
  let hunksPossiblyTruncated = false;

  const sorted = [...review.files].sort((a, b) => a.path.localeCompare(b.path));
  for (const file of sorted) {
    if (file.binary) {
      uncitableFilePaths.push(file.path);
      // No content arrived, so the counts are all there is to pin it by.
      canonicalParts.push(`B:${file.path}:${file.added}+${file.deleted}-`);
      files.push({
        path: file.path,
        added: file.added,
        removed: file.deleted,
        binary: true,
        hunkIds: [],
        changedSymbols: [],
      });
      continue;
    }

    const lines = computeDiff(file.old_content, file.new_content);
    const hunkIds: string[] = [];
    for (const run of hunkRuns(lines)) {
      if (hunkNumber >= MAX_REVIEW_HUNKS) {
        hunksPossiblyTruncated = true;
        break;
      }
      hunkNumber += 1;
      const hunkId = `H${hunkNumber}`;
      const newSide = run.filter((line) => line.newLineNo !== null);
      const oldSide = run.filter((line) => line.oldLineNo !== null);
      const { excerpt, truncated } = excerptOf(run);
      canonicalParts.push(
        `${hunkId}:${file.path}:${run[0].oldLineNo ?? ''}/${run[0].newLineNo ?? ''}`,
        ...run.map((line) => `${line.type === 'added' ? '+' : '-'}${line.text}`),
      );
      hunks.push({
        hunkId,
        path: file.path,
        newStart: newSide.length > 0 ? newSide[0].newLineNo : null,
        newEnd: newSide.length > 0 ? newSide[newSide.length - 1].newLineNo : null,
        oldStart: oldSide.length > 0 ? oldSide[0].oldLineNo : null,
        oldEnd: oldSide.length > 0 ? oldSide[oldSide.length - 1].oldLineNo : null,
        added: run.filter((line) => line.type === 'added').length,
        removed: run.filter((line) => line.type === 'removed').length,
        excerpt,
        excerptTruncated: truncated,
      });
      hunkIds.push(hunkId);
    }

    if (hunkIds.length === 0) uncitableFilePaths.push(file.path);
    files.push({
      path: file.path,
      added: file.added,
      removed: file.deleted,
      binary: false,
      hunkIds,
      changedSymbols: changedSymbolsIn(lines),
    });
  }

  for (const criterion of criteria) {
    canonicalParts.push(`${criterion.criterionId}:${criterion.text}`);
  }

  const filesPossiblyTruncated = review.files.length >= REVIEW_FILE_CAP;
  if (filesPossiblyTruncated) canonicalParts.push('truncated:files');
  if (hunksPossiblyTruncated) canonicalParts.push('truncated:hunks');

  return {
    mode,
    branch: review.branch,
    target: review.target,
    totalAdded: review.total_added,
    totalRemoved: review.total_deleted,
    files,
    hunks,
    criteria,
    uncitableFilePaths,
    filesPossiblyTruncated,
    hunksPossiblyTruncated,
    digest: digestFacts(canonicalParts.join('\n')),
  };
}

/** True when a stored report is still about the diff currently on screen. */
export function isReportStale(report: ReviewCoverageReport, fresh: ReviewFacts): boolean {
  return report.computed.digest !== fresh.digest;
}

export function buildCoverageMessages(facts: ReviewFacts): ChatMessage[] {
  const system = [
    "You are Little Monkey's review coverage mapper. You map acceptance criteria onto a diff. You never edit files, run commands, or call tools here.",
    'You are given numbered criteria (C1, C2, …) and numbered diff hunks (H1, H2, …). For every criterion, decide whether the diff satisfies it, and cite the hunk ids that show it.',
    'Reply with ONLY one JSON object, no Markdown code fence, no prose before or after, matching exactly this shape:',
    '{"claims":[{"criterionId":"C1","verdict":"covered|partial|uncovered","citedHunkIds":["H3"],"rationale":"what in those hunks satisfies (or fails) this criterion"}]}',
    'Rules that are checked mechanically after you reply, so violating them discards your claim:',
    '- Only cite hunk ids that appear in the facts below. A cited id that does not exist rejects the whole claim.',
    '- "covered" and "partial" REQUIRE at least one real cited hunk id. Never claim coverage you cannot cite.',
    '- Use "uncovered" with an empty citation list when nothing in the diff addresses the criterion. That is a useful, expected answer — do not stretch an unrelated hunk to avoid it.',
    '- Emit exactly one claim per given criterion id, and no criterion ids other than the ones given.',
    'Hunk excerpts may be truncated. If an excerpt is too partial to judge, say so in the rationale and prefer "partial" over "covered".',
    ...(facts.filesPossiblyTruncated || facts.hunksPossiblyTruncated
      ? ['This diff is larger than one pass can carry, so the files and hunks below may be an incomplete tail-truncated view. Say so in the rationale of any criterion you would otherwise call "uncovered" — absence from this list is not proof of absence from the change.']
      : []),
  ].join('\n');

  const criteriaBlock = facts.criteria
    .map((criterion) => `${criterion.criterionId}: ${criterion.text}`)
    .join('\n');

  const hunksBlock = facts.hunks
    .map((hunk) => {
      const range = hunk.newStart !== null
        ? `new lines ${hunk.newStart}-${hunk.newEnd}`
        : `removed old lines ${hunk.oldStart}-${hunk.oldEnd}`;
      const truncated = hunk.excerptTruncated ? ' (excerpt truncated)' : '';
      return `${hunk.hunkId} ${hunk.path} — ${range}, +${hunk.added}/-${hunk.removed}${truncated}\n${hunk.excerpt}`;
    })
    .join('\n\n');

  const filesBlock = facts.files
    .map((file) => {
      const symbols = file.changedSymbols.length > 0
        ? ` changed declarations (text-matched): ${file.changedSymbols.join(', ')}`
        : '';
      const binary = file.binary ? ' [binary/oversized — no content available]' : '';
      return `- ${file.path} +${file.added}/-${file.removed}${binary}${symbols}`;
    })
    .join('\n');

  // The diff, the criteria, and the file list are all content this app did
  // not author — the same treatment `issueToPrRunner.ts` gives an issue body.
  const user = wrapUntrustedContent(
    `computed review facts (${facts.mode} base${facts.branch ? `, branch ${facts.branch}` : ''})`,
    [
      'CRITERIA:',
      criteriaBlock,
      '',
      'CHANGED FILES:',
      filesBlock,
      '',
      'HUNKS:',
      hunksBlock || '(no citable hunks)',
    ].join('\n'),
  );

  return [
    { role: 'system', content: system },
    { role: 'user', content: user },
  ];
}

/**
 * Validates the envelope's shape only. Exported separately from
 * [`mapReviewCoverage`] so the contract can be unit-tested against fixed
 * strings without mocking a model call — the convention
 * `crossRepoChangePlanner.ts`'s `parsePlanEnvelope` established.
 *
 * Shape errors throw. Semantic problems — a bad citation, an uncitable
 * coverage claim — are NOT errors here: they are findings, and
 * [`checkClaims`] records them instead of hiding them behind a failure.
 */
export function parseCoverageEnvelope(raw: string, facts: ReviewFacts): RawCriterionClaim[] {
  let parsed: unknown;
  try {
    parsed = JSON.parse(stripJsonFence(raw));
  } catch {
    throw new Error('The model did not return the required JSON coverage envelope.');
  }
  if (!parsed || typeof parsed !== 'object' || Array.isArray(parsed)) {
    throw new Error('Coverage envelope was not a JSON object.');
  }
  const value = parsed as Record<string, unknown>;
  if (!Array.isArray(value.claims)) {
    throw new Error('Coverage envelope did not include a claims array.');
  }

  const criterionIds = new Set(facts.criteria.map((criterion) => criterion.criterionId));
  const seen = new Set<string>();
  const claims: RawCriterionClaim[] = [];

  for (const rawClaim of value.claims) {
    if (!rawClaim || typeof rawClaim !== 'object' || Array.isArray(rawClaim)) continue;
    const item = rawClaim as Record<string, unknown>;
    const criterionId = typeof item.criterionId === 'string' ? item.criterionId : '';
    // A claim about a criterion that was never given is dropped outright:
    // there is no criterion for a reviewer to act on, so there is nothing to
    // record against. Fabricated *citations* are kept and reported instead.
    if (!criterionIds.has(criterionId) || seen.has(criterionId)) continue;
    seen.add(criterionId);

    const verdictRaw = typeof item.verdict === 'string' ? item.verdict.toLowerCase() : '';
    const verdict: CriterionVerdict =
      verdictRaw === 'covered' || verdictRaw === 'partial' ? verdictRaw : 'uncovered';

    const citedHunkIds = Array.isArray(item.citedHunkIds)
      ? item.citedHunkIds.filter((id): id is string => typeof id === 'string').map((id) => id.trim())
      : [];

    claims.push({
      criterionId,
      verdict,
      citedHunkIds: [...new Set(citedHunkIds)],
      rationale: typeof item.rationale === 'string' ? clamp(item.rationale, MAX_RATIONALE_CHARS) : '',
    });
  }

  if (claims.length === 0) {
    throw new Error('Coverage envelope contained no claim about any of the given criteria.');
  }
  return claims;
}

/**
 * The anti-fabrication check: every citation is looked up in the computed
 * facts, and a claim's status follows from what survives. A criterion the
 * model said nothing about is recorded as an unsupported `uncovered` claim
 * rather than dropped, so the report always accounts for every criterion.
 */
export function checkClaims(claims: RawCriterionClaim[], facts: ReviewFacts): CheckedCriterionClaim[] {
  const hunkIds = new Set(facts.hunks.map((hunk) => hunk.hunkId));
  const byCriterion = new Map(claims.map((claim) => [claim.criterionId, claim]));

  return facts.criteria.map((criterion): CheckedCriterionClaim => {
    const claim = byCriterion.get(criterion.criterionId);
    if (!claim) {
      return {
        criterionId: criterion.criterionId,
        claimed: 'uncovered',
        validCitations: [],
        invalidCitations: [],
        rationale: '',
        status: 'unsupported',
      };
    }

    const validCitations = claim.citedHunkIds.filter((id) => hunkIds.has(id));
    const invalidCitations = claim.citedHunkIds.filter((id) => !hunkIds.has(id));

    let status: ClaimStatus;
    if (invalidCitations.length > 0) {
      status = 'rejected';
    } else if (claim.verdict === 'uncovered') {
      status = 'accepted';
    } else {
      status = validCitations.length > 0 ? 'accepted' : 'unsupported';
    }

    return {
      criterionId: claim.criterionId,
      claimed: claim.verdict,
      validCitations,
      invalidCitations,
      rationale: claim.rationale,
      status,
    };
  });
}

/**
 * Assembles the report and computes its roll-ups. Every number here comes
 * from set arithmetic over accepted claims — a rejected or unsupported claim
 * can never move a count, which is the whole reason the counts are worth
 * looking at.
 */
export function buildCoverageReport(
  facts: ReviewFacts,
  claims: CheckedCriterionClaim[],
  modelLabel: string,
): ReviewCoverageReport {
  const accepted = claims.filter((claim) => claim.status === 'accepted');
  const acceptedCovered = accepted.filter((claim) => claim.claimed === 'covered');

  const coveredIds = new Set(acceptedCovered.map((claim) => claim.criterionId));
  const uncoveredCriterionIds = facts.criteria
    .map((criterion) => criterion.criterionId)
    .filter((id) => !coveredIds.has(id));

  const unverifiedCriterionIds = claims
    .filter((claim) => claim.status !== 'accepted')
    .map((claim) => claim.criterionId);

  const cited = new Set(accepted.flatMap((claim) => claim.validCitations));
  const uncitedHunkIds = facts.hunks
    .map((hunk) => hunk.hunkId)
    .filter((id) => !cited.has(id));

  return {
    computed: facts,
    claims,
    uncoveredCriterionIds,
    unverifiedCriterionIds,
    uncitedHunkIds,
    modelLabel,
    createdAtMs: Date.now(),
  };
}

/**
 * Runs one coverage pass over a review payload. Read-only: a single planning-
 * style model call with no tools, following `crossRepoChangePlanner.ts`'s
 * one-shot recipe exactly, including `recordUsage: false` — there is no chat
 * session for a panel-initiated call to attribute tokens to.
 */
export async function mapReviewCoverage(
  review: ReviewCoverageInput,
  criteria: ReviewCriterion[],
  mode: ReviewBaseMode,
  signal?: AbortSignal,
): Promise<ReviewCoverageReport> {
  if (criteria.length === 0) {
    throw new Error('Add at least one acceptance criterion before checking coverage.');
  }
  const facts = computeReviewFacts(review, criteria, mode);
  if (facts.hunks.length === 0) {
    throw new Error('This review has no citable text changes to map criteria onto.');
  }

  const target = await resolveTarget();
  const effort = effortForTarget(target);
  const result = await attemptStream(
    target,
    buildCoverageMessages(facts),
    [],
    signal,
    effort,
    `review-coverage:${crypto.randomUUID()}`,
    undefined,
    false,
  );
  if (result.streamError) throw new Error(result.streamError);

  const claims = checkClaims(parseCoverageEnvelope(result.content, facts), facts);
  return buildCoverageReport(facts, claims, describeUsageTarget(target));
}
