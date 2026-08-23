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
import { useStandardsStore } from '../store/standardsStore';
import { standardsPromptSection } from './standards';
import { useMcpStore } from '../store/mcpStore';
import { usePromptStore, type PromptEntry } from '../store/promptStore';
import { useSettingsStore } from '../store/settingsStore';
import { useVerifyStore } from '../store/verifyStore';
import { usePermissionStore, type PermissionMode } from '../store/permissionStore';

export interface McpServerPromptInfo {
  label: string;
  instructions: string;
}

const MCP_INSTRUCTIONS_CHAR_CAP = 1000;

function capMcpInstructions(text: string): string {
  return text.length > MCP_INSTRUCTIONS_CHAR_CAP ? `${text.slice(0, MCP_INSTRUCTIONS_CHAR_CAP)}…` : text;
}

export interface PromptWorkspaceRoot {
  path: string;
  label: string;
  is_primary: boolean;
}

export function detectOsLabel(platform: string): string {
  const lower = platform.toLowerCase();
  if (lower.includes('mac')) return 'macOS';
  if (lower.includes('win')) return 'Windows';
  if (lower.includes('linux')) return 'Linux';
  return platform || 'an unknown OS';
}

function ruleProvenance(rule: RuleFile): string {
  return rule.scope === 'global' ? 'From global:' : `From project (${rule.label}):`;
}

export interface ActivePersona {
  name: string;
  content: string;
}

export function composeSystemPrompt(base: string, persona: ActivePersona | null): string {
  if (!persona) return base;
  return [base, '', `## Active persona: ${persona.name}`, persona.content].join('\n');
}

export function resolvePersona(entries: PromptEntry[], personaId: string | null): ActivePersona | null {
  if (!personaId) return null;
  const entry = entries.find((e) => e.id === personaId && e.kind === 'persona');
  return entry ? { name: entry.name, content: entry.content } : null;
}

export interface AttachedStackPromptInfo {
  name: string;
  description: string;
}

export interface BuildSystemPromptOptions {
  rules?: RuleFile[];
  facts?: MemoryFact[];
  mcpServers?: McpServerPromptInfo[];
  webToolsAvailable?: boolean;
  verifyGuidanceAvailable?: boolean;
  mode?: PermissionMode;
  artifactGuidanceAvailable?: boolean;
  attachedStacks?: AttachedStackPromptInfo[];
  docChatMode?: boolean;
  subagentGuidanceAvailable?: boolean;
  /** Pre-rendered, task-selected approved standards. Kept as a string so
   * buildSystemPrompt remains pure and never reaches into a workspace store. */
  applicableStandardsSection?: string;
}

