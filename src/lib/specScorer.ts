/**
 * Agent-Ready Spec Scorer (ROADMAP.md Phase 7, item 4) — an advisory,
 * one-shot LLM judge that scores a GitHub issue/spec's readiness for an
 * autonomous coding agent (the Issue-to-PR Agent Flow's headless
 * `issueToPrRunner.ts` implementation phase) BEFORE that implementation
 * commits to a plan, and lists the concrete missing information that would
 * most improve it.
 *
 * Structurally this mirrors `riskJudge.ts`'s `classifyToolCall` almost
 * exactly — the same one-shot, non-streaming, tool-less `callModel`
 * invocation, DEPENDENCY-INJECTED rather than importing `attemptStream`
 * from `turnEngine.ts` directly (this module has no cycle risk with
 * `turnEngine.ts` the way `riskJudge.ts` does, but staying DI'd keeps this
 * module trivially unit-testable with a fake `callModel` and keeps the
 * actual `resolveTarget`/`attemptStream` wiring — which needs the real
 * Tauri runtime — confined to the one caller that has it,
 * `specScorerStore.ts`, exactly like `contextTrimmer.ts`'s `sendForSummary`
 * option is wired by `agentLoop.ts`), the same strict fail-closed JSON
 * parse (anything malformed returns `null`, never a fabricated score), and
 * the same race-against-a-timeout shape.
 *
 * This is advisory only: nothing in this module or its caller ever blocks
 * starting an Issue-to-PR run. It only surfaces a warning banner with
 * concrete questions to answer first — see `IssueToPrPanel.tsx`.
 */
import type { ChatMessage } from './llamaClient';

/** The six dimensions the roadmap item names verbatim, in the fixed order
 * the rubric prompt and the panel's breakdown list both render in. */
export const SPEC_DIMENSIONS = [
  'clarity',
  'scope',
  'missingContext',
  'testability',
  'dependencies',
  'agentReadiness',
] as const;

export type SpecDimension = (typeof SPEC_DIMENSIONS)[number];

export interface SpecScore {
  /** 0-100, the rounded average of the six dimension scores below — computed
   * deterministically by this module rather than trusted from the model's
   * own arithmetic, so it can never drift from the dimensions actually
   * shown. */
  overall: number;
  dimensions: Record<SpecDimension, number>;
  /** Concrete, specific, answerable questions — never generic advice like
   * "add more detail". Empty when the model found nothing worth asking. */
  missingInfo: string[];
  /** One-sentence verdict, shown alongside the score. */
  summary: string;
}

export interface SpecScorerCallResult {
  content: string;
  streamError: string | null;
}

/** Hard timeout for the one-shot scoring call — a slow/hung local model must
 * never stall the panel indefinitely; the caller just shows no banner if
 * this fires (fails closed, never fabricates a score). */
export const SPEC_SCORER_TIMEOUT_MS = 15000;

/** Below this `overall` score, `isSpecTooVague` reports the issue as too
 * vague for an autonomous run to start against unattended. */
export const SPEC_SCORE_WARN_THRESHOLD = 60;

/** Cap on how much of the issue body is inlined into the rubric prompt, so
 * one huge issue description can't blow up the request. */
const MAX_SPEC_CHARS = 6000;

/** Cap on how many missing-info questions are kept from the model's reply —
 * generous for a genuinely underspecified issue, but bounded so a model
 * that ignores the prompt's intent can't flood the banner. */
const MAX_MISSING_INFO_ITEMS = 10;

function truncate(text: string, max: number): string {
  return text.length > max ? `${text.slice(0, max)}…` : text;
}

function buildScorerMessages(issueTitle: string, issueBody: string): ChatMessage[] {
  const body = truncate(issueBody.trim() || '(no description provided)', MAX_SPEC_CHARS);
  return [
    {
      role: 'system',
      content: [
        'You are an expert reviewer judging whether a GitHub issue/spec is ready for an autonomous AI coding agent to implement END-TO-END, unattended, with no human available to answer follow-up questions.',
        'Score these six dimensions from 0 (severely lacking) to 100 (excellent), as integers:',
        '- clarity: are the requirements unambiguous and concrete?',
        '- scope: is the work clearly bounded, not sprawling or open-ended?',
        '- missingContext: is all context an implementer would need present (background, constraints, relevant files/areas)? 100 = nothing missing.',
        '- testability: is it clear how to verify the result is correct (acceptance criteria, expected behavior, examples)?',
        '- dependencies: are external dependencies, APIs, schemas, or prerequisite work clearly identified (or clearly absent)?',
        '- agentReadiness: overall, could an autonomous agent start implementing this right now without asking a clarifying question first?',
        'Also list concrete missing-information questions: specific, answerable questions that — if answered — would most improve agent-readiness. Never generic advice like "add more detail". Use an empty array if genuinely nothing is missing.',
        'Reply with ONLY a single-line JSON object of the exact shape ' +
          '{"dimensions":{"clarity":0,"scope":0,"missingContext":0,"testability":0,"dependencies":0,"agentReadiness":0},"missingInfo":["..."],"summary":"one sentence verdict"} ' +
          '— no markdown, no other text.',
      ].join('\n'),
    },
    {
      role: 'user',
      content: `Issue title: ${issueTitle.trim() || '(no title)'}\n\nIssue body:\n${body}`,
    },
  ];
}

