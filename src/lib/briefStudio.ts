/**
 * Source-Grounded Brief Studio (ROADMAP.md Phase 7, item 7): turns selected
 * source material — a pasted document, a chat session, or a Knowledge 2.0
 * stack query — into one of several TEXT study assets (executive brief,
 * slide outline, quiz, flashcards, study guide), with every factual claim
 * required to carry an inline citation back to a numbered source block.
 *
 * Audio overviews and video outlines (also named in the roadmap line item)
 * are deliberately NOT generated here — this app has no TTS/video pipeline
 * to back them (see `UNSUPPORTED_ASSET_TYPES`'s doc comment). Requesting one
 * returns a clearly-labeled "not supported yet" result instead of a faked
 * asset; `briefStudioStore.ts`/`BriefStudioPanel.tsx` surface that as-is.
 *
 * Model calls reuse the exact same one-shot completion primitives
 * `agentLoop.ts`'s `compactSessionNow` already uses for its own one-shot
 * summarization call: `resolveTarget()` (`agentLoop.ts`) picks whichever
 * model/provider the app is currently pointed at, and `attemptStream()`
 * (`turnEngine.ts`) drives a single non-tool completion against it. No new
 * Rust command, no new network primitive.
 *
 * Grounding verification: this app has no existing "verify a quoted span is
 * a real substring of its source" utility to reuse (checked — nothing in
 * `riskJudge.ts`/`untrustedContent.ts`/`knowledgeV2Store.ts` does this), so
 * `verifyCitations` below implements a simple, self-contained one: every
 * `[S<n>: "quote"]` marker in the model's output is checked against the
 * exact text of source block `S<n>`, case- and whitespace-insensitively.
 * Verification is advisory (surfaced to the user, never silently strips
 * unverified text) — a model that ignores instructions and fabricates a
 * quote still gets flagged, not hidden.
 */
import { resolveTarget } from './agentLoop';
import { attemptStream, type ResolvedTarget } from './turnEngine';
import { textContent, type ChatMessage } from './llamaClient';

// ---------------------------------------------------------------------------
// Source material
// ---------------------------------------------------------------------------

/** One numbered, citable unit of source text. `refId` is what the model is
 * asked to cite back (`S1`, `S2`, ...) and what `verifyCitations` looks the
 * quote up against. */
export interface SourceBlock {
  refId: string;
  /** Short human-readable provenance shown next to the block in the source
   * prompt and in the panel's citation list — a chat turn label, a knowledge
   * chunk's document URI, or "Pasted document". Never itself treated as
   * citable text. */
  label: string;
  text: string;
}

export type BriefSourceKind = 'pasted' | 'session' | 'knowledge_stack';

export interface BriefSourceInput {
  kind: BriefSourceKind;
  /** What the generated asset's header shows as "grounded in" — the pasted
   * document's title, the chat session's title, or the knowledge stack's name. */
  label: string;
  blocks: SourceBlock[];
}

/** One raw (label, text) pair before truncation/numbering — what a caller
 * builds from its own data (chat messages, a knowledge query response, a
 * pasted textarea) before handing it to `normalizeSourceBlocks`. */
export interface RawSourceBlock {
  label: string;
  text: string;
}

/** Per-block cap: a single oversized source (one giant pasted document, one
 * huge chat turn) can't dominate the prompt or blow the model's context on
 * its own — mirrors `mentions.ts`'s `MAX_MENTION_CONTENT_CHARS` precedent for
 * capping one referenced thing's contribution to a prompt. */
export const MAX_BLOCK_CHARS = 3_000;
/** Overall cap across every block combined. */
export const MAX_TOTAL_SOURCE_CHARS = 20_000;
/** Hard cap on block count — keeps the numbered-citation prompt scannable
 * (and the model's own citation bookkeeping tractable) even if a caller hands
 * over dozens of knowledge-stack hits or a very long chat session. */
export const MAX_BLOCKS = 16;

const TRUNCATION_SUFFIX = '… [truncated]';

/** Turns raw (label, text) pairs into numbered, size-capped `SourceBlock`s.
 * Blank blocks are dropped; blocks beyond `MAX_BLOCKS` or the char budget are
 * dropped/truncated rather than erroring — a caller that overshoots gets a
 * smaller-but-usable source set instead of a failure. */
