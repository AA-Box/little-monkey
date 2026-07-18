/**
 * Synthetic Data and Golden Dataset Builder (ROADMAP.md Phase 7, item 30):
 * generates labeled synthetic examples from a seed description via the
 * existing local-model-call pattern, lets a user mix in imported "real"
 * examples, and keeps every example individually traceable — provenance
 * (synthetic + the exact generation prompt, or imported + a source label),
 * a privacy-filter verdict, a duplicate verdict, and the dataset version it
 * was added/last touched in. Nothing here silently drops an example: a
 * real/imported example that fails the privacy filter is flagged and
 * excluded (`included: false`, `exclusionReason: "privacy"`), never quietly
 * folded into the dataset next to synthetic data.
 *
 * Structurally mirrors `sopCompiler.ts`'s dependency-injected `callModel`
 * shape (not `riskJudge.ts`'s advisory one — an empty/failed generation has
 * no safe fallback, so it throws instead): the real model round trip
 * (`agentLoop.ts`'s `resolveTarget` + `turnEngine.ts`'s `attemptStream`) is
 * threaded in by `goldenDatasetBuilderStore.ts` rather than imported here,
 * so this module carries no Tauri/zustand/React dependency and is
 * unit-testable with a fake `callModel`.
 *
 * Deduplication and privacy filtering are both deliberately simple,
 * dependency-free heuristics (normalized-text hashing + word-shingle
 * Jaccard similarity for near-duplicates; PII regexes) — no new ML
 * dependency, per the roadmap item's own scope. A more sophisticated
 * embedding-based near-duplicate detector or a smarter PII model are
 * reasonable follow-ups, not part of this MVP.
 */
import type { ChatMessage } from './llamaClient';

export type ExampleProvenance =
  | { kind: 'synthetic'; generationPrompt: string }
  | { kind: 'imported'; source: string };

export type PrivacyFindingType = 'email' | 'phone' | 'ssn' | 'creditCard';

export interface PrivacyFinding {
  type: PrivacyFindingType;
  count: number;
}

export interface PrivacyFilterResult {
  passed: boolean;
  findings: PrivacyFinding[];
}

export type DuplicateKind = 'none' | 'exact' | 'near';

export type ExclusionReason = 'privacy' | 'duplicate' | null;

/** One example in a golden dataset, fully traceable back to how it got
 * here: its provenance, the privacy filter's verdict, the duplicate check's
 * verdict, and the dataset version it was introduced/last touched in. */
export interface DatasetExample {
  id: string;
  fields: Record<string, string>;
  provenance: ExampleProvenance;
  privacy: PrivacyFilterResult;
  duplicateKind: DuplicateKind;
  /** id of the earlier example this one duplicates, if any. */
  duplicateOfId: string | null;
  /** False when excluded by the privacy filter or as an exact duplicate —
   * an excluded example is KEPT in the dataset (for audit) but never counted
   * toward eval/export. */
  included: boolean;
  exclusionReason: ExclusionReason;
  version: number;
  createdAt: number;
}

/** One entry in a dataset's version history — recorded every time the
 * dataset is edited (generated into, imported into, or an example removed). */
export interface DatasetVersionEntry {
  version: number;
  createdAt: number;
  note: string;
  exampleCount: number;
}

/** One recorded eval run against a dataset at a specific version — see
 * `runSchemaConformanceEval` for what it actually checks. */
export interface EvalRunResult {
  id: string;
  version: number;
  createdAt: number;
  passed: number;
  total: number;
  summary: string;
}

export interface GoldenDataset {
  id: string;
  name: string;
  seedDescription: string;
  /** Schema field names, in order — the first is treated as the primary
   * content field, the rest as labels/metadata, but all are generated,
   * imported, dedupe-compared, and privacy-scanned identically. */
  fields: string[];
  examples: DatasetExample[];
  versions: DatasetVersionEntry[];
  currentVersion: number;
  evalRuns: EvalRunResult[];
  createdAt: number;
  updatedAt: number;
  lastError: string | null;
}

export interface ModelCallResult {
  content: string;
  streamError: string | null;
}

