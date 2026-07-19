/**
 * Deep Research Workspace (ROADMAP.md Phase 7 "Deep Research Workspace"):
 * given a research question, (1) asks the currently active model to draft a
 * short multi-step plan naming which real sources to check — the web (via
 * the existing `tool_web_search` primitive), local workspace files (via
 * `tool_grep`), and indexed knowledge stacks (via `knowledgeV2Store`'s
 * hybrid-search `query`) — (2) actually executes each planned step for real
 * against whichever of those is genuinely available, collecting evidence
 * snippets with their origin, and (3) asks the model to synthesize a report
 * whose every claim is REQUIRED to cite at least one of the actually-
 * collected evidence snippets by id.
 *
 * The acceptance bar ("every research conclusion links to source evidence
 * and shows which sources were searched or skipped") is enforced
 * STRUCTURALLY, not just by prompting: `parseReportResponse` below drops any
 * model-produced claim that cites zero evidence ids or an id that doesn't
 * exist in the evidence actually collected this run — a claim can only ever
 * survive into `ResearchReport.claims` carrying real, resolvable citations.
 * Likewise, `StepOutcome.status` (`'searched' | 'skipped' | 'error'`) is
 * computed by the step executors below from what actually happened, never
 * asserted by the model.
 *
 * Scope, deliberately narrowed for this MVP (see ROADMAP.md's own item
 * description, "web, local files, knowledge stacks, and connected apps"):
 * - Web: `tool_web_search` only (DuckDuckGo by default, permission-gated
 *   exactly like a normal chat turn's `web_search` tool call) — each
 *   result's own title/url/snippet is used as evidence directly. Fetching
 *   the full page of a result (`tool_web_fetch`) for a deeper quote is a
 *   natural follow-up, left undone here to keep one permission prompt per
 *   web step rather than two.
 * - Files: `tool_grep` across the open workspace, treated as a plain
 *   substring/regex search over the step's own query text.
 * - Knowledge: only stacks that are actually indexed
 *   (`stackStore`'s `indexed_at != null && chunk_count > 0`) are ever
 *   offered to the plan; `knowledgeV2Store.query`'s hybrid search results
 *   are used as evidence, one snippet per hit.
 * - Connected apps (MCP): a connected+enabled MCP server can be NAMED as a
 *   candidate source in the plan (so the source map can honestly show
 *   "identified but not queried"), but no MCP tool call is ever
 *   auto-dispatched for it in this MVP — every `connector`-kind step is
 *   always executed as `'skipped'` with an explicit reason, never fabricated
 *   evidence. Automating a real, schema-safe MCP tool call per connector is
 *   a follow-up, not attempted here.
 */
import type { ChatMessage } from './llamaClient';
import { invoke } from '@tauri-apps/api/core';
import { resolveTarget } from './agentLoop';
import { attemptStream, type ResolvedTarget } from './turnEngine';
import { useKnowledgeV2Store, DEFAULT_HYBRID_CONFIG } from '../store/knowledgeV2Store';
import { useStackStore } from '../store/stackStore';
import { useWorkspaceStore } from '../store/workspaceStore';
import { useMcpStore } from '../store/mcpStore';
import { useDeepResearchStore } from '../store/deepResearchStore';

export type ResearchSourceKind = 'web' | 'file' | 'knowledge' | 'connector';

export interface ResearchStepPlan {
  id: string;
  kind: ResearchSourceKind;
  query: string;
  rationale: string;
  /** Set only for `kind: 'knowledge'` steps — the exact stack this step
   * queries, resolved against the CALLER's own available-stack list at plan
   * time (never trusted verbatim from the model beyond picking a name off
   * that list). */
  stackId?: string;
  stackName?: string;
  /** Set only for `kind: 'connector'` steps — see the module doc comment for
   * why these are always executed as skipped in this MVP. */
  connectorId?: string;
  connectorLabel?: string;
}

export interface ResearchPlan {
  question: string;
  steps: ResearchStepPlan[];
}

export type StepStatus = 'searched' | 'skipped' | 'error';

