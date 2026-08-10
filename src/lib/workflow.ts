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
import { runSubagentTask, type RunSubagentTaskParams } from './subagent';
import { useWorkflowStore, type WorkflowPhase } from '../store/workflowStore';
import { useSessionStore } from '../store/sessionStore';
import { useSettingsStore } from '../store/settingsStore';
import { useSavedWorkflowStore, type SavedWorkflow } from '../store/savedWorkflowStore';
import { unwrapUntrustedContent } from './untrustedContent';

export interface WorkflowAgentSpec {
  description: string;
  prompt: string;
  profile: 'explore' | 'code';
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
      const profile: 'explore' | 'code' = agent.profile === 'code' ? 'code' : 'explore';
      return { description: agentDescription, prompt, profile };
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
  const { sessionId, runId, parentCheckpointId, parentSignal, toolCallId, spec, target, effort, risk, onRoutingDecision, onMutatedPath, onMutationFailure } =
    params;

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
    });
    return result;
  };

  try {
    const priorReports: PhaseReport[] = [];
    const phaseSummaries: { title: string; agents: { description: string; status: string; report: string }[] }[] = [];

    for (let phaseIndex = 0; phaseIndex < spec.phases.length; phaseIndex++) {
      if (parentSignal?.aborted) return finish('cancelled', CANCELLED_TOOL_RESULT);
      useWorkflowStore.getState().beginPhase(toolCallId, phaseIndex);
      const phase = spec.phases[phaseIndex];
      const contextBlock = buildPriorReportsBlock(priorReports);

      const limit = useSettingsStore.getState().maxConcurrentSubagents;
      const results = await runBounded(
        phase.agents.map((agent, agentIndex) => () =>
          runSubagentTask({
            sessionId,
            runId,
            parentCheckpointId,
            parentSignal,
            taskId: crypto.randomUUID(),
            toolCallId: workflowAgentTaskId(toolCallId, phaseIndex, agentIndex),
            workflowRunId: toolCallId,
            description: agent.description,
            prompt: `${agent.prompt}${contextBlock}`,
            profile: agent.profile,
            target,
            effort,
            risk,
            onRoutingDecision,
            onMutatedPath,
            onMutationFailure,
          }),
        ),
        limit,
      );

      const agents = phase.agents.map((agent, agentIndex) => {
        const raw = results[agentIndex];
        const cancelled = unwrapUntrustedContent(raw) === CANCELLED_TOOL_RESULT;
        const failed = !cancelled && resultIsError(raw);
        if (!cancelled && !failed) {
          priorReports.push({ phaseTitle: phase.title, agentDescription: agent.description, report: raw });
        }
        return { description: agent.description, status: cancelled ? 'cancelled' : failed ? 'error' : 'done', report: raw };
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
