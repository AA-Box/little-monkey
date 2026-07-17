/**
 * Migration and Upgrade Agent (ROADMAP.md Phase 7) — plan generation for a
 * framework/runtime/dependency/language/Tauri/React/Rust/API migration goal.
 *
 * MVP scope, deliberately narrow (see `migrationAgentRunner.ts`'s doc comment
 * for the execution half): this module only produces the ORDERED SLICE PLAN
 * — a large upgrade broken into small, individually-testable steps with a
 * compatibility risk note and a rollback note per slice — using the exact
 * same "one local-model call, ask for structured JSON, defensively parse it"
 * pattern `riskJudge.ts` uses for its own advisory classification. It never
 * touches the filesystem beyond a best-effort read of the workspace's own
 * `package.json`/`Cargo.toml` for context, and it never throws on a
 * malformed model response — `parseMigrationPlanJson` failing falls through
 * to `fallbackHeuristicPlan`, so a run always produces SOME actionable plan
 * even against a weak local model or an empty response.
 *
 * Turning slice #1 of a generated plan into a real code change is
 * `migrationAgentRunner.ts`'s job (a headless agent turn against an owned
 * worktree, reusing `turnEngine.ts`/`durableRun.ts` exactly like
 * `issueToPrRunner.ts` does) — orchestrated end-to-end by
 * `migrationAgentStore.ts`.
 */
import { invoke } from '@tauri-apps/api/core';

import { resolveTarget } from './agentLoop';
import type { ChatMessage } from './llamaClient';
import { effortForTarget } from '../store/modelStore';
import { attemptStream } from './turnEngine';

export type MigrationRiskLevel = 'low' | 'medium' | 'high';

export const MIGRATION_RISK_LEVELS: readonly MigrationRiskLevel[] = ['low', 'medium', 'high'];

/** Hard cap on how many slices one plan can carry — keeps the plan reviewable
 * and keeps a pathological model response (hundreds of "slices") from
 * blowing up the panel. */
export const MAX_MIGRATION_SLICES = 6;

/** Cap on how much of each manifest file is inlined into the plan prompt. */
const MAX_MANIFEST_CHARS = 4000;
/** Cap on how much of the model's own plan response is kept when parsing
 * fails and the error needs to be reported back to the caller. */
const MAX_PARSE_ERROR_EXCERPT = 300;

export interface MigrationSlice {
  id: string;
  /** 1-based position in the plan — the first slice is the only one the
   * MVP will actually execute (see `migrationAgentStore.ts`); the rest stay
   * a plan-only follow-up checklist. */
  order: number;
  title: string;
  description: string;
  riskLevel: MigrationRiskLevel;
  riskNotes: string[];
  rollbackNotes: string;
  /** Files the model expects this slice to touch — advisory only, never
   * enforced against what the headless run actually edits. */
  filesLikely: string[];
}

export interface MigrationPlan {
  goal: string;
  summary: string;
  slices: MigrationSlice[];
  /** `true` when `parseMigrationPlanJson` couldn't make sense of the model's
   * response and `fallbackHeuristicPlan` was used instead — surfaced in the
   * panel so the user knows to treat the plan as generic rather than
   * specific to their codebase. */
  usedFallback: boolean;
  createdAtMs: number;
}

export interface ManifestExcerpts {
  packageJson: string | null;
  cargoToml: string | null;
}

function truncate(text: string, max: number): string {
  return text.length > max ? `${text.slice(0, max)}…` : text;
}

/** Best-effort read of the primary workspace's own `package.json`/
 * `Cargo.toml` for migration-plan context — a missing file (e.g. a pure-Rust
 * repo with no `package.json`) resolves to `null` rather than throwing, same
 * posture as every other "nice to have" context read in this codebase. */
export async function readManifestExcerpts(): Promise<ManifestExcerpts> {
  const readOne = async (path: string): Promise<string | null> => {
    try {
      const content = await invoke<string>('tool_read_file', { path });
      return truncate(content, MAX_MANIFEST_CHARS);
    } catch {
      return null;
    }
  };
  const [packageJson, cargoToml] = await Promise.all([
    readOne('package.json'),
    readOne('Cargo.toml'),
  ]);
  return { packageJson, cargoToml };
}

function stripJsonFences(raw: string): string {
  const trimmed = raw.trim();
  const fenced = trimmed.match(/^```(?:json)?\s*([\s\S]*?)\s*```$/i);
  return (fenced ? fenced[1] : trimmed).trim();
}

