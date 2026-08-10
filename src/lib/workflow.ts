/**
 * The `workflow` tool's runner — the named, phased big brother of a plain
 * parallel `task` round. `executeToolCall` (`turnEngine.ts`) intercepts
 * `name === 'workflow'` exactly like `task` and delegates here; this module
 * validates the model-supplied spec, then drives phases SEQUENTIALLY with
 * each phase's agents running in parallel (bounded by
 * `settingsStore.maxConcurrentSubagents`), every agent being a normal
 * `runSubagentTask` child — same cancellation, permission, checkpoint,
 * usage-accounting and persistence story as a lone `task` call.
 *
 * Deliberately declarative (a JSON spec, not a script): the model describes
 * phases and agents, the app owns the control flow. No model-authored code
 * ever executes, so there is nothing to sandbox — the trust boundary stays
 * exactly where `task` already put it.
 *
 * Depth cap: `WORKFLOW_TOOL` is never offered to a child loop
 * (`toolsForProfile` — same structural guarantee as `task`), and the
 * `executeToolCall` branch additionally requires the parent turn's
 * `SubagentContext`, which `runSubagentTask`'s own child dispatch never
 * configures.
 */
import { stringifyToolError, stringifyToolResult, CANCELLED_TOOL_RESULT } from './turnEngine';
import { MAX_REPORT_CHARS, runSubagentTask, type RunSubagentTaskParams } from './subagent';
import { useWorkflowStore, type WorkflowPhase } from '../store/workflowStore';
import { useSubagentStore } from '../store/subagentStore';
import { useSessionStore, type WorkflowAgentResult } from '../store/sessionStore';
import { useSettingsStore } from '../store/settingsStore';
import { useSavedWorkflowStore, type SavedWorkflow } from '../store/savedWorkflowStore';
import { unwrapUntrustedContent } from './untrustedContent';

export interface WorkflowAgentSpec {
  description: string;
  prompt: string;
  /** `'explore'`, `'code'`, or a custom agent name — validated at dispatch
   * by `runSubagentTask`'s `resolveSubagentProfile`, same as the `task`
   * tool's own `profile`. */
  profile: string;
  /** Optional per-agent reasoning-effort override, threaded straight to
   * `runSubagentTask`'s existing `effort` param — absent means "inherit the
   * parent turn's effort", exactly like v1 behaved. Model overrides were
   * deliberately NOT added: `SubagentContext.target` is resolved once per
   * turn and passed down so a mid-turn switch can never split parent and
   * child across targets (see its doc comment), and per-profile pinning +
   * dispatch policies already cover per-agent model choice inside
   * `resolveSubagentTarget`. */
  effort?: 'low' | 'medium' | 'high';
  /** Optional worktree isolation — same meaning and validation as the `task`
   * tool's `isolation` (see `RunSubagentTaskParams.isolation`). */
  isolation?: 'worktree';
}

export interface WorkflowSpec {
  name: string;
  description: string;
  phases: { title: string; agents: WorkflowAgentSpec[] }[];
}

/** Caps mirror the tool schema's documented limits — validated here anyway
 * rather than trusting the model's JSON, same posture as every other
 * frontend-validated tool argument. Small on purpose: a workflow is one
 * turn's orchestration, not a batch platform. */
export const MAX_WORKFLOW_PHASES = 6;
export const MAX_AGENTS_PER_PHASE = 6;
export const MAX_WORKFLOW_AGENTS = 16;

/** Per-injected-report cap for the prior-phase context block (chars) — a
 * later phase sees earlier findings, but a chatty phase must not blow out
 * every subsequent agent's prompt. */
const MAX_INJECTED_REPORT_CHARS = 2_000;

/**
 * Parses and validates the raw `workflow` tool arguments into a
 * `WorkflowSpec`, or throws with a message the model can act on. Exported
 * for the DOM-free logic tests.
 */