export function normalizeSourceBlocks(raw: readonly RawSourceBlock[]): SourceBlock[] {
  const blocks: SourceBlock[] = [];
  let totalChars = 0;

  for (const item of raw) {
    if (blocks.length >= MAX_BLOCKS) break;
    const trimmed = item.text.trim();
    if (!trimmed) continue;

    let text = trimmed.length > MAX_BLOCK_CHARS ? `${trimmed.slice(0, MAX_BLOCK_CHARS)}${TRUNCATION_SUFFIX}` : trimmed;
    const remaining = MAX_TOTAL_SOURCE_CHARS - totalChars;
    if (remaining <= TRUNCATION_SUFFIX.length) break;
    if (text.length > remaining) {
      text = `${text.slice(0, remaining - TRUNCATION_SUFFIX.length)}${TRUNCATION_SUFFIX}`;
    }

    totalChars += text.length;
    blocks.push({ refId: `S${blocks.length + 1}`, label: item.label, text });
    if (totalChars >= MAX_TOTAL_SOURCE_CHARS) break;
  }

  return blocks;
}

/** Builds a `BriefSourceInput` from a single pasted document. */
export function buildPastedSource(label: string, text: string): BriefSourceInput {
  return {
    kind: 'pasted',
    label: label.trim() || 'Pasted document',
    blocks: normalizeSourceBlocks([{ label: 'Pasted document', text }]),
  };
}

/** Builds a `BriefSourceInput` from an already-loaded chat session's
 * messages — one block per user/assistant turn (system/tool messages carry
 * no user-authored material worth briefing on, so they're skipped). Takes
 * plain `ChatMessage[]` rather than a session id so this stays pure/testable
 * without mocking `sessionStore`/Tauri — the caller (`briefStudioStore.ts`)
 * reads `sessionMessages(sessionId)` itself and passes the array in. */
export function buildSessionSource(sessionLabel: string, messages: readonly ChatMessage[]): BriefSourceInput {
  const raw: RawSourceBlock[] = messages
    .filter((message) => message.role === 'user' || message.role === 'assistant')
    .map((message, index) => ({
      label: `Turn ${index + 1} (${message.role})`,
      text: textContent(message.content),
    }));
  return {
    kind: 'session',
    label: sessionLabel.trim() || 'Chat session',
    blocks: normalizeSourceBlocks(raw),
  };
}

/** Builds a `BriefSourceInput` from a knowledge stack's retrieval hits — the
 * caller (`briefStudioStore.ts`) runs `knowledgeV2Store.query()` itself and
 * maps each hit's `{ chunk.text, citation.canonical_uri, heading_path }`
 * into a `RawSourceBlock` before calling this, so this stays store-free. */
export function buildKnowledgeStackSource(stackLabel: string, hits: readonly RawSourceBlock[]): BriefSourceInput {
  return {
    kind: 'knowledge_stack',
    label: stackLabel.trim() || 'Knowledge stack',
    blocks: normalizeSourceBlocks(hits),
  };
}

// ---------------------------------------------------------------------------
// Asset types
// ---------------------------------------------------------------------------

export type BriefAssetType =
  | 'brief'
  | 'slide_outline'
  | 'quiz'
  | 'flashcards'
  | 'study_guide'
  | 'audio_overview'
  | 'video_outline';

/** The five text-only assets this MVP actually generates. */
export const TEXT_ASSET_TYPES: readonly BriefAssetType[] = ['brief', 'slide_outline', 'quiz', 'flashcards', 'study_guide'];

/** Named in the roadmap line item, but out of scope for this MVP — this app
 * has no TTS engine and no video-generation pipeline to back either one.
 * `generateBriefAsset` short-circuits for these without ever calling a
 * model, and the panel shows a plain "not supported yet" note rather than a
 * disabled-looking button that silently does nothing. */
export const UNSUPPORTED_ASSET_TYPES: readonly BriefAssetType[] = ['audio_overview', 'video_outline'];