export const MIN_GENERATED_EXAMPLES = 1;
export const MAX_GENERATED_EXAMPLES = 50;
export const MAX_SEED_DESCRIPTION_CHARS = 2_000;
export const MAX_FIELDS = 8;
export const GENERATION_TIMEOUT_MS = 60_000;
/** Word-shingle Jaccard similarity at or above this is treated as a
 * near-duplicate (an exact normalized-text match is always caught first). */
export const NEAR_DUPLICATE_THRESHOLD = 0.82;

// ---------------------------------------------------------------------------
// Schema fields
// ---------------------------------------------------------------------------

/** Parses a comma-separated field list into a deduplicated, capped, trimmed
 * field-name array — the schema for one dataset (e.g. "text, category"). */
export function parseFieldsInput(raw: string): string[] {
  const seen = new Set<string>();
  const fields: string[] = [];
  for (const part of raw.split(',')) {
    const trimmed = part.trim();
    if (!trimmed) continue;
    const key = trimmed.toLowerCase();
    if (seen.has(key)) continue;
    seen.add(key);
    fields.push(trimmed);
    if (fields.length >= MAX_FIELDS) break;
  }
  return fields;
}

// ---------------------------------------------------------------------------
// Privacy filter — simple PII regex heuristics, no ML dependency.
// ---------------------------------------------------------------------------

const EMAIL_RE = /[a-zA-Z0-9._%+-]+@[a-zA-Z0-9.-]+\.[a-zA-Z]{2,}/g;
const PHONE_RE = /(?:\+?\d{1,2}[\s.-]?)?\(?\d{3}\)?[\s.-]\d{3}[\s.-]\d{4}\b/g;
const SSN_RE = /\b\d{3}-\d{2}-\d{4}\b/g;
// 13-16 digits, optionally grouped by spaces/hyphens — a loose credit-card-like
// pattern (deliberately not Luhn-validated: a heuristic filter should err
// toward flagging, not toward confidently letting real-looking numbers through).
const CREDIT_CARD_RE = /\b(?:\d[ -]?){12,15}\d\b/g;

function countMatches(re: RegExp, text: string): number {
  const matches = text.match(re);
  return matches ? matches.length : 0;
}

/** Runs every PII heuristic against one blob of text. Used both standalone
 * (tests) and via `privacyFilterFields` for a whole example's field values. */
export function runPrivacyFilter(text: string): PrivacyFilterResult {
  const findings: PrivacyFinding[] = [];
  const email = countMatches(EMAIL_RE, text);
  if (email > 0) findings.push({ type: 'email', count: email });
  const phone = countMatches(PHONE_RE, text);
  if (phone > 0) findings.push({ type: 'phone', count: phone });
  const ssn = countMatches(SSN_RE, text);
  if (ssn > 0) findings.push({ type: 'ssn', count: ssn });
  const creditCard = countMatches(CREDIT_CARD_RE, text);
  if (creditCard > 0) findings.push({ type: 'creditCard', count: creditCard });
  return { passed: findings.length === 0, findings };
}

export function fieldsText(fields: Record<string, string>): string {
  return Object.values(fields).join(' \n ').trim();
}

export function privacyFilterFields(fields: Record<string, string>): PrivacyFilterResult {
  return runPrivacyFilter(fieldsText(fields));
}

// ---------------------------------------------------------------------------
// Deduplication — exact via normalized-text fingerprint, near via
// word-shingle Jaccard similarity. No new ML dependency.
// ---------------------------------------------------------------------------

function normalizeForDedupe(text: string): string {
  return text
    .toLowerCase()
    .replace(/[^\p{L}\p{N}\s]/gu, '')
    .replace(/\s+/g, ' ')
    .trim();
}

/** Small deterministic string hash (djb2) — a compact fingerprint for exact
 * normalized-text matches, not a security hash. */
export function djb2Hash(text: string): string {
  let hash = 5381;
  for (let i = 0; i < text.length; i++) {
    hash = ((hash << 5) + hash + text.charCodeAt(i)) | 0;
  }
  return (hash >>> 0).toString(16);
}

export function exampleFingerprint(fields: Record<string, string>): string {
  return djb2Hash(normalizeForDedupe(fieldsText(fields)));
}

function shingleSet(text: string): Set<string> {
  return new Set(normalizeForDedupe(text).split(' ').filter(Boolean));
}

/** Jaccard similarity of the two texts' word sets — 1 for identical word
 * sets, 0 for disjoint ones. Cheap, dependency-free stand-in for a real
 * embedding-similarity near-duplicate detector. */