export function parseWorkflowSpec(args: Record<string, unknown>): WorkflowSpec {
  const name = typeof args.name === 'string' && args.name.trim().length > 0 ? args.name.trim() : null;
  if (!name) throw new Error('workflow requires a non-empty "name".');
  const description = typeof args.description === 'string' ? args.description.trim() : '';
  if (!Array.isArray(args.phases) || args.phases.length === 0) {
    throw new Error('workflow requires a non-empty "phases" array.');
  }
  if (args.phases.length > MAX_WORKFLOW_PHASES) {
    throw new Error(`workflow allows at most ${MAX_WORKFLOW_PHASES} phases.`);
  }
  let totalAgents = 0;
  const phases = args.phases.map((rawPhase, phaseIndex) => {
    const phase = (rawPhase ?? {}) as Record<string, unknown>;
    const title = typeof phase.title === 'string' && phase.title.trim().length > 0 ? phase.title.trim() : `Phase ${phaseIndex + 1}`;
    if (!Array.isArray(phase.agents) || phase.agents.length === 0) {
      throw new Error(`workflow phase "${title}" requires a non-empty "agents" array.`);
    }
    if (phase.agents.length > MAX_AGENTS_PER_PHASE) {
      throw new Error(`workflow phase "${title}" allows at most ${MAX_AGENTS_PER_PHASE} agents.`);
    }
    const agents = phase.agents.map((rawAgent, agentIndex) => {
      const agent = (rawAgent ?? {}) as Record<string, unknown>;
      const agentDescription =
        typeof agent.description === 'string' && agent.description.trim().length > 0 ? agent.description.trim() : `${title} agent ${agentIndex + 1}`;
      const prompt = typeof agent.prompt === 'string' && agent.prompt.trim().length > 0 ? agent.prompt : null;
      if (!prompt) throw new Error(`workflow agent "${agentDescription}" requires a non-empty "prompt".`);
      // Any non-empty string passes parse — an unknown profile fails at
      // dispatch with a per-agent tool error naming the known profiles
      // (see `resolveSubagentProfile`), which beats silently coercing a
      // typo to 'explore'.
      const profile = typeof agent.profile === 'string' && agent.profile.trim().length > 0 ? agent.profile.trim() : 'explore';
      const rawEffort = agent.effort;
      const effort: 'low' | 'medium' | 'high' | undefined =
        rawEffort === 'low' || rawEffort === 'medium' || rawEffort === 'high' ? rawEffort : undefined;
      const isolation: 'worktree' | undefined = agent.isolation === 'worktree' ? 'worktree' : undefined;
      return { description: agentDescription, prompt, profile, effort, isolation };
    });
    totalAgents += agents.length;
    return { title, agents };
  });
  if (totalAgents > MAX_WORKFLOW_AGENTS) {
    throw new Error(`workflow allows at most ${MAX_WORKFLOW_AGENTS} agents in total.`);
  }
  return { name, description, phases };
}

/**
 * Resolves the raw `workflow` tool arguments to a spec: a `saved` name (with
 * `phases` omitted) looks up the previously saved spec by name — an unknown
 * name throws a message listing what IS saved, so the model can recover —
 * and anything else falls through to `parseWorkflowSpec` unchanged. The
 * `executeToolCall` `workflow` branch calls this instead of
 * `parseWorkflowSpec` directly. Exported for the logic tests.
 */
export function resolveWorkflowSpec(args: Record<string, unknown>): WorkflowSpec {
  const saved = typeof args.saved === 'string' ? args.saved.trim() : '';
  if (saved && !Array.isArray(args.phases)) {
    const entry = useSavedWorkflowStore.getState().workflows[saved];
    if (!entry) {
      const names = Object.keys(useSavedWorkflowStore.getState().workflows).sort();
      throw new Error(
        names.length > 0
          ? `No saved workflow named "${saved}". Saved workflows: ${names.join(', ')}.`
          : `No saved workflow named "${saved}" — nothing has been saved yet.`,
      );
    }
    return entry.spec;
  }
  return parseWorkflowSpec(args);
}

/**
 * Renders the saved-workflow catalog for the system prompt — the `workflow`
 * counterpart of `skills.ts`'s `composeSkillCatalog`, and appended by
 * `agentLoop.ts` under the same `subagentsEnabled` gate that offers
 * `WORKFLOW_TOOL` itself. Empty string when nothing is saved, so the
 * `.filter(Boolean)` section join drops it entirely.
 */
