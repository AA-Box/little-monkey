/**
 * User-defined agent types — the `.monkey/agents/<name>.md` equivalent of
 * Claude Code's `.claude/agents/*.md`. Each file is YAML frontmatter
 * (`name`, `description`, `tools`, optional `effort`) plus a markdown body
 * used as the agent's system-prompt addendum. A loaded definition becomes an
 * extra value the `task`/`workflow` tools' `profile` parameter accepts (see
 * `subagent.ts`'s `resolveSubagentProfile`), resolving to the def's tool
 * list and addendum instead of a built-in profile's.
 *
 * Trust boundary: a custom agent's tool list can never exceed
 * `CUSTOM_AGENT_TOOL_CEILING` — the `code` profile's own set plus the two
 * read-only web tools already in `TOOLS`. Anything else (and `task`/
 * `workflow` in particular, which would break the structural depth cap) is
 * rejected at load time with a visible per-file error, and the ceiling is
 * re-applied at dispatch (`toolsForCustomAgent`) as defense in depth.
 * Mutating tools resolved through a custom def still go through the exact
 * same Rust commands, permission prompts, and parent-checkpoint plumbing as
 * a `code`-profile child's — a definition file grants tool NAMES, never new
 * trust.
 */
import { TOOLS } from './tools';
import type { ToolDef } from './llamaClient';

/** Where definitions live, relative to the PRIMARY workspace root. Secondary
 * roots are deliberately not scanned in this slice — a def names tools, so
 * "which folder can add agents" should stay the one the user opened, not any
 * attached extra root. */
export const CUSTOM_AGENTS_DIR = '.monkey/agents';

/** Same shape rule as a skill command (`skills.ts`'s `parseSkillTurn`). */
const NAME_PATTERN = /^[a-z0-9][a-z0-9-]{0,31}$/;

/** Names a definition may never claim: the built-in profiles it would
 * shadow, and the orchestration tools whose absence from every child tool
 * list is what caps delegation depth at 1. */
const RESERVED_NAMES: ReadonlySet<string> = new Set(['explore', 'code', 'task', 'workflow']);

/** The only tool names a definition may grant — the `code` profile's own
 * set plus the read-only web tools already in `TOOLS`. Derived by name here
 * (not imported from `tools.ts`'s private profile sets) so this module owns
 * one explicit, reviewable list; `customAgents.test.ts` pins every entry to
 * an actual `TOOLS` member so drift fails a test rather than silently
 * granting a phantom name. */
export const CUSTOM_AGENT_TOOL_CEILING: ReadonlySet<string> = new Set([
  'read_file',
  'list_dir',
  'glob',
  'grep',
  'write_file',
  'edit_file',
  'run_shell',
  'shell_output',
  'shell_kill',
  'web_fetch',
  'web_search',
]);

/** Tools whose presence makes a custom agent ride the `code` task class for
 * model routing/pinning (`resolveSubagentTarget`) — the ones that change
 * files or run commands. */
const MUTATING_TOOL_NAMES: ReadonlySet<string> = new Set(['write_file', 'edit_file', 'run_shell']);

export interface CustomAgentDef {
  name: string;
  description: string;
  /** Validated: every entry is in `CUSTOM_AGENT_TOOL_CEILING`. */
  tools: string[];
  effort?: 'low' | 'medium' | 'high';
  /** The file body — appended to the child's system prompt. May be empty. */
  addendum: string;
  /** Workspace-relative path the def was loaded from, for error display. */
  sourcePath: string;
}

export interface CustomAgentLoadError {
  path: string;
  message: string;
}

export type ParsedCustomAgentFile =
  | { ok: true; def: CustomAgentDef }
  | { ok: false; error: CustomAgentLoadError };

/** Minimal frontmatter splitter: a leading `---` fence, `key: value` lines
 * (with `tools` also accepting an indented `- item` block list), a closing
 * `---`, then the body. Deliberately not a YAML parser — these four fields
 * are flat scalars/lists, and a full parser would only widen what a
 * malformed file can smuggle in. Returns `null` when the fence is missing. */
export function splitFrontmatter(raw: string): { fields: Map<string, string | string[]>; body: string } | null {
  const text = raw.replace(/^﻿/, '');
  const lines = text.split(/\r?\n/);
  if (lines[0]?.trim() !== '---') return null;
  const fields = new Map<string, string | string[]>();
  let index = 1;
  let currentListKey: string | null = null;
  for (; index < lines.length; index++) {
    const line = lines[index];
    if (line.trim() === '---') {
      return { fields, body: lines.slice(index + 1).join('\n').trim() };
    }
    const listItem = /^\s+-\s+(.+)$/.exec(line);
    if (listItem && currentListKey) {
      const existing = fields.get(currentListKey);
      const list = Array.isArray(existing) ? existing : [];
      list.push(listItem[1].trim());
      fields.set(currentListKey, list);
      continue;
    }
    const keyValue = /^([A-Za-z][A-Za-z0-9_-]*):\s*(.*)$/.exec(line);
    if (!keyValue) {
      if (line.trim() === '') continue;
      return null; // an unparseable frontmatter line fails the whole file, visibly
    }
    const key = keyValue[1].toLowerCase();
    const value = keyValue[2].trim();
    if (value === '') {
      currentListKey = key;
      fields.set(key, []);
    } else {
      currentListKey = null;
      fields.set(key, value);
    }
  }
  return null; // fence never closed
}