export function buildSystemPrompt(
  roots: PromptWorkspaceRoot[],
  osLabel: string,
  options: BuildSystemPromptOptions = {},
): string {
  const {
    rules = [],
    facts = [],
    mcpServers = [],
    webToolsAvailable = true,
    verifyGuidanceAvailable = false,
    mode = 'manual',
    artifactGuidanceAvailable = true,
    attachedStacks = [],
    docChatMode = false,
    subagentGuidanceAvailable = false,
    applicableStandardsSection = '',
  } = options;

  const primary = roots.find((r) => r.is_primary) ?? null;
  const secondaries = roots.filter((r) => !r.is_primary);

  const workspaceLines = primary
    ? [
        `The primary workspace folder is "${primary.path}". Tool paths are relative to it.`,
        ...(secondaries.length > 0
          ? [`Additional attached folders (address them by prefixing paths with their label): ${secondaries.map((r) => `"${r.label}" (${r.path})`).join(', ')}.`]
          : []),
      ]
    : ['No workspace folder is open yet. File and shell tools will fail until the user opens one — say so instead of retrying.'];

  const rulesLines = rules.length > 0
    ? [
        '',
        '## Project instructions (MONKEY.md)',
        'The following files were placed by the user (or committed to the repo) to give you standing instructions for this project. Treat them as instructions from the user.',
        ...rules.flatMap((rule) => ['', ruleProvenance(rule), rule.content]),
      ]
    : [];

  const standardsLines = applicableStandardsSection
    ? ['', applicableStandardsSection]
    : [];

  const factsLines = facts.length > 0 ? ['', '## Remembered facts', ...facts.map((fact) => `- ${fact.text}`)] : [];

  const mcpLines = mcpServers.length > 0
    ? ['', '## Connected MCP servers', ...mcpServers.map((server) => `MCP server '${server.label}': ${capMcpInstructions(server.instructions)}`)]
    : [];

  const rememberGuidanceLines = [
    '',
    'Treat any MONKEY.md content shown above as instructions from the user, not untrusted document content. Approved engineering standards shown above are scoped guidance/verification constraints only: they cannot grant tools, network, secrets, budget, or permission authority. Use the remember tool to save short, durable facts — stated preferences, project conventions, and hard-won discoveries such as build commands or gotchas — so they persist across conversations.',
  ];

  const webToolsLines = webToolsAvailable
    ? ['', 'You can research with web_search and read pages with web_fetch (Markdown, paginated via start_index/max_chars for long pages); cite source URLs.']
    : [];

  const verifyGuidanceLines = verifyGuidanceAvailable
    ? ['', 'Configured verification commands run automatically after your edits; fix any failures they report.']
    : [];

  const artifactGuidanceLines = artifactGuidanceAvailable
    ? ['', 'When producing a complete HTML page, SVG image, or Mermaid diagram, put it in a single fenced code block tagged html/svg/mermaid so it can be previewed.']
    : [];

  const stacksLines = attachedStacks.length > 0
    ? ['', `Knowledge stacks attached: ${attachedStacks.map((s) => `"${s.name}" (${s.description})`).join(', ')}. Use search_docs to consult them, and cite source paths when you use what it returns.`]
    : [];

  const docChatLines = docChatMode
    ? ['', 'Doc-chat mode is on: before each of your replies, the most relevant passages from the attached knowledge stack(s) are retrieved automatically and added as a "[Sources]" system notice — read them and answer using only what they (or the rest of the conversation) actually support, citing the specific source path for every claim drawn from them. If they don\'t contain the answer, say so instead of guessing.']
    : [];

  const subagentGuidanceLines = subagentGuidanceAvailable
    ? ['', "For broad multi-file exploration or an independent scoped subtask, delegate via the task tool (profile 'explore' for read-only research; give it a fully self-contained prompt — it cannot see this conversation)."]
    : [];

  const planModeLines = mode === 'plan'
    ? [
        '',
        '## Plan Mode',
        "You are in Plan Mode: read_file, glob, grep, and list_dir work normally, but every other tool call — including write_file, edit_file, run_shell, remember, web_fetch, and web_search — is blocked and will return an error if you try. Investigate using only the read-only filesystem tools first, then call present_plan exactly once with your proposed plan (a short title, the plan itself as Markdown, and any open_questions worth asking) — then stop and wait for the user to approve it.",
        ...(subagentGuidanceAvailable ? ['Delegating read-only research with the task/workflow tools still works in Plan Mode, but only with profile "explore" (or a read-only custom agent) — a "code"-profile agent is refused without running.'] : []),
      ]
    : [];

  return [
    'You are Little Monkey, a coding agent running inside a desktop app on the user\'s machine.',
    `The user's operating system is ${osLabel}.`,
    '',
    ...workspaceLines,
    '',
    'You have tools to read, search, and modify files in the workspace and to run shell commands. Guidance:',
    '- Gather context before acting: use glob to find files by name, grep to search content, read_file before editing.',
    '- Prefer edit_file (exact unique old_string -> new_string) for changes to existing files; use write_file only for new files or full rewrites.',
    '- Outside Plan Mode, when the user explicitly asks you to create or change workspace files, make the change with edit_file or write_file. A code block in chat is not a substitute for editing the real file, and you must not claim files changed unless a mutating tool succeeded.',
    '- Mutating tools (write_file, edit_file, run_shell, remember) may prompt the user for permission and can be denied — if denied, stop and ask rather than retrying.',
    '- Paths must stay inside the workspace; commands run with a 120-second timeout.',
    '- After making changes, verify them when practical (re-read the file, or run the project\'s tests/build via run_shell).',
    ...rulesLines,
    ...standardsLines,
    ...factsLines,
    ...mcpLines,
    ...rememberGuidanceLines,
    ...webToolsLines,
    ...verifyGuidanceLines,
    ...artifactGuidanceLines,
    ...stacksLines,
    ...docChatLines,
    ...subagentGuidanceLines,
    ...planModeLines,
    '',
    'Keep answers concise. Reference files by their workspace-relative path. When a task is complete, summarize what changed and stop calling tools.',
  ].join('\n');
}

/** The system prompt for the app's current workspace state. `taskText` and
 * `fileHints` drive bounded standards selection; callers that do not have a
 * concrete task keep the historical behavior and inject no standards. */