export interface EvidenceSnippet {
  /** Global citation id, e.g. `"S1"` — assigned once per run, in the order
   * steps actually execute (see `assignStepEvidenceIds`), so a report's
   * `[S3]`-style citation always resolves to exactly one snippet. */
  id: string;
  stepId: string;
  kind: ResearchSourceKind;
  sourceLabel: string;
  /** URL, `file:line`, or knowledge-stack document URI — whatever a user
   * would need to go look at the original source themselves. */
  sourceRef: string;
  snippet: string;
}

export interface StepOutcome {
  step: ResearchStepPlan;
  status: StepStatus;
  /** Human-readable reason, set whenever `status !== 'searched'` (a
   * `'searched'` step's own zero-evidence case is folded into `'skipped'`
   * instead — see the per-kind executors below). */
  reason: string | null;
  evidence: EvidenceSnippet[];
}

export interface ReportClaim {
  id: string;
  text: string;
  /** Non-empty by construction — see `parseReportResponse`, which drops any
   * claim that would otherwise have none. */
  evidenceIds: string[];
}

export interface ResearchReport {
  summary: string;
  claims: ReportClaim[];
  openQuestions: string[];
  /** Count of model-produced claims discarded during parsing because they
   * cited no evidence id, or cited one that doesn't exist in this run's
   * evidence — surfaced so the UI/tests can show synthesis wasn't perfectly
   * clean, rather than silently dropping them. */
  droppedClaimCount: number;
}

// ---------------------------------------------------------------------------
// JSON extraction shared by both parse functions below
// ---------------------------------------------------------------------------

function extractJson(content: string): unknown | null {
  const trimmed = content.trim();
  const candidates: string[] = [];
  const fenced = trimmed.match(/```(?:json)?\s*([\s\S]*?)```/i);
  if (fenced) candidates.push(fenced[1].trim());
  candidates.push(trimmed);
  const braceSpan = trimmed.match(/\{[\s\S]*\}/);
  if (braceSpan) candidates.push(braceSpan[0]);

  for (const candidate of candidates) {
    try {
      return JSON.parse(candidate);
    } catch {
      continue;
    }
  }
  return null;
}

function truncate(text: string, max: number): string {
  const trimmed = text.trim();
  return trimmed.length > max ? `${trimmed.slice(0, max)}…` : trimmed;
}

function errorMessage(err: unknown): string {
  if (err instanceof Error) return err.message;
  if (typeof err === 'string') return err;
  try {
    return JSON.stringify(err);
  } catch {
    return String(err);
  }
}

// ---------------------------------------------------------------------------
// Step 1: plan
// ---------------------------------------------------------------------------

export const MAX_PLAN_STEPS = 6;

const VALID_KINDS: ReadonlySet<string> = new Set(['web', 'file', 'knowledge', 'connector']);

export interface PlanContext {
  /** Real indexed knowledge stacks the plan is allowed to reference — only
   * ever populated from `stackStore`'s own already-indexed stacks (see
   * `buildPlanContext`), never invented. */
  stackOptions: Array<{ id: string; name: string }>;
  hasWorkspace: boolean;
  connectorOptions: Array<{ id: string; label: string }>;
}

/** Reads the real, currently-available source catalog from the app's own
 * stores — the single place plan prompts/validation get their "what's
 * actually there" answer from, so a step can never be planned against a
 * stack/connector/workspace that doesn't genuinely exist right now. */
export function buildPlanContext(): PlanContext {
  const stackOptions = useStackStore
    .getState()
    .stacks.filter((stack) => stack.indexed_at != null && stack.chunk_count > 0)
    .map((stack) => ({ id: stack.id, name: stack.name }));
  const hasWorkspace = useWorkspaceStore.getState().roots.length > 0;
  const connectorOptions = useMcpStore
    .getState()
    .servers.filter((server) => server.enabled && server.status === 'connected')
    .map((server) => ({ id: server.id, label: server.label }));
  return { stackOptions, hasWorkspace, connectorOptions };
}

