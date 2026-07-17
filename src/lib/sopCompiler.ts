/**
 * SOP-to-Agent Compiler (ROADMAP Phase 7, item 24) — turns a pasted or
 * imported SOP/runbook/checklist/training document into a DRAFT workflow
 * definition: declared inputs, policy/permission gates, a short
 * test/acceptance checklist, and required evidence fields, on top of the
 * exact same `SkillProposal` shape `skillProposalStore.ts` already exposes
 * (`command` + `instructions`, reviewed via its sha256 digest in Settings →
 * Prompts) — see `sopCompilerStore.ts`'s `sendToReview`, which calls that
 * store's existing `createProposal` unchanged. Nothing compiled by this
 * module is ever installed or activated directly: it only ever produces a
 * `CompiledWorkflowDraft` in memory/localStorage, and the ONLY path from a
 * draft into something runnable is the existing quarantined-proposal review
 * flow, so a compiled workflow always stays inactive until a human reviews
 * and approves its digest there.
 *
 * Model-facing shape mirrors `riskJudge.ts`'s dependency-injection pattern
 * (a `callModel` closure passed in, not `attemptStream` imported directly)
 * so this file stays pure TS with no store/React import: `sopCompilerStore.ts`
 * is the one that builds the closure around `agentLoop.ts`'s `resolveTarget`
 * and `turnEngine.ts`'s `attemptStream`, exactly like `compactSessionNow`
 * does for its own one-shot summary call.
 */
import type { ChatMessage } from './llamaClient';

/** Caps how much of a pasted/imported SOP is sent to the model in one turn —
 * generous for a runbook/checklist, but bounded so a huge training doc can't
 * blow up a local model's context window. */
export const MAX_SOP_SOURCE_CHARS = 20_000;

export interface CompiledStep {
  order: number;
  action: string;
}

export interface CompiledInput {
  name: string;
  description: string;
  required: boolean;
}

export type PolicyGateRisk = 'low' | 'medium' | 'high';

export interface CompiledPolicyGate {
  label: string;
  description: string;
  riskLevel: PolicyGateRisk;
}

export interface CompiledTestCase {
  label: string;
  expected: string;
}

export interface CompiledEvidence {
  label: string;
  description: string;
}

export interface CompiledWorkflowDraft {
  name: string;
  summary: string;
  /** A `/command`-safe slug (see `slugifyCommand`) suggested for the
   * eventual skill proposal — the reviewer can still rename it before
   * approving, `skillProposalStore.createProposal` re-validates it either way. */
  suggestedCommand: string;
  steps: CompiledStep[];
  inputs: CompiledInput[];
  policyGates: CompiledPolicyGate[];
  tests: CompiledTestCase[];
  evidence: CompiledEvidence[];
}

/** The minimal subset of `turnEngine.ts`'s `AttemptResult` this module needs
 * from `callModel` — see this file's top doc comment for why it's
 * dependency-injected rather than imported. */
export interface SopCompilerCallResult {
  content: string;
  streamError: string | null;
}

function truncateSource(text: string): string {
  const trimmed = text.trim();
  return trimmed.length > MAX_SOP_SOURCE_CHARS
    ? `${trimmed.slice(0, MAX_SOP_SOURCE_CHARS)}…`
    : trimmed;
}

/** 1-32 lowercase letters/digits/hyphens, starting with a letter/digit — the
 * exact same shape `skillProposalStore.ts`'s (private) `validate()` enforces
 * for a skill command, duplicated here (rather than imported, since that
 * function isn't exported) so a draft's `suggestedCommand` is already valid
 * by the time it reaches `createProposal`. */
const COMMAND_PATTERN = /^[a-z0-9][a-z0-9-]{0,31}$/;

export function slugifyCommand(name: string): string {
  const slug = name
    .trim()
    .toLowerCase()
    .replace(/[^a-z0-9]+/g, '-')
    .replace(/^-+|-+$/g, '')
    .slice(0, 32)
    .replace(/-+$/g, '');
  if (COMMAND_PATTERN.test(slug)) return slug;
  return 'sop-compiled-workflow';
}

/** Builds the one-shot, tool-less compiler prompt. Strict-JSON-only, exactly
 * like `riskJudge.ts`'s judge prompt, so `parseSopCompilerResponse` below has
 * a fixed shape to validate against regardless of which local/provider model
 * answers it. */