export function composeSavedWorkflowCatalog(saved: SavedWorkflow[]): string {
  if (saved.length === 0) return '';
  return [
    '## Saved workflows',
    'Call the `workflow` tool with `{"saved": "<name>"}` (omitting "phases") to re-run one of these previously saved workflows:',
    ...saved.map((entry) => {
      const agents = entry.spec.phases.reduce((sum, phase) => sum + phase.agents.length, 0);
      const description = entry.spec.description.length > 0 ? entry.spec.description : 'no description';
      return `- ${entry.spec.name} — ${description} (${entry.spec.phases.length} phases, ${agents} agents)`;
    }),
  ].join('\n');
}

/** The `subagentStore` key for one workflow agent — deterministic from the
 * originating tool_call id + position, so a restarted session's persisted
 * `WorkflowRunMeta.phases[].agents[].taskId` entries still point at the
 * right `ChatSession.subagentRunMeta` entries. Exported for the logic
 * tests. */
export function workflowAgentTaskId(runId: string, phaseIndex: number, agentIndex: number): string {
  return `${runId}#p${phaseIndex}a${agentIndex}`;
}

/** Stable, dependency-free hash of one agent's FULL composed prompt (spec
 * prompt + injected prior-phase context) — the per-agent "spec still
 * matches" test `resume` relies on. FNV-1a 32-bit: not cryptographic on
 * purpose (nothing adversarial hashes here — a collision merely replays a
 * stale report), chosen because it is synchronous and four lines. Exported
 * for the logic tests. */
export function promptHash(prompt: string): string {
  let hash = 0x811c9dc5;
  for (let i = 0; i < prompt.length; i++) {
    hash ^= prompt.charCodeAt(i);
    hash = Math.imul(hash, 0x01000193);
  }
  return (hash >>> 0).toString(16).padStart(8, '0');
}

/** One finished agent's contribution to later phases' prompts. */
interface PhaseReport {
  phaseTitle: string;
  agentDescription: string;
  report: string;
}

/** Renders earlier phases' findings as a context block appended to a later
 * phase's agent prompts — the declarative stand-in for a script passing
 * stage results forward. Exported for the logic tests. */
export function buildPriorReportsBlock(reports: PhaseReport[]): string {
  if (reports.length === 0) return '';
  const sections = reports.map((entry) => {
    const capped =
      entry.report.length > MAX_INJECTED_REPORT_CHARS
        ? `${entry.report.slice(0, MAX_INJECTED_REPORT_CHARS)}\n[truncated]`
        : entry.report;
    return `### ${entry.phaseTitle} — ${entry.agentDescription}\n${capped}`;
  });
  return `\n\n## Results from earlier phases of this workflow\n\n${sections.join('\n\n')}`;
}

/** True when a `runSubagentTask` return value is an error payload rather
 * than a report — mirrors how the parent model reads it. */
function resultIsError(result: string): boolean {
  try {
    const parsed: unknown = JSON.parse(unwrapUntrustedContent(result));
    return typeof parsed === 'object' && parsed !== null && 'error' in parsed;
  } catch {
    return false;
  }
}

/** Runs `thunks` with at most `limit` in flight — the same bounded fan-out
 * `runToolCallsForRound` gives parallel `task` calls, local to one phase.
 * Results keep input order. */
async function runBounded<T>(thunks: Array<() => Promise<T>>, limit: number): Promise<T[]> {
  const results: T[] = new Array(thunks.length);
  let next = 0;
  const workers = Array.from({ length: Math.max(1, Math.min(limit, thunks.length)) }, async () => {
    while (next < thunks.length) {
      const index = next++;
      results[index] = await thunks[index]();
    }
  });
  await Promise.all(workers);
  return results;
}

/** Everything `runWorkflow` needs beyond the spec — the same parent-turn
 * pass-throughs `executeToolCall`'s `task` branch hands `runSubagentTask`,
 * minus the per-agent fields this module derives itself. */