export function buildPlanMessages(question: string, context: PlanContext): ChatMessage[] {
  const sourceLines = [
    '- "web": a live web search (DuckDuckGo). Always available.',
    context.hasWorkspace
      ? '- "file": a regular-expression search across the user\'s open workspace files. Available.'
      : '- "file": NOT available right now (no workspace folder is open) — do not propose this kind.',
    context.stackOptions.length > 0
      ? `- "knowledge": a hybrid search of one of the user's indexed knowledge stacks. Available stacks: ${context.stackOptions
          .map((s) => `"${s.name}"`)
          .join(', ')}. Set "stack_name" to exactly one of these names.`
      : '- "knowledge": NOT available right now (no indexed knowledge stack) — do not propose this kind.',
    context.connectorOptions.length > 0
      ? `- "connector": a connected app/tool. Available connectors: ${context.connectorOptions
          .map((c) => `"${c.label}"`)
          .join(', ')}. Set "connector_name" to exactly one of these labels. Note: this MVP only IDENTIFIES a relevant connector, it does not query it automatically — only propose this if a connector is genuinely relevant.`
      : '- "connector": NOT available right now (no connected app) — do not propose this kind.',
  ];

  const system = [
    'You are a research planner. Given a research question, draft a short, concrete plan of which real sources to check.',
    `Produce at most ${MAX_PLAN_STEPS} steps. Each step names exactly one source kind and a specific search query for it.`,
    'Available source kinds for THIS run:',
    ...sourceLines,
    'Reply with ONLY a single JSON object of this exact shape, no markdown fences, no other text:',
    '{"steps": [{"kind": "web", "query": "...", "rationale": "..."}, {"kind": "knowledge", "query": "...", "rationale": "...", "stack_name": "..."}]}',
    'Every step needs "kind", "query", and a one-sentence "rationale". Only use a "kind" listed as available above. Do not propose more than one step for a source kind unless genuinely useful with a different query.',
  ].join('\n');

  return [
    { role: 'system', content: system },
    { role: 'user', content: `Research question: ${question}` },
  ];
}

/**
 * Parses the plan model's reply into a validated `ResearchPlan`. Anything
 * that doesn't fit — unparseable JSON, a missing/invalid `kind`, an empty
 * `query` — is simply dropped rather than failing the whole run; if
 * NOTHING survives, falls back to a single `'web'` step on the raw question
 * itself so a run always has something real to execute rather than
 * dead-ending on a bad model reply.
 */
export function parsePlanResponse(raw: string, question: string, context?: PlanContext): ResearchPlan {
  const parsed = extractJson(raw) as { steps?: unknown } | null;
  const rawSteps = Array.isArray(parsed?.steps) ? (parsed!.steps as unknown[]) : [];

  const stackByName = new Map((context?.stackOptions ?? []).map((s) => [s.name, s.id]));
  const connectorByLabel = new Map((context?.connectorOptions ?? []).map((c) => [c.label, c.id]));

  const steps: ResearchStepPlan[] = [];
  for (const entry of rawSteps) {
    if (steps.length >= MAX_PLAN_STEPS) break;
    if (!entry || typeof entry !== 'object') continue;
    const e = entry as Record<string, unknown>;
    const kind = typeof e.kind === 'string' ? e.kind.trim().toLowerCase() : '';
    if (!VALID_KINDS.has(kind)) continue;
    const query = typeof e.query === 'string' ? e.query.trim() : '';
    if (!query) continue;
    const rationale = typeof e.rationale === 'string' && e.rationale.trim() ? e.rationale.trim() : 'No rationale given.';

    const stackName = typeof e.stack_name === 'string' ? e.stack_name : undefined;
    const connectorLabel = typeof e.connector_name === 'string' ? e.connector_name : undefined;

    // A 'knowledge' step must resolve to a real, currently-available stack
    // (when context is supplied) — an unresolvable stack name is dropped
    // rather than executed against a made-up id.
    if (kind === 'knowledge' && context && (!stackName || !stackByName.has(stackName))) continue;
    if (kind === 'connector' && context && (!connectorLabel || !connectorByLabel.has(connectorLabel))) continue;

    steps.push({
      id: `P${steps.length + 1}`,
      kind: kind as ResearchSourceKind,
      query,
      rationale,
      stackId: kind === 'knowledge' ? stackByName.get(stackName!) : undefined,
      stackName: kind === 'knowledge' ? stackName : undefined,
      connectorId: kind === 'connector' ? connectorByLabel.get(connectorLabel!) : undefined,
      connectorLabel: kind === 'connector' ? connectorLabel : undefined,
    });
  }

  if (steps.length === 0) {
    return {
      question,
      steps: [
        {
          id: 'P1',
          kind: 'web',
          query: question,
          rationale: "Fallback plan — the planner's response could not be parsed into any valid step, so this run defaults to a single web search on the question itself.",
        },
      ],
    };
  }

  return { question, steps };
}