export function buildSopCompilerMessages(sourceText: string, sourceLabel?: string): ChatMessage[] {
  return [
    {
      role: 'system',
      content: [
        'You compile a Standard Operating Procedure (SOP), runbook, checklist, or training document into a DRAFT structured workflow definition for a review queue — you never execute anything and the compiled draft is never active until a human approves it.',
        'Read the SOP text and extract: a short name; a one-sentence summary; an ordered list of concrete action steps; the inputs/parameters an operator or system must supply before running it; policy/permission gates (points requiring approval, credentials, destructive/irreversible actions, or compliance sign-off) with a risk level; a short test/acceptance checklist that would prove the compiled workflow behaves correctly; and the evidence/artifacts that must be captured as proof of a correct run (logs, screenshots, approval records, output files, etc).',
        'Reply with ONLY a single-line JSON object of this exact shape, no markdown, no other text:',
        '{"name":"...","summary":"...","suggestedCommand":"lowercase-hyphenated-slug","steps":[{"order":1,"action":"..."}],"inputs":[{"name":"...","description":"...","required":true}],"policyGates":[{"label":"...","description":"...","riskLevel":"low|medium|high"}],"tests":[{"label":"...","expected":"..."}],"evidence":[{"label":"...","description":"..."}]}',
        'If the source text does not clearly state inputs, policy gates, tests, or evidence, infer the most reasonable minimal set from context rather than leaving the array empty.',
      ].join(' '),
    },
    {
      role: 'user',
      content: `${sourceLabel ? `Source: ${sourceLabel}\n\n` : ''}SOP/runbook/checklist text:\n\n${truncateSource(sourceText)}`,
    },
  ];
}

function asNonEmptyString(value: unknown): string | null {
  return typeof value === 'string' && value.trim().length > 0 ? value.trim() : null;
}

function asRiskLevel(value: unknown): PolicyGateRisk {
  return value === 'low' || value === 'medium' || value === 'high' ? value : 'medium';
}

/**
 * Strict parse of the compiler's reply, mirroring `riskJudge.ts`'s
 * `parseJudgeResponse`: tries the raw trimmed content first, then falls back
 * to the first `{...}` span (small local models sometimes wrap otherwise
 * valid JSON in a sentence or code fence). Returns `null` on anything
 * malformed — callers must fail closed (surface an error), never fabricate a
 * draft from a bad response.
 */
export function parseSopCompilerResponse(content: string): CompiledWorkflowDraft | null {
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
    const record = parsed as Record<string, unknown>;

    const name = asNonEmptyString(record.name);
    const summary = asNonEmptyString(record.summary);
    if (!name || !summary) continue;

    const steps: CompiledStep[] = Array.isArray(record.steps)
      ? record.steps
          .map((entry, index) => {
            const action = asNonEmptyString((entry as { action?: unknown })?.action);
            if (!action) return null;
            const orderValue = (entry as { order?: unknown })?.order;
            const order = typeof orderValue === 'number' && Number.isFinite(orderValue) ? orderValue : index + 1;
            return { order, action };
          })
          .filter((entry): entry is CompiledStep => entry !== null)
      : [];

    const inputs: CompiledInput[] = Array.isArray(record.inputs)
      ? record.inputs
          .map((entry) => {
            const entryName = asNonEmptyString((entry as { name?: unknown })?.name);
            if (!entryName) return null;
            return {
              name: entryName,
              description: asNonEmptyString((entry as { description?: unknown })?.description) ?? '',
              required: (entry as { required?: unknown })?.required !== false,
            };
          })
          .filter((entry): entry is CompiledInput => entry !== null)
      : [];

    const policyGates: CompiledPolicyGate[] = Array.isArray(record.policyGates)
      ? record.policyGates
          .map((entry) => {
            const label = asNonEmptyString((entry as { label?: unknown })?.label);
            if (!label) return null;
            return {
              label,
              description: asNonEmptyString((entry as { description?: unknown })?.description) ?? '',
              riskLevel: asRiskLevel((entry as { riskLevel?: unknown })?.riskLevel),
            };
          })
          .filter((entry): entry is CompiledPolicyGate => entry !== null)
      : [];

    const tests: CompiledTestCase[] = Array.isArray(record.tests)
      ? record.tests
          .map((entry) => {
            const label = asNonEmptyString((entry as { label?: unknown })?.label);
            if (!label) return null;
            return { label, expected: asNonEmptyString((entry as { expected?: unknown })?.expected) ?? '' };
          })
          .filter((entry): entry is CompiledTestCase => entry !== null)
      : [];

    const evidence: CompiledEvidence[] = Array.isArray(record.evidence)
      ? record.evidence
          .map((entry) => {
            const label = asNonEmptyString((entry as { label?: unknown })?.label);
            if (!label) return null;
            return { label, description: asNonEmptyString((entry as { description?: unknown })?.description) ?? '' };
          })
          .filter((entry): entry is CompiledEvidence => entry !== null)
      : [];

    return {
      name,
      summary,
      suggestedCommand: slugifyCommand(asNonEmptyString(record.suggestedCommand) ?? name),
      steps,
      inputs: withFallbackInputs(inputs),
      policyGates: withFallbackPolicyGates(policyGates),
      tests: withFallbackTests(tests),
      evidence: withFallbackEvidence(evidence),
    };
  }
  return null;
}

/** Every compiled draft must ALWAYS carry at least one entry in each of
 * inputs/policy gates/tests/evidence, per this feature's acceptance
 * criterion — a small/quantized local model sometimes omits a section
 * entirely even when told not to, so these fallbacks backstop that rather
 * than shipping a draft silently missing a required category. */