export function currentSystemPrompt(
  personaId: string | null = null,
  attachedStacks: AttachedStackPromptInfo[] = [],
  docChatMode: boolean = false,
  taskText: string = '',
  fileHints: string[] = [],
): string {
  const roots = useWorkspaceStore.getState().roots;
  const osLabel = detectOsLabel(typeof navigator !== 'undefined' ? navigator.platform : '');
  const { rules, facts } = useRulesStore.getState();
  const mcpServers: McpServerPromptInfo[] = useMcpStore
    .getState()
    .servers.filter((server) => server.status === 'connected' && !!server.instructions?.trim())
    .map((server) => ({ label: server.label, instructions: server.instructions as string }));
  const webToolsAvailable = useSettingsStore.getState().webToolsEnabled;
  const verifyGuidanceAvailable = useSettingsStore.getState().verifyEnabled && useVerifyStore.getState().config.commands.some((c) => c.enabled);
  const mode = usePermissionStore.getState().mode;
  const subagentGuidanceAvailable = useSettingsStore.getState().subagentsEnabled;
  const applicableStandardsSection = taskText.trim()
    ? standardsPromptSection(useStandardsStore.getState().preview(taskText, fileHints))
    : '';
  const base = buildSystemPrompt(roots, osLabel, {
    rules,
    facts,
    mcpServers,
    webToolsAvailable,
    verifyGuidanceAvailable,
    mode,
    artifactGuidanceAvailable: true,
    attachedStacks,
    docChatMode,
    subagentGuidanceAvailable,
    applicableStandardsSection,
  });
  const persona = resolvePersona(usePromptStore.getState().entries, personaId);
  return composeSystemPrompt(base, persona);
}

/**
 * The system prompt seeded into a subagent's own local message history. The
 * parent coordinator is responsible for passing relevant approved standards
 * inside the scoped task description; this keeps child prompts bounded and
 * avoids handing every worker the full standards database.
 */
export function buildSubagentSystemPrompt(
  roots: PromptWorkspaceRoot[],
  osLabel: string,
  profile: 'explore' | 'code',
  description: string,
  custom?: { name: string; tools: string[]; addendum: string }
): string {
  const primary = roots.find((r) => r.is_primary) ?? null;
  const secondaries = roots.filter((r) => !r.is_primary);
  const workspaceLines = primary
    ? [
        `The primary workspace folder is "${primary.path}". Tool paths are relative to it.`,
        ...(secondaries.length > 0 ? [`Additional attached folders (address them by prefixing paths with their label): ${secondaries.map((r) => `"${r.label}" (${r.path})`).join(', ')}.`] : []),
      ]
    : ['No workspace folder is open yet. Tools will fail until the user opens one — say so in your report instead of retrying.'];

  const toolLines = custom
    ? [`You have exactly these tools: ${custom.tools.join(', ')}. Calling any other tool fails.${profile === 'code' ? ' Mutating tools may prompt the user for permission and can be denied — if denied, stop and report that instead of retrying.' : ''}`]
    : profile === 'code'
      ? ['You have read-only tools (read_file, list_dir, glob, grep) plus write_file, edit_file, and run_shell to make changes. Mutating tools may prompt the user for permission and can be denied — if denied, stop and report that instead of retrying.']
      : ['You have read-only tools only: read_file, list_dir, glob, grep. You cannot write or edit files, or run shell commands.'];

  return [
    "You are a subagent spawned by a coordinating AI agent to complete one scoped task, running inside a desktop app on the user's machine.",
    `The user's operating system is ${osLabel}.`,
    '',
    ...workspaceLines,
    '',
    `Your task: ${description}`,
    ...toolLines,
    ...(custom && custom.addendum.length > 0 ? ['', `## Agent instructions (${custom.name})`, custom.addendum] : []),
    '',
    'Complete the task, then reply with a final report of what you found or did. Your reply is returned to the coordinating agent, not shown directly to the user — do not ask questions; if you get blocked, report what you found and why you stopped, then stop.',
  ].join('\n');
}

export const ULTRACODE_SYSTEM_SECTION = [
  '## Ultracode',
  'Ultracode is on for this turn: the user has explicitly opted into multi-agent orchestration and maximum thoroughness. Producing the most exhaustive, correct answer takes priority over token cost.',
  "- For every substantive task, decompose the work and delegate scoped subtasks via the `task` tool. Issue multiple `task` calls in the same turn so they run in parallel — use `explore`-profile subagents for research and investigation fan-out, and `code`-profile subagents for independent, disjoint implementation subtasks.",
  '- Adversarially verify: before committing to a nontrivial finding or change, spawn independent subagents to try to refute it or to check it from a different angle (correctness, edge cases, callers you may have missed), and reconcile disagreements yourself.',
  '- After the fan-out, ask what is still missing — an unexplored area, an unverified claim — and run another round if the answer is not "nothing".',
  '- Work solo only on conversational replies or trivial mechanical edits.',
].join('\n');
