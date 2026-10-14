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
import { flushSessionPersistence, useSessionStore, type WorkflowAgentResult } from '../store/sessionStore';
import { useSettingsStore } from '../store/settingsStore';
import { useSavedWorkflowStore, type SavedWorkflow } from '../store/savedWorkflowStore';
import { unwrapUntrustedContent, wrapUntrustedContent } from './untrustedContent';
import { buildSwarmPreset } from './swarmPresets';
import { buildGoalSwarm, parseSwarmMemoryStackIds, selectSwarmDomain } from './swarmPlanning';
import { recordSpecialistMetric, loadSpecialistMetrics } from './swarmSpecialistMetrics';
import { parseSwarmTopology, planCoordinationWaves, nextAdaptiveCoordinationWave, formatWaveMessages, type SwarmTopology } from './swarmCoordination';
import { retrieveSwarmKnowledge } from './swarmKnowledge';
import { compileSwarmTaskGraph } from './swarmGraph';
import { assertReadOnlyPeerPlacement } from './swarmPeerSchema';
import { reconcileSwarmReviewers,parseSwarmVotePolicy,type SwarmVotePolicy } from './swarmConsensus';
import { approvedSwarmNotebookBlock, stageSwarmNote } from './swarmNotebook';
import { approvedLearningContext } from './swarmLearningCycle';
import { buildSwarmTrajectoryContext } from './swarmTrajectoryMemory';
import {frozenAuthenticatedPeerVoters,revalidateAuthenticatedPeerVoters} from './swarmPeerQuorum';
import { LiveSwarmBus } from './swarmLiveBus';
import { captureAutoMemory, retrieveAutoMemory, validateAutoMemoryStack, type AutoMemoryResult } from './swarmAutoMemory';

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
  /** Explicitly selected paired peer; only allowed for read-only explore agents. */
  peer?: string;
  /** Health-ranked candidates supplied explicitly by the operator. */
  peerCandidates?: string[];
  /** One opt-in read-only alternate only after verified terminal peer failure. */
  peerFailover?: boolean;
}

export interface WorkflowSpec {
  name: string;
  description: string;
  phases: { title: string; agents: WorkflowAgentSpec[] }[];
  /** Optional read-only local coordination between agents in a phase. */
  topology?: SwarmTopology;
  /** Existing, indexed Knowledge 2.0 stacks explicitly selected for this run. */
  memoryStackIds?: string[];
  /** Do not run downstream phases after failed prerequisites. */
  stopOnFailure?: boolean;
  /** Optional strict non-authoritative agreement/dispute summary from independent reviewers. */
  consensus?: SwarmVotePolicy;
  /** Explicitly require the vote phase to come from unique paired peer device
   * identities under unchanged task grants and certificate fingerprints. */
  authenticatedPeerVoting?: boolean;
  /** Opt-in, operator-approved versioned notebook reference and staging. */
  notebook?: boolean;
  /** Explicit opt-in to operator-approved, domain-matched past learning. */
  learning?: boolean;
  /** Opt-in in-process collaboration. Remote agents are disallowed. */
  liveMessaging?: boolean;
  /** Opt-in semantic storage bound to an existing Knowledge embedding stack. */
  autoMemoryStackId?: string;
}

/** Caps mirror the tool schema's documented limits — validated here anyway
 * rather than trusting the model's JSON, same posture as every other
 * frontend-validated tool argument. Small on purpose: a workflow is one
 * turn's orchestration, not a batch platform. */
export const MAX_WORKFLOW_PHASES = 6;
export const MAX_AGENTS_PER_PHASE = 6;
export const MAX_WORKFLOW_AGENTS = 16;

/** Runner-side defense-in-depth for stored specs and programmatic callers
 * that bypass the workflow tool's parser. Reject oversized plans before any
 * agent can be dispatched, including after a restore from disk. */