/** Builds the one-shot planning prompt: the migration goal plus whatever
 * manifest context was found, asking the model for strict JSON matching
 * `MigrationPlan`'s shape (minus `usedFallback`/`createdAtMs`, which this
 * module fills in itself). */
export function buildMigrationPlanMessages(goal: string, manifests: ManifestExcerpts): ChatMessage[] {
  const manifestSection = [
    manifests.packageJson ? `package.json:\n${manifests.packageJson}` : null,
    manifests.cargoToml ? `Cargo.toml:\n${manifests.cargoToml}` : null,
  ].filter((entry): entry is string => entry !== null);

  const system = [
    'You are a senior engineer planning a migration/upgrade for an existing codebase.',
    `Break the requested migration into at most ${MAX_MIGRATION_SLICES} small, independently testable slices, ordered so each slice leaves the codebase in a working, shippable state before the next one starts.`,
    'Reply with ONLY a single JSON object (no markdown fences, no commentary) with this exact shape:',
    '{"summary": string, "slices": [{"title": string, "description": string, "riskLevel": "low"|"medium"|"high", "riskNotes": string[], "rollbackNotes": string, "filesLikely": string[]}]}',
    'Every slice needs a specific, actionable "description" (what to change and why), at least one concrete "riskNotes" entry about what could break or be incompatible, and a "rollbackNotes" describing exactly how to safely undo just that slice.',
  ].join('\n');

  const user = [
    `Migration goal: ${goal}`,
    '',
    manifestSection.length > 0
      ? `Relevant project manifests:\n\n${manifestSection.join('\n\n')}`
      : '(No package.json or Cargo.toml could be read from the current workspace.)',
  ].join('\n');

  return [
    { role: 'system', content: system },
    { role: 'user', content: user },
  ];
}

function coerceRiskLevel(value: unknown): MigrationRiskLevel {
  return typeof value === 'string' && (MIGRATION_RISK_LEVELS as readonly string[]).includes(value)
    ? (value as MigrationRiskLevel)
    : 'medium';
}

function coerceStringArray(value: unknown, max = 8): string[] {
  if (!Array.isArray(value)) return [];
  return value.filter((entry): entry is string => typeof entry === 'string' && entry.trim().length > 0).slice(0, max);
}

function normalizeSlice(raw: unknown, index: number): MigrationSlice {
  const obj = (raw && typeof raw === 'object' ? raw : {}) as Record<string, unknown>;
  const title = typeof obj.title === 'string' && obj.title.trim() ? obj.title.trim() : `Slice ${index + 1}`;
  const description = typeof obj.description === 'string' && obj.description.trim() ? obj.description.trim() : '';
  const rollbackNotes = typeof obj.rollbackNotes === 'string' && obj.rollbackNotes.trim()
    ? obj.rollbackNotes.trim()
    : 'Revert this slice\'s branch/commits and reopen it from a clean checkout of the base branch.';
  return {
    id: `slice-${index + 1}`,
    order: index + 1,
    title,
    description,
    riskLevel: coerceRiskLevel(obj.riskLevel),
    riskNotes: coerceStringArray(obj.riskNotes),
    rollbackNotes,
    filesLikely: coerceStringArray(obj.filesLikely, 12),
  };
}

/**
 * Parses a model's raw plan response into a `MigrationPlan`. Throws a short,
 * human-readable `Error` on anything unparseable/empty — callers (only
 * `generateMigrationPlan` below) are expected to catch it and fall back to
 * `fallbackHeuristicPlan` rather than surface a raw JSON parse error to the
 * user.
 */
export function parseMigrationPlanJson(raw: string, goal: string): MigrationPlan {
  const stripped = stripJsonFences(raw);
  if (!stripped) throw new Error('The model returned an empty plan.');

  let parsed: unknown;
  try {
    parsed = JSON.parse(stripped);
  } catch {
    throw new Error(`The model's plan response was not valid JSON: "${truncate(stripped, MAX_PARSE_ERROR_EXCERPT)}"`);
  }

  if (!parsed || typeof parsed !== 'object') {
    throw new Error('The model\'s plan response was not a JSON object.');
  }
  const obj = parsed as Record<string, unknown>;
  const rawSlices = Array.isArray(obj.slices) ? obj.slices : [];
  if (rawSlices.length === 0) {
    throw new Error('The model\'s plan response contained no slices.');
  }

  const summary = typeof obj.summary === 'string' && obj.summary.trim()
    ? obj.summary.trim()
    : `Migration plan for: ${goal}`;
  const slices = rawSlices.slice(0, MAX_MIGRATION_SLICES).map((entry, index) => normalizeSlice(entry, index));

  return { goal, summary, slices, usedFallback: false, createdAtMs: Date.now() };
}