export function jaccardSimilarity(a: string, b: string): number {
  const setA = shingleSet(a);
  const setB = shingleSet(b);
  if (setA.size === 0 && setB.size === 0) return 1;
  if (setA.size === 0 || setB.size === 0) return 0;
  let intersection = 0;
  for (const word of setA) if (setB.has(word)) intersection++;
  const union = setA.size + setB.size - intersection;
  return union === 0 ? 0 : intersection / union;
}

export interface DedupeInput {
  id: string;
  fields: Record<string, string>;
}

export interface DedupeResult {
  id: string;
  duplicateKind: DuplicateKind;
  duplicateOfId: string | null;
}

/** Scans a list of examples IN ORDER, marking each one as `exact`/`near`/
 * `none` relative to whichever earlier example it matches — the earliest
 * occurrence of a group is always the "none" canonical entry. Runs in
 * O(n^2) worst case over the near-duplicate comparison, which is fine at the
 * dataset sizes this feature targets (tens to low hundreds of examples). */
export function detectDuplicates(examples: readonly DedupeInput[]): DedupeResult[] {
  const results: DedupeResult[] = [];
  const seenFingerprints = new Map<string, string>();
  const canonical: { id: string; text: string }[] = [];

  for (const example of examples) {
    const text = fieldsText(example.fields);
    const fingerprint = exampleFingerprint(example.fields);
    const exactMatch = seenFingerprints.get(fingerprint);
    if (exactMatch) {
      results.push({ id: example.id, duplicateKind: 'exact', duplicateOfId: exactMatch });
      continue;
    }
    const nearMatch = canonical.find((entry) => jaccardSimilarity(text, entry.text) >= NEAR_DUPLICATE_THRESHOLD);
    if (nearMatch) {
      results.push({ id: example.id, duplicateKind: 'near', duplicateOfId: nearMatch.id });
    } else {
      results.push({ id: example.id, duplicateKind: 'none', duplicateOfId: null });
      seenFingerprints.set(fingerprint, example.id);
      canonical.push({ id: example.id, text });
    }
  }
  return results;
}

/** Re-runs duplicate detection over a whole example list and folds the
 * result back into each example's `duplicateKind`/`duplicateOfId`/
 * `included`/`exclusionReason` — the single place inclusion is decided, so a
 * privacy failure and an exact-duplicate flag can never both silently lose
 * to the other. Order matters: pass examples oldest-first (e.g. sorted by
 * `createdAt`) so the earliest copy is always the surviving canonical one. */
export function recomputeDuplicates(examples: readonly DatasetExample[]): DatasetExample[] {
  const dedupeResults = detectDuplicates(examples.map((example) => ({ id: example.id, fields: example.fields })));
  const resultById = new Map(dedupeResults.map((entry) => [entry.id, entry]));
  return examples.map((example) => {
    const dedupe = resultById.get(example.id);
    if (!dedupe) return example;
    const isExactDuplicate = dedupe.duplicateKind === 'exact';
    const included = example.privacy.passed && !isExactDuplicate;
    const exclusionReason: ExclusionReason = !example.privacy.passed ? 'privacy' : isExactDuplicate ? 'duplicate' : null;
    return {
      ...example,
      duplicateKind: dedupe.duplicateKind,
      duplicateOfId: dedupe.duplicateOfId,
      included,
      exclusionReason,
    };
  });
}

// ---------------------------------------------------------------------------
// Example materialization
// ---------------------------------------------------------------------------

/** Turns one generated/imported field record into a full, storable
 * `DatasetExample` — runs the privacy filter immediately (duplicate
 * detection happens afterward, across the whole dataset, via
 * `recomputeDuplicates`). `idFactory` is overridable purely for deterministic
 * tests. */
export function materializeExample(
  fields: Record<string, string>,
  provenance: ExampleProvenance,
  version: number,
  idFactory: () => string = () => crypto.randomUUID(),
): DatasetExample {
  const privacy = privacyFilterFields(fields);
  return {
    id: idFactory(),
    fields,
    provenance,
    privacy,
    duplicateKind: 'none',
    duplicateOfId: null,
    included: privacy.passed,
    exclusionReason: privacy.passed ? null : 'privacy',
    version,
    createdAt: Date.now(),
  };
}