// ---------------------------------------------------------------------------
// Step 2: execute
// ---------------------------------------------------------------------------

const WEB_SEARCH_COUNT = 5;
const MAX_WEB_EVIDENCE = 3;
const MAX_FILE_EVIDENCE = 5;
const MAX_KNOWLEDGE_EVIDENCE = 4;
const KNOWLEDGE_TOKEN_BUDGET = 4000;
const MAX_SNIPPET_CHARS = 600;

function escapeRegExp(text: string): string {
  return text.replace(/[.*+?^${}()|[\]\\]/g, '\\$&');
}

/** Whether `err`'s message is the exact Rust-side denial string
 * (`permissions.rs`'s `request_permission`) — used to give the user a
 * clearer "denied" reason than a generic error bucket for a tool that
 * requires their permission (web search/fetch). */
function isPermissionDenied(err: unknown): boolean {
  return errorMessage(err).includes('Permission denied');
}

async function executeWebStep(step: ResearchStepPlan, turnId: string): Promise<StepOutcome> {
  try {
    const results = await invoke<Array<{ title: string; url: string; snippet: string }>>('tool_web_search', {
      query: step.query,
      count: WEB_SEARCH_COUNT,
      turn_id: turnId,
      tool_call_id: crypto.randomUUID(),
    });
    if (!results || results.length === 0) {
      return { step, status: 'skipped', reason: `Web search returned no results for "${step.query}".`, evidence: [] };
    }
    const evidence: EvidenceSnippet[] = results.slice(0, MAX_WEB_EVIDENCE).map((result) => ({
      id: '',
      stepId: step.id,
      kind: 'web' as const,
      sourceLabel: result.title || result.url,
      sourceRef: result.url,
      snippet: truncate(result.snippet, MAX_SNIPPET_CHARS),
    }));
    return { step, status: 'searched', reason: null, evidence };
  } catch (err) {
    if (isPermissionDenied(err)) {
      return { step, status: 'skipped', reason: 'Permission to search the web was denied.', evidence: [] };
    }
    return { step, status: 'error', reason: errorMessage(err), evidence: [] };
  }
}

async function executeFileStep(step: ResearchStepPlan): Promise<StepOutcome> {
  try {
    const pattern = escapeRegExp(step.query);
    const matches = await invoke<Array<{ file: string; line: number; text: string }>>('tool_grep', { pattern });
    if (!matches || matches.length === 0) {
      return { step, status: 'skipped', reason: `No workspace files matched "${step.query}".`, evidence: [] };
    }
    const evidence: EvidenceSnippet[] = matches.slice(0, MAX_FILE_EVIDENCE).map((match) => ({
      id: '',
      stepId: step.id,
      kind: 'file' as const,
      sourceLabel: `${match.file}:${match.line}`,
      sourceRef: `${match.file}:${match.line}`,
      snippet: truncate(match.text, MAX_SNIPPET_CHARS),
    }));
    return { step, status: 'searched', reason: null, evidence };
  } catch (err) {
    return { step, status: 'error', reason: errorMessage(err), evidence: [] };
  }
}