const ASSET_INSTRUCTIONS: Record<Exclude<BriefAssetType, 'audio_overview' | 'video_outline'>, string> = {
  brief:
    'Write a concise executive brief in Markdown with these headings, in order: "## Overview", "## Key Findings", "## Risks & Open Questions", "## Recommended Next Steps". Use short paragraphs or bullet points. 200-500 words total.',
  slide_outline:
    'Produce a slide-by-slide outline in Markdown: 6-10 slides, each starting with "## Slide N: <title>" followed by 2-4 short bullet points of what that slide should say. Do not write full paragraphs — bullets only.',
  quiz:
    'Produce a 6-10 question multiple-choice quiz in Markdown. For each question: "**Q<n>.** <question>", then four lettered options on their own lines ("A. ...", "B. ...", "C. ...", "D. ..."), then "**Answer:** <letter>". Every question must be answerable from the sources alone.',
  flashcards:
    'Produce 8-15 flashcards in Markdown. For each: "**Card N**" on its own line, then "Front: <short prompt>" and "Back: <short answer>" on the next two lines.',
  study_guide:
    'Produce a structured study guide in Markdown: a "## " heading per major topic covered by the sources, with bullet points for key facts/definitions/terms under each, and a final "## Summary" section of 3-5 bullets.',
};

const ASSET_TITLES: Record<BriefAssetType, string> = {
  brief: 'executive brief',
  slide_outline: 'slide outline',
  quiz: 'quiz',
  flashcards: 'flashcards',
  study_guide: 'study guide',
  audio_overview: 'audio overview',
  video_outline: 'video outline',
};

const CITATION_INSTRUCTIONS =
  'You are a grounded study-material writer. You will be given numbered source blocks (S1, S2, ...), each with a short label describing where it came from. Use ONLY information present in these source blocks — never use outside knowledge, and never invent facts, numbers, or names not present in the sources. Every factual claim must be immediately followed by a citation marker in EXACTLY this form: [S<n>: "<short exact quote copied verbatim from that source block, a few words to one sentence>"]. The quoted text inside the brackets must be copied character-for-character from the referenced block — never paraphrase inside the quote. If the sources do not contain enough material for part of the requested format, say so plainly in that section instead of inventing content.';

// ---------------------------------------------------------------------------
// Citation verification
// ---------------------------------------------------------------------------

export interface BriefCitation {
  /** The raw marker text as it appeared in the model's output, e.g. `[S2: "revenue grew 12%"]`. */
  marker: string;
  refId: string;
  quote: string;
  /** True when `quote` is a verbatim (whitespace/case-insensitive) substring
   * of the referenced block's text — false if the block doesn't exist or the
   * quote can't be found in it. */
  verified: boolean;
  sourceLabel: string | null;
}

const CITATION_MARKER_RE = /\[(S\d+):\s*"([^"]*)"\]/g;

function normalizeForMatch(value: string): string {
  return value.replace(/\s+/g, ' ').trim().toLowerCase();
}

/** Extracts every `[S<n>: "quote"]` marker from `content` and checks each
 * quote against the exact text of the block it cites. Never mutates
 * `content` — verification is purely additive metadata for the UI to
 * display alongside the untouched generated text. */
export function verifyCitations(content: string, blocks: readonly SourceBlock[]): BriefCitation[] {
  const blockById = new Map(blocks.map((block) => [block.refId, block]));
  const citations: BriefCitation[] = [];
  for (const match of content.matchAll(CITATION_MARKER_RE)) {
    const refId = match[1];
    const quote = match[2].trim();
    const block = blockById.get(refId);
    const verified = Boolean(block && quote.length > 0 && normalizeForMatch(block.text).includes(normalizeForMatch(quote)));
    citations.push({ marker: match[0], refId, quote, verified, sourceLabel: block?.label ?? null });
  }
  return citations;
}

// ---------------------------------------------------------------------------
// Generation
// ---------------------------------------------------------------------------

/** Thrown by `generateBriefAsset` when the caller required a local-only run
 * (`options.requireLocalOnly`) but the app's currently active model target
 * is a cloud provider — the whole point of the check, so this must be thrown
 * BEFORE any `attemptStream` call, never after. */
export class BriefStudioPolicyError extends Error {
  constructor(message: string) {
    super(message);
    this.name = 'BriefStudioPolicyError';
  }
}

export interface GeneratedBriefAsset {
  assetType: BriefAssetType;
  sourceLabel: string;
  generatedAtMs: number;
  /** `null` for the two out-of-scope asset types below (no model was ever called). */
  targetKind: ResolvedTarget['kind'] | null;
  /** True when the model target that produced this asset was local/Ollama
   * (never left the machine) — the acceptance line's "can run fully local"
   * signal, shown as-is in the panel. Always false for an unsupported type. */
  ranLocally: boolean;
  supported: boolean;
  /** Set only when `supported` is false — why audio/video wasn't generated. */
  unsupportedReason: string | null;
  content: string;
  citations: BriefCitation[];
  unverifiedCitationCount: number;
}

