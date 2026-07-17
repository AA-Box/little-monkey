/**
 * Cross-Repo Change Planner (ROADMAP.md Phase 7, item 12) — turns a plain-
 * text description of a cross-cutting change into an ordered, per-root plan
 * across the workspace's attached roots (primary + secondary folders; see
 * `workspaceStore.ts`'s `WorkspaceRootInfo`).
 *
 * Model calls reuse the exact one-shot pattern `agentLoop.ts`'s
 * `compactSessionNow` already uses for its own structured summary call:
 * `resolveTarget()` + `turnEngine.ts`'s `attemptStream()` with an empty tool
 * list and no tool-calling loop — this is a single planning turn, not an
 * implementation run, so there is nothing for the model to call a tool for.
 * The JSON-envelope contract and its `stripJsonFence`/validate-then-parse
 * shape mirrors `crewRunner.ts`'s `parseMemberEnvelope`/`parseCoordinatorEnvelope`
 * — the model's reply is untrusted structured text, never executed code.
 *
 * This module never touches a repository — it only asks the model to reason
 * about the workspace roots it's told about and returns a plan object for a
 * human to review, edit, and explicitly approve. Turning an approved step
 * into a real git branch is entirely separate: `crossRepoChangePlannerStore.ts`
 * gates that on approval and reuses `gitDelivery.ts`'s existing owned-worktree
 * confirm-and-type-the-phrase mutation flow, not anything in this file.
 */
import { resolveTarget } from './agentLoop';
import { attemptStream } from './turnEngine';
import { effortForTarget } from '../store/modelStore';
import type { ChatMessage } from './llamaClient';
import type { WorkspaceRootInfo } from '../store/workspaceStore';

/** Safety cap on how many per-root steps a single plan may contain — a
 * runaway/misbehaving model reply is rejected rather than silently truncated,
 * so the caller sees an explicit error instead of a partial plan. */
export const MAX_CROSS_REPO_STEPS = 12;
const MAX_TEXT_FIELD_CHARS = 2_000;
const MAX_NOTES_CHARS = 4_000;

export interface CrossRepoPlanStep {
  stepId: string;
  rootId: string;
  rootLabel: string;
  rootPath: string;
  /** 1-based position in the rollout sequence — steps are sorted and
   * renumbered contiguously by `parsePlanEnvelope` regardless of what the
   * model returned. */
  order: number;
  summary: string;
  changes: string;
  risks: string;
  rollback: string;
  /** Other roots (by id) this step's change depends on landing first —
   * purely informational for the panel; nothing enforces it mechanically. */
  dependsOnRootIds: string[];
}

export interface CrossRepoPlan {
  planId: string;
  description: string;
  createdAtMs: number;
  /** Free-form sequencing rationale / assumptions the model called out,
   * shown above the per-root steps in the panel. */
  notes: string;
  steps: CrossRepoPlanStep[];
}

function clamp(value: string, max: number): string {
  const trimmed = value.trim();
  return trimmed.length > max ? `${trimmed.slice(0, max)}…` : trimmed;
}

/** Matches `crewRunner.ts`'s `stripJsonFence` — the model is asked for bare
 * JSON but reliably wraps it in a ```json fence anyway often enough that
 * every JSON-envelope caller in this codebase strips it defensively. */
function stripJsonFence(raw: string): string {
  const trimmed = raw.trim();
  const fenced = trimmed.match(/^```(?:json)?\s*([\s\S]*?)\s*```$/i);
  return (fenced?.[1] ?? trimmed).trim();
}

export function buildPlanningMessages(description: string, roots: WorkspaceRootInfo[]): ChatMessage[] {
  const rootsList = roots
    .map((root) => `- id="${root.id}" label="${root.label}" path="${root.path}"${root.is_primary ? ' (primary)' : ''}`)
    .join('\n');

  const system = [
    "You are Little Monkey's Cross-Repo Change Planner: a planning-only assistant. You never edit files, run commands, or call any tool here — you only produce a JSON plan for a human to review.",
    'Given a description of a coordinated change and the exact list of workspace roots (repositories/packages) available, produce a JSON plan for how to roll the change out across some or all of those roots, in dependency order.',
    'Reply with ONLY one JSON object, no Markdown code fence, no prose before or after, matching exactly this shape:',
    '{"notes":"overall assumptions/sequencing rationale as one string","steps":[{"rootId":"<one of the given root ids>","order":1,"summary":"one-line summary of what changes in this root","changes":"concrete description of the change to make in this root","risks":"what could break, specific to this root and its position in the sequence","rollback":"how to revert this root\'s change safely if something goes wrong","dependsOnRootIds":["<rootId>"]}]}',
    `Only use "rootId" values exactly as given below - never invent a root id, and never emit more than ${MAX_CROSS_REPO_STEPS} steps. Every root you include may appear at most once. Order values must be sequential starting at 1 in the sequence you intend the steps to be carried out in.`,
    'If the description only concerns some of the roots, only include those roots as steps - do not force every root into the plan.',
  ].join('\n');

  const user = [
    'Change description (from the user, verbatim):',
    description.trim(),
    '',
    'Workspace roots available for this plan:',
    rootsList || '(none attached)',
  ].join('\n');

  return [
    { role: 'system', content: system },
    { role: 'user', content: user },
  ];
}