async function executeKnowledgeStep(step: ResearchStepPlan): Promise<StepOutcome> {
  if (!step.stackId) {
    return { step, status: 'skipped', reason: 'No indexed knowledge stack was available for this step.', evidence: [] };
  }
  try {
    const response = await useKnowledgeV2Store
      .getState()
      .query(step.stackId, step.query, DEFAULT_HYBRID_CONFIG, [], false, KNOWLEDGE_TOKEN_BUDGET);
    const hits = response.search.hits.slice(0, MAX_KNOWLEDGE_EVIDENCE);
    if (hits.length === 0) {
      return {
        step,
        status: 'skipped',
        reason: `No matches found in knowledge stack "${step.stackName ?? step.stackId}" for "${step.query}".`,
        evidence: [],
      };
    }
    const evidence: EvidenceSnippet[] = hits.map((hit) => ({
      id: '',
      stepId: step.id,
      kind: 'knowledge' as const,
      sourceLabel: `${step.stackName ?? step.stackId} · ${hit.chunk.citation.canonical_uri}`,
      sourceRef: hit.chunk.citation.canonical_uri,
      snippet: truncate(hit.chunk.text, MAX_SNIPPET_CHARS),
    }));
    return { step, status: 'searched', reason: null, evidence };
  } catch (err) {
    return { step, status: 'error', reason: errorMessage(err), evidence: [] };
  }
}

/** Always skipped — see the module doc comment's "Connected apps (MCP)"
 * scoping note for why this MVP never auto-dispatches an MCP tool call. */
function executeConnectorStep(step: ResearchStepPlan): Promise<StepOutcome> {
  return Promise.resolve({
    step,
    status: 'skipped',
    reason: `Connector research isn't automated in this MVP — open "${step.connectorLabel ?? step.connectorId ?? 'the connector'}" from the MCP tools settings to query it directly.`,
    evidence: [],
  });
}

export async function executeResearchStep(step: ResearchStepPlan, ctx: { turnId: string }): Promise<StepOutcome> {
  switch (step.kind) {
    case 'web':
      return executeWebStep(step, ctx.turnId);
    case 'file':
      return executeFileStep(step);
    case 'knowledge':
      return executeKnowledgeStep(step);
    case 'connector':
      return executeConnectorStep(step);
    default:
      return { step, status: 'error', reason: `Unknown source kind "${step.kind}".`, evidence: [] };
  }
}

/**
 * Assigns this step's evidence its global citation ids, continuing from
 * `startId` — kept as its own small pure function (rather than inlined in
 * the runner loop) so the id-assignment contract (sequential, run-wide,
 * `"S<n>"`) is independently testable without needing a real step
 * execution.
 */
export function assignStepEvidenceIds(outcome: StepOutcome, startId: number): { outcome: StepOutcome; nextId: number } {
  let counter = startId;
  const evidence = outcome.evidence.map((snippet) => ({ ...snippet, id: `S${counter++}` }));
  return { outcome: { ...outcome, evidence }, nextId: counter };
}

// ---------------------------------------------------------------------------
// Step 3: synthesize
// ---------------------------------------------------------------------------

export function buildSynthesisMessages(question: string, outcomes: readonly StepOutcome[]): ChatMessage[] {
  const evidenceLines = outcomes.flatMap((outcome) =>
    outcome.evidence.map((snippet) => `[${snippet.id}] (${snippet.kind} — ${snippet.sourceLabel}): ${snippet.snippet}`),
  );
  const skippedLines = outcomes
    .filter((outcome) => outcome.status !== 'searched')
    .map((outcome) => `- (${outcome.step.kind}) "${outcome.step.query}" — ${outcome.status}: ${outcome.reason ?? 'unknown reason'}`);

  const system = [
    'You are a careful research analyst. You are given a research question and a FIXED set of already-collected evidence snippets, each with a citation id like [S1].',
    'Write a synthesis using ONLY the given evidence. Every claim you make MUST cite at least one evidence id that genuinely supports it. Never invent an evidence id and never state a conclusion with zero citations — if the evidence doesn\'t support a conclusion, leave it out and mention it as an open question instead.',
    'Reply with ONLY a single JSON object of this exact shape, no markdown fences, no other text:',
    '{"summary": "...", "claims": [{"text": "...", "evidence_ids": ["S1","S2"]}], "open_questions": ["..."]}',
    '"summary" is a short 1-3 sentence overview (no citation required). "claims" is a list of specific, checkable conclusions, each carrying the evidence_ids that support it. "open_questions" lists what remains unanswered given what was — and was not — actually searched.',
  ].join('\n');

  const user = [
    `Research question: ${question}`,
    '',
    'Collected evidence:',
    evidenceLines.length > 0 ? evidenceLines.join('\n') : '(none collected)',
    '',
    'Sources skipped or errored — do not cite these, they carry no evidence:',
    skippedLines.length > 0 ? skippedLines.join('\n') : '(none)',
  ].join('\n');

  return [
    { role: 'system', content: system },
    { role: 'user', content: user },
  ];
}