function withFallbackInputs(inputs: CompiledInput[]): CompiledInput[] {
  if (inputs.length > 0) return inputs;
  return [
    {
      name: 'operator_confirmation',
      description: 'Confirmation from the operator running this workflow that the source SOP still matches current practice.',
      required: true,
    },
  ];
}

function withFallbackPolicyGates(gates: CompiledPolicyGate[]): CompiledPolicyGate[] {
  if (gates.length > 0) return gates;
  return [
    {
      label: 'Human approval required before live execution',
      description: 'No compiled step from this SOP may run against real systems until a human has reviewed and approved this workflow.',
      riskLevel: 'high',
    },
  ];
}

function withFallbackTests(tests: CompiledTestCase[]): CompiledTestCase[] {
  if (tests.length > 0) return tests;
  return [
    {
      label: 'Dry run against a representative sample matches the SOP\'s expected outcome',
      expected: 'Every declared step completes and produces the outcome the source SOP describes, with no unexpected side effects.',
    },
  ];
}

function withFallbackEvidence(evidence: CompiledEvidence[]): CompiledEvidence[] {
  if (evidence.length > 0) return evidence;
  return [
    {
      label: 'Run log',
      description: 'A capture of the compiled workflow\'s full run output, retained for audit alongside the reviewed proposal.',
    },
  ];
}

/**
 * Runs the one-shot, non-streaming, tool-less compiler call and returns a
 * validated draft. Unlike `riskJudge.ts`'s `classifyToolCall` (which fails
 * closed to `null` because it only ever annotates an existing permission
 * prompt), a failure here is surfaced to the user as a real error — nothing
 * about "the compiler failed" should be silently swallowed in a user-driven
 * import/compile action.
 */
export async function compileSop(
  sourceText: string,
  callModel: (messages: ChatMessage[], signal?: AbortSignal) => Promise<SopCompilerCallResult>,
  sourceLabel?: string,
  signal?: AbortSignal,
): Promise<CompiledWorkflowDraft> {
  if (!sourceText.trim()) {
    throw new Error('Paste or import an SOP, runbook, checklist, or training document before compiling.');
  }
  const result = await callModel(buildSopCompilerMessages(sourceText, sourceLabel), signal);
  if (result.streamError) {
    throw new Error(result.streamError);
  }
  const draft = parseSopCompilerResponse(result.content);
  if (!draft) {
    throw new Error('The model did not return a compilable workflow definition. Try again, or simplify the source document.');
  }
  return draft;
}

function formatSteps(steps: CompiledStep[]): string {
  if (steps.length === 0) return '(no steps extracted)';
  return steps
    .slice()
    .sort((left, right) => left.order - right.order)
    .map((step) => `${step.order}. ${step.action}`)
    .join('\n');
}

function formatInputs(inputs: CompiledInput[]): string {
  return inputs
    .map((input) => `- \`${input.name}\` (${input.required ? 'required' : 'optional'})${input.description ? ` — ${input.description}` : ''}`)
    .join('\n');
}

function formatPolicyGates(gates: CompiledPolicyGate[]): string {
  return gates
    .map((gate) => `- [${gate.riskLevel.toUpperCase()}] ${gate.label}${gate.description ? ` — ${gate.description}` : ''}`)
    .join('\n');
}

function formatTests(tests: CompiledTestCase[]): string {
  return tests
    .map((test) => `- [ ] ${test.label}${test.expected ? ` — expected: ${test.expected}` : ''}`)
    .join('\n');
}

function formatEvidence(evidence: CompiledEvidence[]): string {
  return evidence
    .map((item) => `- ${item.label}${item.description ? ` — ${item.description}` : ''}`)
    .join('\n');
}

/**
 * Renders a compiled draft into the `instructions` text handed to
 * `skillProposalStore.createProposal` — this IS the hand-off into the
 * existing review flow (see this file's top doc comment): the resulting
 * `SkillProposal` starts `quarantined` exactly like every other proposal,
 * and only that store's own `approveProposal` (an explicit user action in
 * Settings → Prompts, digest-checked) can ever turn it into a usable
 * `/command`. Nothing in this module or its caller activates anything.
 */
export function renderCompiledSkillInstructions(draft: CompiledWorkflowDraft, sourceExcerpt: string): string {
  return [
    `# ${draft.name}`,
    '',
    '_Compiled by the SOP-to-Agent Compiler from an imported SOP/runbook/checklist. This is a DRAFT — it stays inactive until reviewed, tested, and approved through the Skill Proposals review flow._',
    '',
    draft.summary,
    '',
    '## Steps',
    formatSteps(draft.steps),
    '',
    '## Required inputs',
    formatInputs(draft.inputs),
    '',
    '## Policy / permission gates',
    formatPolicyGates(draft.policyGates),
    '',
    '## Acceptance / test checklist',
    formatTests(draft.tests),
    '',
    '## Required evidence',
    formatEvidence(draft.evidence),
    '',
    '## Source excerpt',
    sourceExcerpt.trim() ? sourceExcerpt.trim() : '(no source excerpt retained)',
  ].join('\n');
}