export interface RunWorkflowParams {
  sessionId: string;
  runId?: string;
  parentCheckpointId: string | null;
  parentSignal?: AbortSignal;
  /** The originating `workflow` tool_call's own id — the
   * `workflowStore`/`ChatSession.workflowRunMeta` key, exactly like
   * `RunSubagentTaskParams.toolCallId`. */
  toolCallId: string;
  spec: WorkflowSpec;
  /** An earlier `workflow` call's own runId (toolCallId) to resume from —
   * `done` agents whose journaled `promptHash` still matches replay their
   * report instantly; failed/cancelled/missing ones re-dispatch. Best-effort:
   * an unknown id, or one whose run fully succeeded, simply runs everything
   * fresh rather than erroring — resume is an optimization, not a gate. */
  resume?: string;
  target: RunSubagentTaskParams['target'];
  effort?: string;
  risk?: RunSubagentTaskParams['risk'];
  onRoutingDecision?: RunSubagentTaskParams['onRoutingDecision'];
  onMutatedPath?: RunSubagentTaskParams['onMutatedPath'];
  onMutationFailure?: RunSubagentTaskParams['onMutationFailure'];
}

/**
 * Drives one workflow run to completion and returns the string to use as
 * the parent's `workflow` tool result. Same never-throws contract as
 * `runSubagentTask`: every outcome — reports, per-agent failures,
 * cancellation — comes back as a result payload the parent model can read.
 */