/**
 * Parses the synthesis model's reply into a `ResearchReport`, enforcing the
 * feature's core acceptance bar in code rather than merely by prompting: any
 * claim citing zero evidence ids, or an id not present in `evidenceIds`
 * (this run's ACTUAL collected evidence), is dropped and counted in
 * `droppedClaimCount` — a claim can only ever survive with real, resolvable
 * citations.
 */
export function parseReportResponse(raw: string, evidenceIds: readonly string[]): ResearchReport {
  const validIds = new Set(evidenceIds);
  const parsed = extractJson(raw) as { summary?: unknown; claims?: unknown; open_questions?: unknown } | null;

  const rawClaims = Array.isArray(parsed?.claims) ? (parsed!.claims as unknown[]) : [];
  const claims: ReportClaim[] = [];
  let dropped = 0;
  for (const entry of rawClaims) {
    if (!entry || typeof entry !== 'object') {
      dropped++;
      continue;
    }
    const e = entry as Record<string, unknown>;
    const text = typeof e.text === 'string' ? e.text.trim() : '';
    const rawIds = Array.isArray(e.evidence_ids) ? e.evidence_ids : [];
    const claimEvidenceIds = rawIds.filter((id): id is string => typeof id === 'string' && validIds.has(id));
    if (!text || claimEvidenceIds.length === 0) {
      dropped++;
      continue;
    }
    claims.push({ id: `C${claims.length + 1}`, text, evidenceIds: claimEvidenceIds });
  }

  const openQuestions = Array.isArray(parsed?.open_questions)
    ? (parsed!.open_questions as unknown[])
        .filter((q): q is string => typeof q === 'string' && q.trim().length > 0)
        .map((q) => q.trim())
    : [];

  const summary = typeof parsed?.summary === 'string' && parsed.summary.trim() ? parsed.summary.trim() : '';

  if (claims.length === 0 && !summary) {
    return {
      summary:
        "The synthesis response could not be parsed into any evidence-linked claims — review the collected evidence and source map below directly.",
      claims: [],
      openQuestions: openQuestions.length > 0 ? openQuestions : ['Re-run the synthesis step, or review the raw evidence manually.'],
      droppedClaimCount: dropped,
    };
  }

  return { summary, claims, openQuestions, droppedClaimCount: dropped };
}

export { errorMessage as deepResearchErrorMessage };

// ---------------------------------------------------------------------------
// Orchestration: ties plan -> execute -> synthesize together and drives
// `deepResearchStore.ts` as each phase completes, mirroring
// `sideTaskRunner.ts`'s "store here, runner logic in the lib file" split.
// ---------------------------------------------------------------------------

/** Per-run cancellation, keyed by run id — same "process-local JS handle,
 * not something Rust needs to key on for this part" posture as
 * `sideTaskRunner.ts`'s own `controllers` map. Web-search/grep calls made
 * through it still get their own `turn_id` (the run id itself) so a denial
 * or Stop-button cancellation from elsewhere can never cross into another
 * run's pending permission prompt. */
const controllers = new Map<string, AbortController>();

/** Cancels a research run's in-flight step/model call, if any — a no-op if
 * the run already reached a terminal status. */