export function assertWorkflowBounds(spec: WorkflowSpec): void {
  if (!spec || typeof spec.name !== 'string' || !spec.name.trim() ||
      !Array.isArray(spec.phases) || !spec.phases.length ||
      spec.phases.length > MAX_WORKFLOW_PHASES) {
    throw new Error('Workflow must have a name and 1–6 phases.');
  }
  let total = 0;
  for (const phase of spec.phases) {
    if (!phase || typeof phase.title !== 'string' ||
        !Array.isArray(phase.agents) || !phase.agents.length ||
        phase.agents.length > MAX_AGENTS_PER_PHASE) {
      throw new Error('Each workflow phase requires 1–6 agents.');
    }
    total += phase.agents.length;
    for (const agent of phase.agents) {
      if (!agent || typeof agent.description !== 'string' ||
          typeof agent.prompt !== 'string' || !agent.prompt.trim() ||
          agent.prompt.length > 8_000 || typeof agent.profile !== 'string') {
        throw new Error('Workflow agent has invalid or oversized instructions.');
      }
      if (agent.peerFailover !== undefined && typeof agent.peerFailover !== 'boolean')
        throw new Error('Peer failover flag must be a boolean.');
      if (agent.peerFailover && (agent.profile !== 'explore' || !agent.peer ||
         agent.isolation || !Array.isArray(agent.peerCandidates) ||
         agent.peerCandidates.length < 2 || agent.peerCandidates.length > 3 ||
         new Set(agent.peerCandidates).size !== agent.peerCandidates.length)) {
        throw new Error('Peer failover requires 2–3 distinct explicitly approved read-only peer candidates.');
      }
    }
  }
  if (total > MAX_WORKFLOW_AGENTS) throw new Error('Workflow cannot exceed 16 agents.');
  parseSwarmVotePolicy(spec.consensus);
  if(spec.authenticatedPeerVoting!==undefined&&typeof spec.authenticatedPeerVoting!=='boolean')
    throw new Error('Authenticated peer voting must be a boolean.');
  if(spec.authenticatedPeerVoting){
    if(!spec.consensus||spec.phases.length<2)
      throw new Error('Authenticated peer voting requires a reviewer consensus phase and a following verification phase.');
    const reviewers=spec.phases[spec.phases.length-2].agents;
    if(reviewers.length<2||reviewers.some(agent=>agent.profile!=='explore'||
       !agent.peer||agent.isolation||agent.peerFailover||agent.peerCandidates?.length)||
       new Set(reviewers.map(agent=>agent.peer)).size!==reviewers.length)
      throw new Error('Authenticated peer voting requires distinct pinned, read-only reviewer peers without placement fallback.');
  }
  const topology = parseSwarmTopology(spec.topology);
  if (topology !== 'phased' && spec.phases.some(phase => phase.agents.some(agent =>
    agent.profile !== 'explore' || agent.peer !== undefined || agent.peerCandidates?.length || agent.isolation))) {
    throw new Error('Non-phased coordination is local, read-only and cannot relay reports to remote peers or modifying agents.');
  }
  if (spec.memoryStackIds !== undefined) parseSwarmMemoryStackIds(spec.memoryStackIds);
  if (spec.liveMessaging !== undefined && typeof spec.liveMessaging !== 'boolean')
    throw new Error('Invalid live messaging flag.');
  if (spec.liveMessaging && spec.phases.some(p => p.agents.some(a => !!a.peer)))
    throw new Error('Live workflow messages are local only.');
  if (spec.autoMemoryStackId !== undefined) {
    validateAutoMemoryStack(spec.autoMemoryStackId);
    if (spec.phases.some(p => p.agents.some(a => !!a.peer)))
      throw new Error('Automatic local memory cannot be shared with remote agents.');
  }
  if (spec.learning !== undefined && typeof spec.learning !== 'boolean') throw new Error('Invalid learning flag.');
  if (spec.learning && spec.phases.some(phase=>phase.agents.some(agent=>agent.peer!==undefined || agent.profile!=='explore'))) {
    throw new Error('Operator-approved learning retrieval is local read-only only.');
  }
}

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
      if (agent.peer !== undefined) assertReadOnlyPeerPlacement(agent.peer, profile, Boolean(isolation));
      const peer = agent.peer as string | undefined;
      const peerCandidates = agent.peerCandidates;
      if (peerCandidates !== undefined && (!Array.isArray(peerCandidates) || peerCandidates.length === 0 ||
          peerCandidates.length > 3 || !peerCandidates.every((alias: unknown) => typeof alias === 'string' && alias.length <= 120) ||
          !peer || !peerCandidates.includes(peer))) {
        throw new Error('Peer candidate pool requires 1–3 aliases and a valid primary read-only peer.');
      }
      if (agent.peerFailover !== undefined && typeof agent.peerFailover !== 'boolean')
        throw new Error('peerFailover must be a boolean.');
      if (agent.peerFailover === true && (profile !== 'explore' || !peer ||
        !Array.isArray(peerCandidates) || peerCandidates.length < 2 ||
        new Set(peerCandidates).size !== peerCandidates.length)) {
        throw new Error('Peer failover requires 2–3 distinct explicitly approved read-only peer candidates.');
      }
      return { description: agentDescription, prompt, profile, effort, isolation,
        ...(peer ? { peer } : {}), ...(Array.isArray(peerCandidates) ? { peerCandidates: [...peerCandidates] as string[] } : {}),
        ...(agent.peerFailover === true ? { peerFailover: true } : {}) };
    });
    totalAgents += agents.length;
    return { title, agents };
  });
  if (totalAgents > MAX_WORKFLOW_AGENTS) {
    throw new Error(`workflow allows at most ${MAX_WORKFLOW_AGENTS} agents in total.`);
  }
  const topology = parseSwarmTopology(args.topology);
  const memoryStackIds = parseSwarmMemoryStackIds(args.memory_stack_ids);
  const consensus = parseSwarmVotePolicy(args.consensus);
  if (args.live_messaging !== undefined && typeof args.live_messaging !== 'boolean')
    throw new Error('live_messaging must be a boolean.');
  if (args.auto_memory_stack_id !== undefined && typeof args.auto_memory_stack_id !== 'string')
    throw new Error('auto_memory_stack_id must be a stack ID string.');
  if(args.authenticated_peer_voting!==undefined&&typeof args.authenticated_peer_voting!=='boolean')
    throw new Error('authenticated_peer_voting must be a boolean.');
  return { name, description, phases,
    ...(topology !== 'phased' ? { topology } : {}),
    ...(args.stop_on_failure === true ? { stopOnFailure: true } : {}),
    ...(consensus ? { consensus } : {}),

    ...(args.authenticated_peer_voting === true ? { authenticatedPeerVoting: true } : {}),
    ...(args.notebook === true ? { notebook: true } : {}),
    ...(args.learning === true ? { learning: true } : {}),
    ...(args.live_messaging === true ? { liveMessaging: true } : {}),
    ...(typeof args.auto_memory_stack_id === 'string'
      ? { autoMemoryStackId: validateAutoMemoryStack(args.auto_memory_stack_id) } : {}),
    ...(memoryStackIds.length ? { memoryStackIds } : {}) };
}