export function newDatasetId(): string {
  return crypto.randomUUID();
}

export function newVersionEntry(version: number, note: string, exampleCount: number): DatasetVersionEntry {
  return { version, createdAt: Date.now(), note, exampleCount };
}

// ---------------------------------------------------------------------------
// Synthetic generation via a dependency-injected model call.
// ---------------------------------------------------------------------------

/** Builds the one-shot generation prompt. The returned `prompt` is the exact
 * human-readable instruction stored as each resulting example's
 * `provenance.generationPrompt` — the traceability the roadmap item asks for. */
export function buildGenerationMessages(
  seedDescription: string,
  fields: readonly string[],
  count: number,
): { messages: ChatMessage[]; prompt: string } {
  const trimmedSeed = seedDescription.trim().slice(0, MAX_SEED_DESCRIPTION_CHARS);
  const fieldList = fields.join(', ');
  const prompt = `Generate ${count} example(s) for: "${trimmedSeed}" with fields [${fieldList}]`;
  const systemPrompt = [
    'You generate synthetic labeled examples for a golden evaluation/fine-tuning/RAG-test dataset.',
    `Generate EXACTLY ${count} distinct, realistic examples for the following seed description.`,
    `Each example must be a JSON object with EXACTLY these fields, no more and no fewer: ${fieldList}.`,
    'Make every example meaningfully different from the others in wording, scenario, and label — never repeat the same example twice.',
    "Never include any real person's name, email address, phone number, government ID number, or payment card number — invent clearly fictional placeholder details instead.",
    'Reply with ONLY a single-line JSON object of the exact shape ' +
      `{"examples":[{${fields.map((field) => `"${field}":"..."`).join(',')}}]} — no markdown, no other text.`,
  ].join(' ');
  const messages: ChatMessage[] = [
    { role: 'system', content: systemPrompt },
    { role: 'user', content: `Seed description: ${trimmedSeed}\nFields: ${fieldList}\nCount: ${count}` },
  ];
  return { messages, prompt };
}

/** Strict parse of the generation model's reply: tries the raw trimmed
 * content first, then the first `{...}` span (small local models sometimes
 * wrap otherwise valid JSON in a sentence or code fence) — mirrors
 * `sopCompiler.ts`'s `parseSopCompilerResponse`. Drops any item missing a
 * non-empty string for one of the required fields rather than fabricating
 * one; caps at `count`. */
export function parseGenerationResponse(content: string, fields: readonly string[], count: number): Record<string, string>[] {
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
    const examplesRaw = (parsed as { examples?: unknown } | null)?.examples;
    if (!Array.isArray(examplesRaw)) continue;

    const results: Record<string, string>[] = [];
    for (const rawItem of examplesRaw.slice(0, count)) {
      if (!rawItem || typeof rawItem !== 'object') continue;
      const item = rawItem as Record<string, unknown>;
      const record: Record<string, string> = {};
      let valid = true;
      for (const field of fields) {
        const value = item[field];
        if (typeof value !== 'string' || !value.trim()) {
          valid = false;
          break;
        }
        record[field] = value.trim();
      }
      if (valid) results.push(record);
    }
    if (results.length > 0) return results;
  }
  return [];
}

/**
 * Runs the one-shot synthetic-generation call via a dependency-injected
 * `callModel` (see this module's doc comment for why it's injected rather
 * than imported). Fails loudly (throws) rather than silently — an
 * empty/failed generation has no safe fallback value to hand back to the
 * dataset.
 */
