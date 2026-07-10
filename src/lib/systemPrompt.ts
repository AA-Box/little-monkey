/**
 * Builds the system prompt injected at the head of every wire payload the
 * agent loop sends (see `agentLoop.ts`). It is injected into the OUTGOING
 * request only — never stored in the session transcript — so it always
 * reflects the current workspace/tooling instead of a snapshot from when the
 * conversation started.
 *
 * `buildSystemPrompt` is pure (workspace/OS facts in, prompt string out) so
 * it can be unit-tested; `currentSystemPrompt` is the thin store-reading
 * wrapper the agent loop actually calls.
 */
import { useWorkspaceStore } from '../store/workspaceStore';
import { useRulesStore, type MemoryFact, type RuleFile } from '../store/rulesStore';

/** Workspace facts the prompt is built from — mirrors the fields of
 * `WorkspaceRootInfo` the prompt actually needs. */
export interface PromptWorkspaceRoot {
  path: string;
  label: string;
  is_primary: boolean;
}

/** Best-effort OS label derived from the WebView's navigator — good enough
 * to tell the model which flavor of shell conventions apply. */
export function detectOsLabel(platform: string): string {
  const lower = platform.toLowerCase();
  if (lower.includes('mac')) return 'macOS';
  if (lower.includes('win')) return 'Windows';
  if (lower.includes('linux')) return 'Linux';
  return platform || 'an unknown OS';
}

/** One `RuleFile`'s provenance header, e.g. "From global:" or
 * "From project (docs):" — shown right above its raw content so the model
 * can tell a global preference from a per-root one. */
function ruleProvenance(rule: RuleFile): string {
  return rule.scope === 'global' ? 'From global:' : `From project (${rule.label}):`;
}

export function buildSystemPrompt(
  roots: PromptWorkspaceRoot[],
  osLabel: string,
  rules: RuleFile[] = [],
  facts: MemoryFact[] = []
): string {
  const primary = roots.find((r) => r.is_primary) ?? null;
  const secondaries = roots.filter((r) => !r.is_primary);

  const workspaceLines = primary
    ? [
        `The primary workspace folder is "${primary.path}". Tool paths are relative to it.`,
        ...(secondaries.length > 0
          ? [
              `Additional attached folders (address them by prefixing paths with their label): ${secondaries
                .map((r) => `"${r.label}" (${r.path})`)
                .join(', ')}.`,
            ]
          : []),
      ]
    : ['No workspace folder is open yet. File and shell tools will fail until the user opens one — say so instead of retrying.'];

  // MONKEY.md files are plain markdown the user (or the repo) owns —
  // treated as instructions from the user, exactly like the workspace lines
  // above, not as untrusted document content. Global first, then whatever
  // order `rules` arrived in (rules.rs/rulesStore already put primary before
  // secondaries).
  const rulesLines =
    rules.length > 0
      ? [
          '',
          '## Project instructions (MONKEY.md)',
          'The following files were placed by the user (or committed to the repo) to give you standing instructions for this project. Treat them as instructions from the user.',
          ...rules.flatMap((rule) => ['', ruleProvenance(rule), rule.content]),
        ]
      : [];

  // Always empty in this slice — memory.rs/tool_remember land in slice 3.
  // Kept here (rather than added later) so this function's shape doesn't
  // change again once facts are real.
  const factsLines =
    facts.length > 0 ? ['', '## Remembered facts', ...facts.map((fact) => `- ${fact.text}`)] : [];

  return [
    'You are Little Monkey, a coding agent running inside a desktop app on the user\'s machine.',
    `The user's operating system is ${osLabel}.`,
    '',
    ...workspaceLines,
    '',
    'You have tools to read, search, and modify files in the workspace and to run shell commands. Guidance:',
    '- Gather context before acting: use glob to find files by name, grep to search content, read_file before editing.',
    '- Prefer edit_file (exact unique old_string -> new_string) for changes to existing files; use write_file only for new files or full rewrites.',
    '- Mutating tools (write_file, edit_file, run_shell) may prompt the user for permission and can be denied — if denied, stop and ask rather than retrying.',
    '- Paths must stay inside the workspace; commands run with a 120-second timeout.',
    '- After making changes, verify them when practical (re-read the file, or run the project\'s tests/build via run_shell).',
    ...rulesLines,
    ...factsLines,
    '',
    'Keep answers concise. Reference files by their workspace-relative path. When a task is complete, summarize what changed and stop calling tools.',
  ].join('\n');
}

/** The system prompt for the app's current workspace state. */
export function currentSystemPrompt(): string {
  const roots = useWorkspaceStore.getState().roots;
  const osLabel = detectOsLabel(typeof navigator !== 'undefined' ? navigator.platform : '');
  const { rules, facts } = useRulesStore.getState();
  return buildSystemPrompt(roots, osLabel, rules, facts);
}