/**
 * Resolves the raw `workflow` tool arguments to a spec: a `saved` name (with
 * `phases` omitted) looks up the previously saved spec by name — an unknown
 * name throws a message listing what IS saved, so the model can recover —
 * and anything else falls through to `parseWorkflowSpec` unchanged. The
 * `executeToolCall` `workflow` branch calls this instead of
 * `parseWorkflowSpec` directly. Exported for the logic tests.
 */
function withLiveOptions(spec: WorkflowSpec, args: Record<string, unknown>): WorkflowSpec {
  if (args.live_messaging !== undefined && typeof args.live_messaging !== 'boolean')
    throw new Error('live_messaging must be a boolean.');
  if (args.auto_memory_stack_id !== undefined && typeof args.auto_memory_stack_id !== 'string')
    throw new Error('auto_memory_stack_id must be a stack ID string.');
  const augmented: WorkflowSpec = {
    ...spec,
    ...(args.live_messaging === true ? { liveMessaging: true } : {}),
    ...(typeof args.auto_memory_stack_id === 'string'
      ? { autoMemoryStackId: validateAutoMemoryStack(args.auto_memory_stack_id) } : {}),
  };
  assertWorkflowBounds(augmented);
  return augmented;
}
export function resolveWorkflowSpec(args: Record<string, unknown>, sessionId?: string): WorkflowSpec {
  // A resume-only tool invocation explicitly opts into recovering an
  // interrupted workflow. Never re-run one merely because it hydrated.
  if (typeof args.resume === 'string' && args.resume.trim() && sessionId &&
      args.goal === undefined && args.preset === undefined &&
      args.tasks === undefined && args.saved === undefined &&
      args.phases === undefined && args.name === undefined) {
    const meta = useSessionStore.getState().sessions.find(s => s.id === sessionId)
      ?.workflowRunMeta?.[args.resume.trim()];
    if (!meta?.spec) throw new Error('No recoverable workflow specification for this run in the current session.');
    return parseWorkflowSpec({
      ...meta.spec,
      memory_stack_ids: meta.spec.memoryStackIds,
      stop_on_failure: meta.spec.stopOnFailure,
      consensus: meta.spec.consensus,
      authenticated_peer_voting: meta.spec.authenticatedPeerVoting,
      notebook: meta.spec.notebook,
      learning: meta.spec.learning,
      topology: meta.spec.topology,
      live_messaging: meta.spec.liveMessaging,
      auto_memory_stack_id: meta.spec.autoMemoryStackId,
    });
  }
  if (args.goal !== undefined) return withLiveOptions(buildGoalSwarm(args), args);
  if (args.preset !== undefined) return withLiveOptions(buildSwarmPreset(args), args);
  if (args.tasks !== undefined) return withLiveOptions(compileSwarmTaskGraph(args), args);
  if (args.mode !== undefined || args.scope !== undefined) throw new Error('workflow mode and scope require a goal or preset.');
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
/** Persisted root identity of a workflow replay chain. Does not change
 * ordinary local checkpoints, only remote peer receipt correlation. */
export function workflowCorrelationOrigin(
  toolCallId: string,
  resume?: string,
  previous?: { correlationOrigin?: string },
): string {
  return previous?.correlationOrigin ?? resume ?? toolCallId;
}

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
    return `### ${entry.phaseTitle} — ${entry.agentDescription}\n${wrapUntrustedContent('prior workflow agent report', capped)}`;
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
  /** An earlier interrupted `workflow` tool-call ID in the same session.
   * Only completed, full-prompt-matching agents replay their reports. An
   * unknown, active or already completed run MUST fail closed: silently
   * treating it as new work could repeat dangerous external mutations. */
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

  const sourceMeta = resume
    ? useSessionStore.getState().sessions.find(s => s.id === sessionId)?.workflowRunMeta?.[resume]
    : undefined;
  if (resume && (!sourceMeta || (sourceMeta.status !== 'error' && sourceMeta.status !== 'cancelled'))) {
    return stringifyToolError(new Error(
      'Cannot resume: workflow run does not exist, is still active, or has already completed in this session. No new agents were dispatched.',
    ));
  }
  const resumeJournal: Record<string, WorkflowAgentResult> | undefined =
    resume ? sourceMeta?.agentResults : undefined;

  // This run's own journal, keyed by the NEW run's deterministic taskIds and
  // snapshotted with the terminal meta in `finish` — which is what makes a
  // chain of partial resumes work: each resume writes a fresh, complete
  // journal of its own.
  const agentResults: Record<string, WorkflowAgentResult> = {};
  let liveBus: LiveSwarmBus | null = null;
  // For a partial resume chain A -> B -> C, C must still attach to A's
  // durable peer task receipt. Using only the immediately previous run id
  // would mint a new correlation and could send duplicate remote work.
  const correlationOrigin = workflowCorrelationOrigin(toolCallId, resume, sourceMeta);

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
  const finish = async (status: 'done' | 'error' | 'cancelled', result: string): Promise<string> => {
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
      ...(liveBus ? { liveMailbox: liveBus.snapshot() } : {}),
      correlationOrigin,
      ...(runId ? { sourceRunId: runId } : {}),
      spec,
    });
    try {
      await flushSessionPersistence();
    } catch (err) {
      return stringifyToolError(new Error('Workflow completion could not be persisted: ' + String(err)));
    }
    return result;
  };

  /** Save a bounded restart journal before dispatch and after each agent.
   * Session persistence is file-backed but debounced, so forced power loss
   * during a write may still lose the last update; never replay mutations
   * automatically after a crash. */
  const checkpoint = async (): Promise<void> => {
    const live = useWorkflowStore.getState().runs[toolCallId];
    useSessionStore.getState().setWorkflowRun(sessionId, toolCallId, {
      name: spec.name, description: spec.description, status: 'running',
      startedAt: live?.startedAt ?? Date.now(), finishedAt: Date.now(),
      phases, agentResults: { ...agentResults },
      ...(liveBus ? { liveMailbox: liveBus.snapshot() } : {}),
      correlationOrigin, ...(runId ? { sourceRunId: runId } : {}), spec,
    });
    // Dependents cannot start until the last agent journal has been written.
    await flushSessionPersistence();
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
    assertWorkflowBounds(spec);
    await checkpoint(); // durably register the graph before dispatch
    if (spec.liveMessaging) {
      const workspaceRoot = useSessionStore.getState().sessions.find(s => s.id === sessionId)?.workspacePath ?? '';
      liveBus = new LiveSwarmBus(spec.phases.flatMap((phase, phaseIndex) =>
        phase.agents.map((_, agentIndex) => workflowAgentTaskId(toolCallId, phaseIndex, agentIndex))), workspaceRoot);
      if (resume) {
        if (!sourceMeta?.liveMailbox)
          throw new Error('Cannot resume live messaging: no persisted mailbox checkpoint.');
        liveBus.restore(sourceMeta.liveMailbox,
          sourceMeta.phases.flatMap(phase => phase.agents.map(agent => agent.taskId)));
        for (let pi = 0; pi < spec.phases.length; pi++)
          for (let ai = 0; ai < spec.phases[pi].agents.length; ai++) {
            const previous = sourceMeta.agentResults?.[workflowAgentTaskId(resume, pi, ai)];
            if (previous?.status !== 'done')
              liveBus.reopenAgent(workflowAgentTaskId(toolCallId, pi, ai));
          }
      }
      liveBus.setPersistence(async () => { await checkpoint(); });
      await checkpoint();
    }
    const voterSnapshot=spec.authenticatedPeerVoting
      ? await frozenAuthenticatedPeerVoters(spec.phases[spec.phases.length-2].agents
        .map(agent=>agent.peer!))
      : null;
    // Resolve one module instance per graph instead of racing concurrent
    // per-agent dynamic imports while a remote reviewer wave fans out.
    const remoteRunner=spec.phases.some(phase=>phase.agents.some(agent=>!!agent.peer))
      ? await import('./swarmPeer') : null;
    const failoverRunner=spec.phases.some(phase=>phase.agents.some(agent=>!!agent.peerFailover))
      ? await import('./swarmPeerRecovery') : null;
    // Persisted saved workflows can be hand-edited; validate the permission
    // boundary again at dispatch, not only during JSON-tool parsing.
    for (const phase of spec.phases) {
      for (const agent of phase.agents) {
        if (agent.peer !== undefined) assertReadOnlyPeerPlacement(agent.peer, agent.profile, Boolean(agent.isolation));
        if (agent.peerCandidates !== undefined && (
          !agent.peer || !Array.isArray(agent.peerCandidates) || agent.peerCandidates.length < 1 ||
          agent.peerCandidates.length > 3 ||
          !agent.peerCandidates.includes(agent.peer) ||
          agent.peerCandidates.some(candidate => {
            try { assertReadOnlyPeerPlacement(candidate, agent.profile, Boolean(agent.isolation)); return false; }
            catch { return true; }
          })
        )) throw new Error('Saved workflow has invalid read-only peer candidates.');
      }
    }
    if ((spec.memoryStackIds?.length || spec.notebook) && spec.phases.some(phase => phase.agents.some(agent => agent.peer))) {
      throw new Error('Indexed knowledge cannot be automatically forwarded to remote peers. Remove memory_stack_ids for a peer workflow.');
    }
    const currentSession = useSessionStore.getState().sessions.find(s => s.id === sessionId);
    const memoryWorkspace = currentSession?.workspacePath?.trim() || 'session:' + sessionId;
    const autoMemoryBlock = spec.autoMemoryStackId
      ? await retrieveAutoMemory(spec.autoMemoryStackId, memoryWorkspace, spec.description || spec.name)
      : '';
    const knowledgeBlock = spec.memoryStackIds?.length
      ? await retrieveSwarmKnowledge(spec.memoryStackIds, spec.description)
      : '';
    const session = useSessionStore.getState().sessions.find(s => s.id === sessionId);
    const notebookProject = session?.workspacePath?.trim() || 'session:' + sessionId;
    const notebookBlock = spec.notebook ? approvedSwarmNotebookBlock(notebookProject) : '';
    const learningBlock = spec.learning
      ? approvedLearningContext(selectSwarmDomain(spec.description)) +
        buildSwarmTrajectoryContext(
          useSessionStore.getState().sessions, selectSwarmDomain(spec.description),
          notebookProject, sessionId,
        )
      : '';
    const priorReports: PhaseReport[] = [];
    const phaseSummaries: { title: string; agents: { description: string; status: string; report: string; reused?: boolean }[] }[] = [];

    for (let phaseIndex = 0; phaseIndex < spec.phases.length; phaseIndex++) {
      if (parentSignal?.aborted) return finish('cancelled', CANCELLED_TOOL_RESULT);
      useWorkflowStore.getState().beginPhase(toolCallId, phaseIndex);
      const phase = spec.phases[phaseIndex];
      const contextBlock = buildPriorReportsBlock(priorReports);
      const sharedContext = [knowledgeBlock, autoMemoryBlock, notebookBlock, learningBlock].filter(Boolean).map(x => '\n\n' + x).join('');

      const limit = useSettingsStore.getState().maxConcurrentSubagents;
      // A topology is a scheduling policy, never a new agent/permission grant.
      // A failed coordinator can be bypassed by later read-only waves,
      // but modifying workflows cannot use these policies.
      const stats = loadSpecialistMetrics();
      const domain = selectSwarmDomain(spec.description);
      const waves = planCoordinationWaves(spec.topology ?? 'phased', phase.agents.length,
        phase.agents.map(agent => {
          const observation = stats[domain + ':' + agent.description];
          return {completions: observation?.successes ?? 0, failures: observation?.failures ?? 0};
        }));
      const results: string[] = new Array(phase.agents.length);
      const waveMessages: {sender: string; report: string}[] = [];
      // Adaptive read-only teams elect a replacement on a failed report,
      // or fan out the still-pending agents after one coordinator succeeds.
      // Never repeat a failed or completed agent.
      const adaptivePending = waves.topology === 'adaptive' ? waves.waves.flat() : [];
      let coordinatorReady = false;
      for (let waveIndex = 0; waveIndex < waves.waves.length; waveIndex++) {
        const wave = waves.topology === 'adaptive'
          ? nextAdaptiveCoordinationWave(adaptivePending, coordinatorReady)
          : waves.waves[waveIndex];
        if (!wave.length) break;
        if (waves.topology === 'adaptive') adaptivePending.splice(0, wave.length);
        const completed = await runBounded(
          wave.map(agentIndex => {
            const agent = phase.agents[agentIndex];
            return async () => {
          const startedAt = Date.now();
          const agentTaskId = workflowAgentTaskId(toolCallId, phaseIndex, agentIndex);
          const prompt = `${agent.prompt}${contextBlock}${sharedContext}${formatWaveMessages(waveMessages, agent.description, waves.topology)}`;
          const hash = promptHash(prompt);

          // Resume hit: the same position in the resumed run completed with
          // the exact same composed prompt — replay its report instantly. A
          // later phase whose context CHANGED (because a re-run agent now
          // contributes a report the original run lacked) hashes differently
          // and correctly re-dispatches.
          const journaled = resume ? resumeJournal?.[workflowAgentTaskId(resume, phaseIndex, agentIndex)] : undefined;
          if (journaled && journaled.status === 'done' && journaled.promptHash === hash) {
            replayJournaledAgent(agentTaskId, agent, prompt, journaled.report);
            if (liveBus) await liveBus.finishAgentDurable(agentTaskId);
            agentResults[agentTaskId] = { promptHash: hash, status: 'done', report: journaled.report, reused: true };
            await checkpoint();
            return journaled.report;
          }

          const raw = agent.peer
            ? await (agent.peerFailover
                ? failoverRunner!.runPairedPeerAgentWithFailover
                : remoteRunner!.runPairedPeerAgent)({
                sessionId,
                workflowRunId: toolCallId,
                taskId: agentTaskId,
                description: agent.description,
                prompt,
                peer: agent.peer,
                peerCandidates: agent.peerCandidates,
                correlationRunId: correlationOrigin,
                correlationTaskPosition: `p${phaseIndex}a${agentIndex}`,
                signal: parentSignal,
              })
            : await runSubagentTask({
            liveBus: liveBus ?? undefined,
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
          if (liveBus) {
            if (cancelled || failed) await liveBus.suspendAgentDurable(agentTaskId);
            else await liveBus.finishAgentDurable(agentTaskId);
          }
          agentResults[agentTaskId] = {
            promptHash: hash,
            status: cancelled ? 'cancelled' : failed ? 'error' : 'done',
            report: raw.slice(0, MAX_REPORT_CHARS),
          };
          await checkpoint();
          if (phaseIndex === 0 && spec.name.startsWith('auto-swarm-') && !cancelled) {
            recordSpecialistMetric(selectSwarmDomain(spec.description), agent.description, !failed, Date.now() - startedAt);
          }
          return raw;
          };
        }),
        limit,
      );
        wave.forEach((index, ordinal) => {
          const report = completed[ordinal];
          results[index] = report;
          if (report && unwrapUntrustedContent(report) !== CANCELLED_TOOL_RESULT && !resultIsError(report)) {
            waveMessages.push({sender: phase.agents[index].description, report});
            if (waves.topology === 'adaptive' && wave.length === 1) coordinatorReady = true;
          }
        });
      }

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

      // In dependency-aware workflows and modifying swarms, an incomplete
      // prerequisite must never unlock a downstream agent. Earlier workflow
      // specs preserve their previous best-effort continuation semantics.
      if (parentSignal?.aborted) {
        return finish('cancelled', CANCELLED_TOOL_RESULT);
      }
      if (spec.stopOnFailure && agents.some(agent => agent.status !== 'done')) break;
    }

    const anyFailure = phaseSummaries.some((phase) => phase.agents.some((agent) => agent.status !== 'done'));
    // Every fully-successful run saves its spec under its name (last-run-wins)
    // — what makes `resolveWorkflowSpec`'s `saved` lookup and the drawer's
    // Saved-workflows list work without the user ever pressing Save.
    if (!anyFailure) useSavedWorkflowStore.getState().upsert(spec, Date.now());
    const reviewPhase = spec.consensus && phaseSummaries.length >= 2
      ? phaseSummaries[phaseSummaries.length - 2] : null;
    // Revalidate the native pairing registry before a remote vote becomes a
    // notebook candidate. Revocation or certificate rotation invalidates the
    // quorum even when old report text looks convincing.
    if(voterSnapshot)await revalidateAuthenticatedPeerVoters(voterSnapshot);
    const consensus = reviewPhase ? reconcileSwarmReviewers(reviewPhase.agents
      .flatMap((agent,index)=>agent.status==='done'?[{
        reviewer:voterSnapshot ? 'paired:'+voterSnapshot[index].certificateSha256 : agent.description,
        report:agent.report,
      }]:[]),
      spec.consensus ?? 'strict', reviewPhase.agents.length) : undefined;
    const notebookStaged: string[] = [];
    const notebookWarnings: string[] = [];
    if (spec.notebook && consensus && !anyFailure) {
      for (const claim of consensus.claims) {
        if (claim.status !== 'reviewer_agreement' || !claim.evidence.length) continue;
        try {
          stageSwarmNote({ project: notebookProject, id: claim.id,
            evidence: claim.evidence.slice(0, 6), reviewers: claim.supportingReviewers,
            runId: correlationOrigin });
          notebookStaged.push(claim.id);
        } catch (error) {
          notebookWarnings.push('Failed to stage claim ' + claim.id + ': ' + String(error).slice(0, 200));
        }
      }
    }
    let automaticMemory: AutoMemoryResult | undefined;
    if (spec.autoMemoryStackId && !anyFailure) {
      try {
        automaticMemory = await captureAutoMemory(
          spec.autoMemoryStackId, memoryWorkspace, spec.name,
          spec.phases.flatMap((phase, phaseIndex) => phase.agents.map((agent, agentIndex) => ({
            taskId: workflowAgentTaskId(correlationOrigin, phaseIndex, agentIndex),
            role: agent.description,
            report: agentResults[workflowAgentTaskId(toolCallId, phaseIndex, agentIndex)]?.report ?? '',
          }))),
        );
      } catch (error) {
        automaticMemory = { saved: 0, reused: 0,
          warnings: ['Automatic memory capture failed: ' + String(error).slice(0, 200)] };
      }
    }
    return finish(
      anyFailure ? 'error' : 'done',
      stringifyToolResult({
        workflow: spec.name, status: anyFailure ? 'completed_with_failures' : 'completed',
        phases: phaseSummaries,
        ...(consensus ? { consensus } : {}),
        ...(spec.notebook ? { notebookStaged, notebookWarnings, notebookRequiresApproval: true } : {}),
         ...(liveBus ? { liveMessaging: liveBus.statistics() } : {}),
         ...(automaticMemory ? { automaticMemory } : {}),
      }),
    );
  } catch (err) {
    return finish('error', stringifyToolError(err));
  } finally {
    liveBus?.close();
  }
}