export async function generateSyntheticExamples(
  seedDescription: string,
  fields: readonly string[],
  count: number,
  callModel: (messages: ChatMessage[], signal: AbortSignal) => Promise<ModelCallResult>,
  signal?: AbortSignal,
): Promise<{ examples: Record<string, string>[]; prompt: string }> {
  const trimmedSeed = seedDescription.trim();
  if (!trimmedSeed) throw new Error('Describe what this dataset should contain before generating examples.');
  if (fields.length === 0) throw new Error('Add at least one schema field before generating examples.');
  const clampedCount = Math.max(MIN_GENERATED_EXAMPLES, Math.min(MAX_GENERATED_EXAMPLES, Math.round(count) || MIN_GENERATED_EXAMPLES));
  const { messages, prompt } = buildGenerationMessages(trimmedSeed, fields, clampedCount);

  const timeoutController = new AbortController();
  const timeoutId = setTimeout(() => timeoutController.abort(), GENERATION_TIMEOUT_MS);
  const onParentAbort = () => timeoutController.abort();
  if (signal) {
    if (signal.aborted) timeoutController.abort();
    else signal.addEventListener('abort', onParentAbort, { once: true });
  }

  try {
    const result = await callModel(messages, timeoutController.signal);
    if (result.streamError) throw new Error(result.streamError);
    const examples = parseGenerationResponse(result.content, fields, clampedCount);
    if (examples.length === 0) {
      throw new Error('The model did not return any usable examples — try a clearer seed description, or fewer/simpler fields.');
    }
    return { examples, prompt };
  } finally {
    clearTimeout(timeoutId);
    signal?.removeEventListener('abort', onParentAbort);
  }
}

// ---------------------------------------------------------------------------
// Importing real examples — same privacy filter, never silently included.
// ---------------------------------------------------------------------------

export interface ImportParseResult {
  examples: Record<string, string>[];
  skippedLines: number;
}

/** Parses pasted "real" examples against the dataset's schema fields. Tries
 * a JSON array of `{field: value, ...}` objects first; falls back to one
 * example per line, `|`-delimited in field order. Rows that don't cleanly
 * match the schema are skipped (counted, never guessed at) rather than
 * silently padded with empty values. Privacy filtering happens afterward in
 * `materializeExample`, exactly like it does for synthetic examples. */
export function parseImportedExamples(rawText: string, fields: readonly string[]): ImportParseResult {
  const trimmed = rawText.trim();
  if (!trimmed || fields.length === 0) return { examples: [], skippedLines: 0 };

  try {
    const parsed = JSON.parse(trimmed);
    if (Array.isArray(parsed)) {
      const examples: Record<string, string>[] = [];
      let skipped = 0;
      for (const item of parsed) {
        if (!item || typeof item !== 'object') {
          skipped++;
          continue;
        }
        const record: Record<string, string> = {};
        let valid = true;
        for (const field of fields) {
          const value = (item as Record<string, unknown>)[field];
          if (typeof value !== 'string' || !value.trim()) {
            valid = false;
            break;
          }
          record[field] = value.trim();
        }
        if (valid) examples.push(record);
        else skipped++;
      }
      return { examples, skippedLines: skipped };
    }
  } catch {
    // Not JSON — fall through to line-based parsing.
  }

  const lines = trimmed
    .split('\n')
    .map((line) => line.trim())
    .filter((line) => line.length > 0);
  const examples: Record<string, string>[] = [];
  let skipped = 0;
  for (const line of lines) {
    const parts = line.split('|').map((part) => part.trim());
    if (parts.length !== fields.length || parts.some((part) => !part)) {
      skipped++;
      continue;
    }
    const record: Record<string, string> = {};
    fields.forEach((field, index) => {
      record[field] = parts[index];
    });
    examples.push(record);
  }
  return { examples, skippedLines: skipped };
}

// ---------------------------------------------------------------------------
// Eval — a real, simple, honest check derived from the dataset's actual
// current state (not a hardcoded/fabricated result). A fuller eval harness
// (running the dataset against an actual RAG/workflow/fine-tune target) is a
// follow-up; this MVP checks the one thing every downstream consumer of a
// golden dataset needs true regardless of what it's used for: every INCLUDED
// example actually has every schema field populated.
// ---------------------------------------------------------------------------

export function runSchemaConformanceEval(dataset: Pick<GoldenDataset, 'examples' | 'fields' | 'currentVersion'>): EvalRunResult {
  const included = dataset.examples.filter((example) => example.included);
  let passed = 0;
  for (const example of included) {
    const complete = dataset.fields.every((field) => (example.fields[field] ?? '').trim().length > 0);
    if (complete) passed++;
  }
  const excludedCount = dataset.examples.length - included.length;
  const summary =
    `${passed}/${included.length} included example(s) have every schema field populated` +
    (excludedCount > 0 ? ` (${excludedCount} excluded by privacy/duplicate filters).` : '.');
  return {
    id: crypto.randomUUID(),
    version: dataset.currentVersion,
    createdAt: Date.now(),
    passed,
    total: included.length,
    summary,
  };
}