export function cancelDeepResearch(runId: string): void {
  controllers.get(runId)?.abort();
  const run = useDeepResearchStore.getState().runs[runId];
  if (run && (run.status === 'planning' || run.status === 'researching' || run.status === 'synthesizing')) {
    useDeepResearchStore.getState().setStatus(runId, 'cancelled');
  }
}

/** Creates a new run (status `'planning'`) and fires its pipeline off
 * WITHOUT awaiting it, returning the new run's id immediately — same
 * fire-and-forget shape as `sideTaskRunner.ts`'s `startSideTask`, so the
 * panel can select/render the new run right away. */
export function startDeepResearch(question: string): string {
  const trimmed = question.trim();
  const run = useDeepResearchStore.getState().create(trimmed);
  void runDeepResearch(run.id);
  return run.id;
}

/** Drives one research run's plan -> execute -> synthesize pipeline to a
 * terminal status. Exported (alongside `startDeepResearch`, the real entry
 * point every caller uses) so `deepResearch.test.ts` can `await` a run
 * directly instead of polling the store — same reasoning as
 * `subagent.ts`'s `runSubagentTask`/`sideTaskRunner.ts`'s `runSideTask` both
 * being directly awaitable for their own tests.
 */
export async function runDeepResearch(runId: string): Promise<void> {
  const controller = new AbortController();
  controllers.set(runId, controller);
  const store = useDeepResearchStore.getState();

  try {
    const run = store.runs[runId];
    if (!run) return;

    let target: ResolvedTarget;
    try {
      target = await resolveTarget();
    } catch (err) {
      store.setError(runId, errorMessage(err));
      return;
    }
    if (controller.signal.aborted) return;

    // --- 1. Plan --------------------------------------------------------
    const context = buildPlanContext();
    const planMessages = buildPlanMessages(run.question, context);
    const planAttempt = await attemptStream(target, planMessages, [], controller.signal, undefined, runId, undefined, false);
    if (controller.signal.aborted) return;
    if (planAttempt.streamError !== null) {
      store.setError(runId, planAttempt.streamError);
      return;
    }
    const plan = parsePlanResponse(planAttempt.content, run.question, context);
    store.setPlan(runId, plan);

    // --- 2. Execute -------------------------------------------------------
    store.setStatus(runId, 'researching');
    let nextEvidenceId = 1;
    for (const step of plan.steps) {
      if (controller.signal.aborted) return;
      store.setPendingStep(runId, step.id);
      const rawOutcome = await executeResearchStep(step, { turnId: runId });
      const { outcome, nextId } = assignStepEvidenceIds(rawOutcome, nextEvidenceId);
      nextEvidenceId = nextId;
      store.appendStepResult(runId, outcome);
    }
    if (controller.signal.aborted) return;

    // --- 3. Synthesize ------------------------------------------------
    store.setStatus(runId, 'synthesizing');
    const stepResults = useDeepResearchStore.getState().runs[runId]?.stepResults ?? [];
    const allEvidence = stepResults.flatMap((outcome) => outcome.evidence);

    if (allEvidence.length === 0) {
      store.setReport(runId, {
        summary:
          'No evidence was collected from any planned source — every step was skipped or errored, so no cited report can be produced.',
        claims: [],
        openQuestions: [
          'Retry with a broader question, open a workspace folder, attach an indexed knowledge stack, or grant web-search permission when prompted.',
        ],
        droppedClaimCount: 0,
      });
      return;
    }

    const synthesisMessages = buildSynthesisMessages(run.question, stepResults);
    const synthesisAttempt = await attemptStream(
      target,
      synthesisMessages,
      [],
      controller.signal,
      undefined,
      runId,
      undefined,
      false,
    );
    if (controller.signal.aborted) return;
    if (synthesisAttempt.streamError !== null) {
      store.setError(runId, synthesisAttempt.streamError);
      return;
    }
    const report = parseReportResponse(
      synthesisAttempt.content,
      allEvidence.map((snippet) => snippet.id),
    );
    store.setReport(runId, report);
  } catch (err) {
    store.setError(runId, errorMessage(err));
  } finally {
    controllers.delete(runId);
  }
}