export async function runWorkflow(params: RunWorkflowParams): Promise<string> {
  const { sessionId, runId, parentCheckpointId, parentSignal, toolCallId, spec, resume, target, effort, risk, onRoutingDecision, onMutatedPath, onMutationFailure } =
    params;

  // The journal of the run being resumed — only a TERMINAL-WITH-FAILURES
  // run's entries are consulted (a fully-'done' run has nothing to resume,
  // and re-running it fresh is the least surprising reading of the call).
  const resumeJournal: Record<string, WorkflowAgentResult> | undefined = (() => {
    if (!resume) return undefined;
    const meta = useSessionStore.getState().sessions.find((s) => s.id === sessionId)?.workflowRunMeta?.[resume];
    if (!meta || meta.status === 'done') return undefined;
    return meta.agentResults;
  })();

  // This run's own journal, keyed by the NEW run's deterministic taskIds and
  // snapshotted with the terminal meta in `finish` — which is what makes a
  // chain of partial resumes work: each resume writes a fresh, complete
  // journal of its own.
  const agentResults: Record<string, WorkflowAgentResult> = {};

  const phases: WorkflowPhase[] = spec.phases.map((phase, phaseIndex) => ({
    title: phase.title,
    agents: phase.agents.map((agent, agentIndex) => ({
      taskId: workflowAgentTaskId(toolCallId, phaseIndex, agentIndex),
      description: agent.description,
    })),
  }));
  useWorkflowStore.getState().start({ sessionId, runId: toolCallId, name: spec.name, description: spec.description, phases, spec });

  /** Single exit point — mirrors `runSubagentTask`'s own `finish` helper:
   * marks the run terminal in the live store and snapshots the shape into
   * `ChatSession.workflowRunMeta` so the drawer keeps rendering it after a
   * restart (the agents' own stats persist via each child's existing
   * `setSubagentRun` call). */
  const finish = (status: 'done' | 'error' | 'cancelled', result: string): string => {
    useWorkflowStore.getState().finish(toolCallId, status);
    const live = useWorkflowStore.getState().runs[toolCallId];
    useSessionStore.getState().setWorkflowRun(sessionId, toolCallId, {
      name: spec.name,
      description: spec.description,
      status,
      startedAt: live?.startedAt ?? Date.now(),
      finishedAt: Date.now(),
      phases,
      agentResults,
    });
    return result;
  };

  /** Registers a replayed (journal-hit) agent in the live store AND the
   * persisted per-agent transcript, so the drawer's workflow card shows a
   * truthful 'done' row with the reused report — not a forever-"Queued" dot
   * for an agent that will never dispatch. `cancelId: ""` — nothing to stop
   * or steer, same convention as restored runs. */
  const replayJournaledAgent = (taskId: string, agent: WorkflowAgentSpec, prompt: string, report: string): void => {
    const now = Date.now();
    useSubagentStore.getState().start({ sessionId, taskId, cancelId: '', workflowRunId: toolCallId, description: agent.description, profile: agent.profile });
    useSubagentStore.getState().appendMessage(taskId, { role: 'assistant', content: report });
    useSubagentStore.getState().finish(taskId, 'done');
    useSessionStore.getState().setSubagentRun(
      sessionId,
      taskId,
      [
        { role: 'user', content: prompt },
        { role: 'assistant', content: report },
      ],
      {
        status: 'done',
        workflowRunId: toolCallId,
        description: agent.description,
        profile: agent.profile,
        startedAt: now,
        finishedAt: now,
        toolCallCount: 0,
      },
    );
  };

  try {
    const priorReports: PhaseReport[] = [];
    const phaseSummaries: { title: string; agents: { description: string; status: string; report: string; reused?: boolean }[] }[] = [];

    for (let phaseIndex = 0; phaseIndex < spec.phases.length; phaseIndex++) {
      if (parentSignal?.aborted) return finish('cancelled', CANCELLED_TOOL_RESULT);
      useWorkflowStore.getState().beginPhase(toolCallId, phaseIndex);
      const phase = spec.phases[phaseIndex];
      const contextBlock = buildPriorReportsBlock(priorReports);

      const limit = useSettingsStore.getState().maxConcurrentSubagents;
      const results = await runBounded(
        phase.agents.map((agent, agentIndex) => async () => {
          const agentTaskId = workflowAgentTaskId(toolCallId, phaseIndex, agentIndex);
          const prompt = `${agent.prompt}${contextBlock}`;
          const hash = promptHash(prompt);

          // Resume hit: the same position in the resumed run completed with
          // the exact same composed prompt — replay its report instantly. A
          // later phase whose context CHANGED (because a re-run agent now
          // contributes a report the original run lacked) hashes differently
          // and correctly re-dispatches.
          const journaled = resume ? resumeJournal?.[workflowAgentTaskId(resume, phaseIndex, agentIndex)] : undefined;
          if (journaled && journaled.status === 'done' && journaled.promptHash === hash) {
            replayJournaledAgent(agentTaskId, agent, prompt, journaled.report);
            agentResults[agentTaskId] = { promptHash: hash, status: 'done', report: journaled.report, reused: true };
            return journaled.report;
          }

          const raw = await runSubagentTask({
            sessionId,
            runId,
            parentCheckpointId,
            parentSignal,
            taskId: crypto.randomUUID(),
            toolCallId: agentTaskId,
            workflowRunId: toolCallId,
            description: agent.description,
            prompt,
            profile: agent.profile,
            isolation: agent.isolation,
            target,
            effort: agent.effort ?? effort,
            risk,
            onRoutingDecision,
            onMutatedPath,
            onMutationFailure,
          });
          const cancelled = unwrapUntrustedContent(raw) === CANCELLED_TOOL_RESULT;
          const failed = !cancelled && resultIsError(raw);
          agentResults[agentTaskId] = {
            promptHash: hash,
            status: cancelled ? 'cancelled' : failed ? 'error' : 'done',
            report: raw.slice(0, MAX_REPORT_CHARS),
          };
          return raw;
        }),
        limit,
      );

      const agents = phase.agents.map((agent, agentIndex) => {
        const raw = results[agentIndex];
        const cancelled = unwrapUntrustedContent(raw) === CANCELLED_TOOL_RESULT;
        const failed = !cancelled && resultIsError(raw);
        if (!cancelled && !failed) {
          priorReports.push({ phaseTitle: phase.title, agentDescription: agent.description, report: raw });
        }
        const reused = agentResults[workflowAgentTaskId(toolCallId, phaseIndex, agentIndex)]?.reused === true;
        return {
          description: agent.description,
          status: cancelled ? 'cancelled' : failed ? 'error' : 'done',
          report: raw,
          ...(reused ? { reused: true } : {}),
        };
      });
      phaseSummaries.push({ title: phase.title, agents });

      if (parentSignal?.aborted) {
        return finish('cancelled', CANCELLED_TOOL_RESULT);
      }
    }

    const anyFailure = phaseSummaries.some((phase) => phase.agents.some((agent) => agent.status !== 'done'));
    // Every fully-successful run saves its spec under its name (last-run-wins)
    // — what makes `resolveWorkflowSpec`'s `saved` lookup and the drawer's
    // Saved-workflows list work without the user ever pressing Save.
    if (!anyFailure) useSavedWorkflowStore.getState().upsert(spec, Date.now());
    return finish(
      anyFailure ? 'error' : 'done',
      stringifyToolResult({ workflow: spec.name, status: anyFailure ? 'completed_with_failures' : 'completed', phases: phaseSummaries }),
    );
  } catch (err) {
    return finish('error', stringifyToolError(err));
  }
}