/**
 * Validates and normalizes the model's raw reply into plan steps. Exported
 * (separately from `generateCrossRepoPlan`) so the envelope contract can be
 * unit-tested against fixed strings without mocking any model call.
 */
export function parsePlanEnvelope(
  raw: string,
  roots: WorkspaceRootInfo[],
): { notes: string; steps: Array<Omit<CrossRepoPlanStep, 'stepId'>> } {
  let parsed: unknown;
  try {
    parsed = JSON.parse(stripJsonFence(raw));
  } catch {
    throw new Error('The model did not return the required JSON plan envelope.');
  }
  if (!parsed || typeof parsed !== 'object' || Array.isArray(parsed)) {
    throw new Error('Plan envelope was not a JSON object.');
  }
  const value = parsed as Record<string, unknown>;
  const notes = typeof value.notes === 'string' ? clamp(value.notes, MAX_NOTES_CHARS) : '';

  if (!Array.isArray(value.steps) || value.steps.length === 0) {
    throw new Error('Plan envelope did not include any steps.');
  }
  if (value.steps.length > MAX_CROSS_REPO_STEPS) {
    throw new Error(`Plan envelope exceeded the ${MAX_CROSS_REPO_STEPS}-step limit.`);
  }

  const rootById = new Map(roots.map((root) => [root.id, root]));
  const seenRootIds = new Set<string>();

  const steps = value.steps.map((rawStep, index): Omit<CrossRepoPlanStep, 'stepId'> => {
    if (!rawStep || typeof rawStep !== 'object' || Array.isArray(rawStep)) {
      throw new Error(`Step ${index + 1} was not a JSON object.`);
    }
    const item = rawStep as Record<string, unknown>;
    const rootId = typeof item.rootId === 'string' ? item.rootId : '';
    const root = rootById.get(rootId);
    if (!root) {
      throw new Error(`Step ${index + 1} referenced unknown root id "${rootId}".`);
    }
    if (seenRootIds.has(rootId)) {
      throw new Error(`Root "${root.label}" appeared in more than one step.`);
    }
    seenRootIds.add(rootId);

    const summary = typeof item.summary === 'string' ? clamp(item.summary, MAX_TEXT_FIELD_CHARS) : '';
    if (!summary) throw new Error(`Step ${index + 1} (${root.label}) is missing a summary.`);
    const changes = typeof item.changes === 'string' ? clamp(item.changes, MAX_TEXT_FIELD_CHARS) : '';
    const risks = typeof item.risks === 'string' ? clamp(item.risks, MAX_TEXT_FIELD_CHARS) : '';
    const rollback = typeof item.rollback === 'string' ? clamp(item.rollback, MAX_TEXT_FIELD_CHARS) : '';

    const dependsOnRootIds = Array.isArray(item.dependsOnRootIds)
      ? item.dependsOnRootIds.filter(
          (id): id is string => typeof id === 'string' && id !== rootId && rootById.has(id),
        )
      : [];

    const orderRaw = typeof item.order === 'number' && Number.isFinite(item.order) ? item.order : index + 1;

    return {
      rootId,
      rootLabel: root.label,
      rootPath: root.path,
      order: orderRaw,
      summary,
      changes,
      risks,
      rollback,
      dependsOnRootIds,
    };
  });

  steps.sort((a, b) => a.order - b.order);
  steps.forEach((step, index) => {
    step.order = index + 1;
  });

  return { notes, steps };
}

/**
 * Generates a fresh cross-repo plan from a change description and the
 * workspace's current roots. Never mutates any repository — this is a read-
 * only planning call; nothing is written until a human later approves the
 * plan and explicitly requests branch creation per step (see
 * `crossRepoChangePlannerStore.ts`).
 */
export async function generateCrossRepoPlan(
  description: string,
  roots: WorkspaceRootInfo[],
  signal?: AbortSignal,
): Promise<CrossRepoPlan> {
  const trimmedDescription = description.trim();
  if (!trimmedDescription) throw new Error('Describe the change before generating a plan.');
  if (roots.length === 0) {
    throw new Error('Attach at least one workspace root before generating a cross-repo plan.');
  }

  const target = await resolveTarget();
  const effort = effortForTarget(target);
  const messages = buildPlanningMessages(trimmedDescription, roots);
  // `recordUsage: false` — there is no active chat session for this one-shot
  // planning call to attribute token usage to, same reasoning `subagent.ts`
  // documents for its own child attempts.
  const result = await attemptStream(
    target,
    messages,
    [],
    signal,
    effort,
    `cross-repo-plan:${crypto.randomUUID()}`,
    undefined,
    false,
  );
  if (result.streamError) throw new Error(result.streamError);

  const { notes, steps } = parsePlanEnvelope(result.content, roots);
  return {
    planId: crypto.randomUUID(),
    description: trimmedDescription,
    createdAtMs: Date.now(),
    notes,
    steps: steps.map((step) => ({ ...step, stepId: crypto.randomUUID() })),
  };
}