function clampScore(value: unknown): number | null {
  if (typeof value !== 'number' || !Number.isFinite(value)) return null;
  return Math.max(0, Math.min(100, Math.round(value)));
}

/**
 * Strict parse of the scorer's reply: anything that isn't exactly
 * `{dimensions: {<all six keys>: number}, missingInfo: string[], summary:
 * string}` — extra prose, unparseable JSON, a missing dimension — returns
 * `null`. Fails closed exactly like `riskJudge.ts`'s `parseJudgeResponse`:
 * `null` means "no score available", never a fabricated/partial one. Tries
 * the raw trimmed content first, then falls back to the first `{...}` span
 * found in it (small local models sometimes wrap otherwise valid JSON in a
 * sentence or code fence).
 */
export function parseSpecScoreResponse(content: string): SpecScore | null {
  const candidates = [content.trim()];
  const embedded = content.match(/\{[\s\S]*\}/);
  if (embedded) candidates.push(embedded[0]);

  for (const candidate of candidates) {
    let parsed: unknown;
    try {
      parsed = JSON.parse(candidate);
    } catch {
      continue;
    }
    if (!parsed || typeof parsed !== 'object') continue;

    const rawDimensions = (parsed as { dimensions?: unknown }).dimensions;
    if (!rawDimensions || typeof rawDimensions !== 'object') continue;

    const dimensions = {} as Record<SpecDimension, number>;
    let allDimensionsValid = true;
    for (const dim of SPEC_DIMENSIONS) {
      const score = clampScore((rawDimensions as Record<string, unknown>)[dim]);
      if (score === null) {
        allDimensionsValid = false;
        break;
      }
      dimensions[dim] = score;
    }
    if (!allDimensionsValid) continue;

    const rawMissingInfo = (parsed as { missingInfo?: unknown }).missingInfo;
    const missingInfo = Array.isArray(rawMissingInfo)
      ? rawMissingInfo
          .filter((item): item is string => typeof item === 'string' && item.trim().length > 0)
          .map((item) => item.trim())
          .slice(0, MAX_MISSING_INFO_ITEMS)
      : [];

    const rawSummary = (parsed as { summary?: unknown }).summary;
    const summary = typeof rawSummary === 'string' ? rawSummary.trim() : '';

    const overall = Math.round(
      SPEC_DIMENSIONS.reduce((sum, dim) => sum + dimensions[dim], 0) / SPEC_DIMENSIONS.length,
    );

    return { overall, dimensions, missingInfo, summary };
  }
  return null;
}

/** True when the issue scored below `SPEC_SCORE_WARN_THRESHOLD` overall —
 * the one signal `IssueToPrPanel.tsx` uses to decide whether to show the
 * warning banner. Purely advisory: nothing reads this to block a run. */
export function isSpecTooVague(score: SpecScore): boolean {
  return score.overall < SPEC_SCORE_WARN_THRESHOLD;
}

/**
 * Scores an issue/spec via one one-shot, non-streaming, tool-less
 * `callModel` invocation. Fails closed on anything malformed, errored, or
 * slower than `SPEC_SCORER_TIMEOUT_MS`: every one of those cases resolves
 * `null`, never a fabricated score — the caller simply shows no banner.
 *
 * `signal` (e.g. the panel's own unmount/selection-change signal, if any) is
 * raced against this call's own timeout, exactly like
 * `riskJudge.ts`'s `classifyToolCall`.
 */
export async function scoreSpec(
  issueTitle: string,
  issueBody: string,
  callModel: (messages: ChatMessage[], signal: AbortSignal) => Promise<SpecScorerCallResult>,
  signal?: AbortSignal,
): Promise<SpecScore | null> {
  const timeoutController = new AbortController();
  const timeoutId = setTimeout(() => timeoutController.abort(), SPEC_SCORER_TIMEOUT_MS);
  const onParentAbort = () => timeoutController.abort();
  if (signal) {
    if (signal.aborted) timeoutController.abort();
    else signal.addEventListener('abort', onParentAbort, { once: true });
  }

  try {
    const result = await callModel(buildScorerMessages(issueTitle, issueBody), timeoutController.signal);
    if (result.streamError) return null;
    return parseSpecScoreResponse(result.content);
  } catch {
    return null;
  } finally {
    clearTimeout(timeoutId);
    signal?.removeEventListener('abort', onParentAbort);
  }
}
