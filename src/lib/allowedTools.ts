/**
 * A skill's `allowed_tools` narrowing, as one rule two runtimes share.
 *
 * The ordinary chat turn (`agentLoop.ts`, which re-exports both of these) and
 * the headless runner (`headlessAgentRunner.ts`, which the learning loop's
 * evaluation arms run on) must narrow tools identically — otherwise a staged
 * skill could pass an evaluation using a tool it will not have once installed.
 * Kept in its own module because those two modules both need it and neither
 * can import the other.
 */
import type { ToolDef } from './llamaClient';
import type { SlashSkill } from './skills';

/**
 * The union of the invoked skills' `allowedTools`, or `null` when the turn is
 * unrestricted — because no skill was invoked, or because at least one invoked
 * skill declares no list and therefore accepts whatever the turn already had.
 */
export function allowedToolsRestriction(
  invokedCommands: ReadonlySet<string>,
  availableSkills: SlashSkill[],
): ReadonlySet<string> | null {
  const invoked = availableSkills.filter((candidate) => invokedCommands.has(candidate.command));
  if (invoked.length === 0 || invoked.some((candidate) => !candidate.allowedTools || candidate.allowedTools.length === 0)) {
    return null;
  }
  return new Set(invoked.flatMap((candidate) => candidate.allowedTools ?? []));
}

/**
 * Applies `allowedToolsRestriction`'s result (if any) to a per-turn tool
 * list — the `skill` tool itself always stays offered even under a
 * restrictive list (deliberate exception): this app stacks up to
 * `MAX_SKILLS_PER_TURN` skills per turn (unlike a single-skill-at-a-time
 * model), so a restrictive skill must never strand the model unable to
 * invoke a different, less-restricted one.
 */
export function applyAllowedToolsRestriction(tools: ToolDef[], restriction: ReadonlySet<string> | null): ToolDef[] {
  if (restriction === null) return tools;
  return tools.filter((tool) => tool.function.name === 'skill' || restriction.has(tool.function.name));
}