/**
 * A generic, always-available 3-slice plan used whenever the model's own
 * response can't be parsed (offline/no model configured is a real error and
 * still throws out of `generateMigrationPlan` — this only covers "a model
 * answered, but not usefully"). Not specific to the target codebase, but
 * every field is populated with real, actionable guidance so the run still
 * produces a working plan/branch/tests/risks/follow-up checklist end to end.
 */
export function fallbackHeuristicPlan(goal: string): MigrationPlan {
  const slices: MigrationSlice[] = [
    {
      id: 'slice-1',
      order: 1,
      title: 'Audit current usage and bump the declared version',
      description: `Locate every place the codebase depends on what "${goal}" is migrating, and update the manifest/declared version to the new target without changing call sites yet.`,
      riskLevel: 'medium',
      riskNotes: [
        'The new version may not resolve/install cleanly alongside other pinned dependencies.',
        'A major version bump can carry breaking changes not yet accounted for in this slice.',
      ],
      rollbackNotes: 'Revert the manifest/lockfile change and reinstall — no call sites were touched yet.',
      filesLikely: [],
    },
    {
      id: 'slice-2',
      order: 2,
      title: 'Update call sites and fix compile/type errors',
      description: 'Work through every compiler/type-checker/linter error the version bump surfaced, updating call sites to the new API one file at a time.',
      riskLevel: 'high',
      riskNotes: [
        'Behavioral differences in the new API may not show up as compile errors at all.',
        'A partial update can leave the codebase in a non-compiling state if interrupted.',
      ],
      rollbackNotes: 'Revert this slice\'s commits on top of slice 1\'s clean, still-compiling state.',
      filesLikely: [],
    },
    {
      id: 'slice-3',
      order: 3,
      title: 'Run the full test/build suite and address regressions',
      description: 'Run the repository\'s own test and build scripts, and fix any regression the migration introduced before calling the migration complete.',
      riskLevel: 'low',
      riskNotes: ['A green test suite does not guarantee full behavioral parity for untested paths.'],
      rollbackNotes: 'Revert just the regression fixes if one of them turns out to be wrong; the migration itself stays in place.',
      filesLikely: [],
    },
  ];
  return {
    goal,
    summary: `Generic 3-slice plan for: ${goal} (the model's own plan response could not be parsed, so this fallback was used).`,
    slices,
    usedFallback: true,
    createdAtMs: Date.now(),
  };
}

export interface GenerateMigrationPlanOptions {
  signal?: AbortSignal;
}

/**
 * Generates a `MigrationPlan` for `goal`: resolves the active model target
 * (same resolution every chat turn/subagent uses), reads whatever manifest
 * context is available, asks for a structured plan in one non-tool-calling
 * completion, and defensively parses the result — falling back to
 * `fallbackHeuristicPlan` rather than throwing when the response can't be
 * parsed. Only throws when no model target can be resolved at all (no model
 * installed/selected), which the caller (`migrationAgentStore.ts`) surfaces
 * as a real, actionable error rather than silently faking a plan.
 */
export async function generateMigrationPlan(
  goal: string,
  options: GenerateMigrationPlanOptions = {},
): Promise<MigrationPlan> {
  const trimmedGoal = goal.trim();
  if (!trimmedGoal) throw new Error('Enter a migration goal first.');

  const target = await resolveTarget();
  const effort = effortForTarget(target);
  const manifests = await readManifestExcerpts();
  const messages = buildMigrationPlanMessages(trimmedGoal, manifests);
  const signal = options.signal ?? new AbortController().signal;

  const attempt = await attemptStream(target, messages, [], signal, effort, `migration-plan-${crypto.randomUUID()}`);
  if (attempt.streamError !== null) {
    throw new Error(attempt.streamError);
  }

  try {
    return parseMigrationPlanJson(attempt.content, trimmedGoal);
  } catch {
    return fallbackHeuristicPlan(trimmedGoal);
  }
}