function renderSourcePrompt(blocks: readonly SourceBlock[]): string {
  return blocks.map((block) => `[${block.refId}] (${block.label})\n${block.text}`).join('\n\n---\n\n');
}

export interface GenerateBriefAssetOptions {
  /** When true, refuse to generate against a cloud-provider model target —
   * the Brief Studio panel's "Run fully local" toggle. */
  requireLocalOnly: boolean;
  /** Usage-ledger key `attemptStream` records token usage under — kept
   * separate from any real chat session id so a brief generation never
   * clobbers a chat's own context-usage display (same reasoning as
   * `subagent.ts`'s `runSubagentTask`, which passes `recordUsage: false` for
   * the analogous reason; Brief Studio instead uses `recordUsage: false`
   * outright, since there is no session whose usage bar this should move). */
  usageKey?: string;
  signal?: AbortSignal;
}

export const BRIEF_STUDIO_USAGE_KEY = 'brief-studio';

/**
 * Generates one asset from `source`. Throws `BriefStudioPolicyError` (never
 * calls a model) if `options.requireLocalOnly` is set and the active target
 * resolves to a cloud provider. Returns a `supported: false` result without
 * calling a model at all for `audio_overview`/`video_outline`.
 */
export async function generateBriefAsset(
  source: BriefSourceInput,
  assetType: BriefAssetType,
  options: GenerateBriefAssetOptions,
): Promise<GeneratedBriefAsset> {
  const now = Date.now();

  if (UNSUPPORTED_ASSET_TYPES.includes(assetType)) {
    return {
      assetType,
      sourceLabel: source.label,
      generatedAtMs: now,
      targetKind: null,
      ranLocally: false,
      supported: false,
      unsupportedReason:
        `${ASSET_TITLES[assetType]} generation isn't available yet — this app has no local text-to-speech or video-rendering pipeline. Follow-up work, not faked here.`,
      content: '',
      citations: [],
      unverifiedCitationCount: 0,
    };
  }

  if (source.blocks.length === 0) {
    throw new Error('No source material to generate from — add a document, pick a knowledge stack, or select a chat session.');
  }

  const target = await resolveTarget();
  if (options.requireLocalOnly && target.kind === 'provider') {
    throw new BriefStudioPolicyError(
      '"Run fully local" is on for Brief Studio, but the active model is a cloud provider. Switch to a local llama.cpp or Ollama model, or turn off "Run fully local" to generate this asset.',
    );
  }

  const instructions = ASSET_INSTRUCTIONS[assetType as Exclude<BriefAssetType, 'audio_overview' | 'video_outline'>];
  const systemPrompt = `${CITATION_INSTRUCTIONS}\n\nFormat requested: ${instructions}`;
  const userPrompt = `${renderSourcePrompt(source.blocks)}\n\n---\n\nUsing ONLY the numbered sources above, produce the ${ASSET_TITLES[assetType]} described in your instructions.`;

  const messages: ChatMessage[] = [
    { role: 'system', content: systemPrompt },
    { role: 'user', content: userPrompt },
  ];

  // No `effort` override here — mirrors `sideTaskRunner.ts`'s own one-shot
  // `attemptStream` call (`undefined`, letting the provider's own default
  // apply) rather than pulling in `modelStore.ts`'s `effortForTarget` for a
  // minor per-request nicety this MVP doesn't need.
  const result = await attemptStream(
    target,
    messages,
    [],
    options.signal,
    undefined,
    options.usageKey ?? BRIEF_STUDIO_USAGE_KEY,
    undefined,
    false,
  );

  if (result.streamError) throw new Error(result.streamError);

  const content = result.content.trim();
  const citations = verifyCitations(content, source.blocks);

  return {
    assetType,
    sourceLabel: source.label,
    generatedAtMs: Date.now(),
    targetKind: target.kind,
    ranLocally: target.kind !== 'provider',
    supported: true,
    unsupportedReason: null,
    content,
    citations,
    unverifiedCitationCount: citations.filter((citation) => !citation.verified).length,
  };
}