function fieldAsList(value: string | string[] | undefined): string[] {
  if (value === undefined) return [];
  if (Array.isArray(value)) return value;
  return value
    .split(',')
    .map((entry) => entry.trim())
    .filter((entry) => entry.length > 0);
}

/**
 * Parses one definition file. Every failure mode is a visible per-file
 * error (never a silently-dropped or silently-narrowed def) — the "fail the
 * def with a visible warning, not silently" contract.
 */
export function parseCustomAgentFile(sourcePath: string, raw: string): ParsedCustomAgentFile {
  const fail = (message: string): ParsedCustomAgentFile => ({ ok: false, error: { path: sourcePath, message } });

  const parsed = splitFrontmatter(raw);
  if (!parsed) return fail('Missing or malformed YAML frontmatter (expected a leading and closing "---" fence with "key: value" lines).');
  const { fields, body } = parsed;

  const name = typeof fields.get('name') === 'string' ? (fields.get('name') as string) : '';
  if (!name) return fail('Frontmatter is missing the required "name" field.');
  if (!NAME_PATTERN.test(name)) return fail(`Agent name "${name}" is invalid — use 1-32 lowercase letters, digits, or hyphens, starting with a letter or digit.`);
  if (RESERVED_NAMES.has(name)) return fail(`Agent name "${name}" is reserved.`);

  const description = typeof fields.get('description') === 'string' ? (fields.get('description') as string) : '';
  if (!description) return fail('Frontmatter is missing the required "description" field.');

  const tools = fieldAsList(fields.get('tools'));
  if (tools.length === 0) return fail('Frontmatter is missing the required "tools" list.');
  for (const tool of tools) {
    if (tool === 'task' || tool === 'workflow') {
      return fail(`Tool "${tool}" can never be granted to an agent — delegation depth is capped at 1.`);
    }
    if (!CUSTOM_AGENT_TOOL_CEILING.has(tool)) {
      return fail(`Tool "${tool}" is not grantable. Allowed tools: ${[...CUSTOM_AGENT_TOOL_CEILING].join(', ')}.`);
    }
  }
  const uniqueTools = [...new Set(tools)];

  const rawEffort = fields.get('effort');
  let effort: CustomAgentDef['effort'];
  if (rawEffort !== undefined) {
    if (rawEffort !== 'low' && rawEffort !== 'medium' && rawEffort !== 'high') {
      return fail(`Effort "${String(rawEffort)}" is invalid — use low, medium, or high.`);
    }
    effort = rawEffort;
  }

  return { ok: true, def: { name, description, tools: uniqueTools, effort, addendum: body, sourcePath } };
}

/**
 * Folds a directory's parsed files into a name-keyed def map plus the error
 * list. A duplicate name is an error on the LATER file (files are processed
 * in the caller's listing order), keeping the first def active. Exported for
 * the logic tests; `customAgentStore.refresh` is the one real caller.
 */
export function collectCustomAgents(parsed: ParsedCustomAgentFile[]): {
  defs: Record<string, CustomAgentDef>;
  errors: CustomAgentLoadError[];
} {
  const defs: Record<string, CustomAgentDef> = {};
  const errors: CustomAgentLoadError[] = [];
  for (const entry of parsed) {
    if (!entry.ok) {
      errors.push(entry.error);
      continue;
    }
    const existing = defs[entry.def.name];
    if (existing) {
      errors.push({ path: entry.def.sourcePath, message: `Duplicate agent name "${entry.def.name}" — already defined by ${existing.sourcePath}.` });
      continue;
    }
    defs[entry.def.name] = entry.def;
  }
  return { defs, errors };
}

/** The dispatch-time tool list for a custom agent: `TOOLS` filtered to the
 * def's names re-intersected with the ceiling. The intersection is redundant
 * for a def that came through `parseCustomAgentFile` — that's the point:
 * defense in depth against any def that reached the store some other way. */
export function toolsForCustomAgent(def: CustomAgentDef): ToolDef[] {
  const granted = new Set(def.tools.filter((name) => CUSTOM_AGENT_TOOL_CEILING.has(name)));
  return TOOLS.filter((tool) => granted.has(tool.function.name));
}

/** Whether routing/pinning should treat this agent as `code`-class work. */
export function customAgentBaseProfile(def: CustomAgentDef): 'explore' | 'code' {
  return def.tools.some((name) => MUTATING_TOOL_NAMES.has(name)) ? 'code' : 'explore';
}

/**
 * Compact catalog section for the parent system prompt — the custom-agent
 * counterpart of `skills.ts`'s `composeSkillCatalog`, appended by
 * `agentLoop.ts` under the same `subagentsEnabled` gate that offers
 * `TASK_TOOL`. Empty string when nothing is loaded, so the
 * `.filter(Boolean)` section join drops it.
 */
export function composeCustomAgentCatalog(defs: CustomAgentDef[]): string {
  if (defs.length === 0) return '';
  return [
    '## Custom agents',
    'These user-defined agents are also accepted as the `profile` value of the `task` and `workflow` tools, alongside the built-in "explore" and "code". Pick the one whose description matches the subtask.',
    ...defs.map((def) => `- ${def.name} — ${def.description} (tools: ${def.tools.join(', ')})`),
  ].join('\n');
}
