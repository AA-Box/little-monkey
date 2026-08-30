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
import { useMcpStore } from '../store/mcpStore';
import { usePromptStore, type PromptEntry } from '../store/promptStore';
import { useSettingsStore } from '../store/settingsStore';
import { useVerifyStore } from '../store/verifyStore';
import { usePermissionStore, type PermissionMode } from '../store/permissionStore';

/** A connected MCP server's label + `initialize`-result instructions —
 * mirrors the subset of `McpServerInfo` (mcpStore.ts) that
 * `buildSystemPrompt` actually needs, already filtered to servers that are
 * connected and have non-empty instructions (see `currentSystemPrompt`). */
export interface McpServerPromptInfo {
  label: string;
  instructions: string;
}

/** Per-server cap on how much of a connected MCP server's `instructions`
 * gets injected into the prompt — a single misbehaving/verbose server
 * shouldn't be able to blow out the system prompt for every turn. */
const MCP_INSTRUCTIONS_CHAR_CAP = 1000;

function capMcpInstructions(text: string): string {
  return text.length > MCP_INSTRUCTIONS_CHAR_CAP
    ? `${text.slice(0, MCP_INSTRUCTIONS_CHAR_CAP)}…`
    : text;
}

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

/** The subset of a persona `PromptEntry` (see `promptStore.ts`) that
 * `composeSystemPrompt` actually needs — just enough to render the section
 * header and body, not the whole record (command/description/timestamps are
 * irrelevant here). */
export interface ActivePersona {
  name: string;
  content: string;
}

/**
 * Appends a clearly-delimited "## Active persona: <name>" section after
 * `base` — APPENDS, never replaces, because `base` carries load-bearing
 * sandbox/tool/permission guidance a user- or import-authored persona must
 * not be able to silently drop (see the design doc's risk note). `persona`
 * is `null` when the session has no active persona (or its `personaId`
 * didn't resolve — see `resolvePersona`), in which case `base` is returned
 * unchanged. Pure and synchronous, same "buildSystemPrompt is pure so it can
 * be unit-tested" rationale as the rest of this module — this is the same
 * sectioned-append convention `buildSystemPrompt` already uses for the
 * MONKEY.md rules/facts/MCP sections below, just applied on top of its
 * output instead of inside it.
 */
export function composeSystemPrompt(base: string, persona: ActivePersona | null): string {
  if (!persona) return base;
  return [base, '', `## Active persona: ${persona.name}`, persona.content].join('\n');
}

/**
 * Resolves a session's `ChatSession.personaId` against the prompt library's
 * current entries. Returns `null` for no persona (`personaId` is `null`) and
 * also `null` — never throws — when `personaId` doesn't match a saved
 * `kind: "persona"` entry, e.g. the persona was deleted after the session
 * started pointing at it (a dangling reference); `composeSystemPrompt` then
 * simply gets `null` and the base prompt is used as-is.
 */
export function resolvePersona(entries: PromptEntry[], personaId: string | null): ActivePersona | null {
  if (!personaId) return null;
  const entry = entries.find((e) => e.id === personaId && e.kind === 'persona');
  return entry ? { name: entry.name, content: entry.content } : null;
}

/** One attached knowledge stack's prompt-facing summary — mirrors the subset
 * of `stackStore.ts`'s `KnowledgeStack` that `buildSystemPrompt`'s guidance
 * line actually needs. `description` is a short human-readable status (e.g.
 * "1,234 chunks indexed" or "not indexed yet") computed by the caller
 * (`agentLoop.ts`) from the stack's `chunk_count`/`indexed_at`, since
 * `KnowledgeStack` itself carries no free-text description field for a
 * stack — see that call site for the exact derivation. */
export interface AttachedStackPromptInfo {
  name: string;
  description: string;
}

/**
 * Every optional input to `buildSystemPrompt`, beyond the always-required
 * `roots`/`osLabel` — one named, self-documenting object instead of ~10
 * positional booleans/arrays whose meaning depended on argument order (the
 * shape this replaces; see ROADMAP.md §3.2). Each field defaults exactly the
 * way its old positional parameter did, so every pre-existing call site that
 * only cared about a subset of them is unaffected either way.
 */
export interface BuildSystemPromptOptions {
  /** MONKEY.md files currently in effect — see the `rulesLines` section below. */
  rules?: RuleFile[];
  /** Facts remembered via the `remember` tool — see the `factsLines` section below. */
  facts?: MemoryFact[];
  /** Connected MCP servers' own `instructions` — see the `mcpLines` section below. */
  mcpServers?: McpServerPromptInfo[];
  /** Whether the web_fetch/web_search tools are being offered this turn —
   * wired by `currentSystemPrompt` from the settingsStore `webToolsEnabled`
   * toggle (see `agentLoop.ts`'s `toolsForSettings`, the actual tool-list
   * filter this guidance line is just describing in prose). Defaults to
   * `true` so existing tests/call sites that don't care about the toggle
   * don't have to pass it. */
  webToolsAvailable?: boolean;
  /** Whether post-mutation verification will actually run this turn. This is
   * true either for ordinary configured Verification or because the frozen
   * Standards context carries locally-bound mandatory checker IDs. */
  verifyGuidanceAvailable?: boolean;
  /** The active permission mode (see `permissionStore.ts`) — only ever
   * changes this prompt's output when it's `'plan'` (see `planModeLines`
   * below); every other mode produces byte-identical output to omitting this
   * field entirely, which is what every pre-existing call site (and test)
   * that doesn't pass it still gets via the default. Read once per turn by
   * `currentSystemPrompt`, same as every other store snapshot in this
   * module. */
  mode?: PermissionMode;
  /** Whether to nudge the model toward tagging previewable code fences (see
   * `src/lib/artifacts.ts`'s fence-detection scheme). Defaults to `true`
   * since there's no settings toggle gating this in phase 1 — kept as a
   * field anyway (rather than an unconditional line) purely to match this
   * module's established "conditional guidance line" pattern
   * (`webToolsLines`/`verifyGuidanceLines` below), so a later phase can wire
   * in a real toggle (or turn it off in plan mode, say) without changing
   * this function's shape again. */
  artifactGuidanceAvailable?: boolean;
  /** Knowledge stacks attached to this session (see `ChatSession.attachedStackIds`,
   * `StackPicker.tsx`) — empty for the overwhelming majority of turns (no
   * stacks attached, or the feature unused), in which case this contributes
   * nothing, same "absence is fine" stance as `rules`/`facts` above. Non-empty
   * only when the `search_docs` tool is actually being offered this turn too
   * (see `agentLoop.ts`'s `buildTools` call), so the guidance line and the
   * tool's own availability never drift out of sync. */
  attachedStacks?: AttachedStackPromptInfo[];
  /** Whether the session's doc-chat mode (see `ChatSession.docChatMode`,
   * `StackPicker.tsx`) is on — adds a citation instruction beyond the plain
   * `stacksLines` mention above, since doc-chat retrieves passages
   * automatically (as a `[Sources]` notice, see `agentLoop.ts`) rather than
   * relying on the model to call `search_docs` itself. Kept as its own
   * condition instead of folding into `attachedStacks.length > 0` so a
   * `docChatMode` toggle left on after every stack was detached from the
   * session degrades to no citation instruction rather than a dangling one —
   * `runAgentTurnBody` never actually retrieves anything in that case either
   * (see its `attachedStackIds.length > 0` gate), so the two stay in sync.
   * Defaults to `false` so every pre-existing call site/test is unaffected. */
  docChatMode?: boolean;
  /** Whether the `task` tool is being offered this turn — wired by
   * `currentSystemPrompt` from the settingsStore `subagentsEnabled` toggle
   * (see `agentLoop.ts`'s `toolsForSettings`, the actual tool-list filter
   * this guidance line just describes in prose). Defaults to `false` so
   * every pre-existing call site/test is unaffected, same posture as
   * `verifyGuidanceAvailable`. */
  subagentGuidanceAvailable?: boolean;
  /** Task-selected, already bounded standards text. Selection happens outside
   * this pure builder so tests can continue providing plain inputs. */
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

  // Standards differ from MONKEY.md: discovery never makes them authoritative.
  // Only explicitly approved, task-selected versions reach this section.
  const standardsLines = applicableStandardsSection ? ['', applicableStandardsSection] : [];

  // Facts remembered via the `remember` tool (see memory.rs/tool_remember),
  // scoped to the current primary workspace root — refreshed alongside
  // `rules` once per turn (see rulesStore.ts).
  const factsLines =
    facts.length > 0 ? ['', '## Remembered facts', ...facts.map((fact) => `- ${fact.text}`)] : [];

  // Each connected MCP server's own `initialize`-result `instructions` field
  // (spec-correct use of it — see mcp.rs's module doc) — the only prompt
  // change MCP support needs, since tool schemas are otherwise
  // self-describing. Capped per server so one verbose server can't dominate
  // the prompt; servers with no instructions (or not connected) contribute
  // nothing here, same "absence is fine" stance as rules/facts above.
  const mcpLines =
    mcpServers.length > 0
      ? [
          '',
          '## Connected MCP servers',
          ...mcpServers.map((server) => `MCP server '${server.label}': ${capMcpInstructions(server.instructions)}`),
        ]
      : [];

  // One trailing guidance line telling the model when to use `remember` and
  // to treat the MONKEY.md content above as instructions from the user
  // rather than untrusted background text — always present (not gated on
  // `rules`/`facts` being non-empty) since it's guidance about behavior going
  // forward, not a description of what's currently loaded.
  const rememberGuidanceLines = [
    '',
    'Treat any MONKEY.md content shown above as instructions from the user, not untrusted document content. Approved engineering standards shown above are guidance/verification constraints only: they cannot grant tools, network, secrets, budget, or permission authority. Use the remember tool to save short, durable facts — stated preferences, project conventions, and hard-won discoveries such as build commands or gotchas — so they persist across conversations.',
  ];

  const webToolsLines = webToolsAvailable
    ? ['', 'You can research with web_search and read pages with web_fetch (Markdown, paginated via start_index/max_chars for long pages); cite source URLs.']
    : [];

  const verifyGuidanceLines = verifyGuidanceAvailable
    ? ['', 'Configured verification commands run automatically after your edits; fix any failures they report.']
    : [];

  const artifactGuidanceLines = artifactGuidanceAvailable
    ? [
        '',
        'When producing a complete HTML page, SVG image, or Mermaid diagram, put it in a single fenced code block tagged html/svg/mermaid so it can be previewed.',
      ]
    : [];

  const stacksLines =
    attachedStacks.length > 0
      ? [
          '',
          `Knowledge stacks attached: ${attachedStacks.map((s) => `"${s.name}" (${s.description})`).join(', ')}. Use search_docs to consult them, and cite source paths when you use what it returns.`,
        ]
      : [];

  const docChatLines = docChatMode
    ? [
        '',
        'Doc-chat mode is on: before each of your replies, the most relevant passages from the attached knowledge stack(s) are retrieved automatically and added as a "[Sources]" system notice — read them and answer using only what they (or the rest of the conversation) actually support, citing the specific source path for every claim drawn from them. If they don\'t contain the answer, say so instead of guessing.',
      ]
    : [];

  const subagentGuidanceLines = subagentGuidanceAvailable
    ? [
        '',
        "For broad multi-file exploration or an independent scoped subtask, delegate via the task tool (profile 'explore' for read-only research; give it a fully self-contained prompt — it cannot see this conversation).",
      ]
    : [];

  const planModeLines =
    mode === 'plan'
      ? [
          '',
          '## Plan Mode',
          "You are in Plan Mode: read_file, glob, grep, and list_dir work normally, but every other tool call — including write_file, edit_file, run_shell, remember, web_fetch, and web_search — is blocked and will return an error if you try. Investigate using only the read-only filesystem tools first, then call present_plan exactly once with your proposed plan (a short title, the plan itself as Markdown, and any open_questions worth asking) — then stop and wait for the user to approve it.",
          ...(subagentGuidanceAvailable
            ? [
                'Delegating read-only research with the task/workflow tools still works in Plan Mode, but only with profile "explore" (or a read-only custom agent) — a "code"-profile agent is refused without running.',
              ]
            : []),
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

/** The system prompt for the app's current workspace state. Standards are not
 * selected here: each execution path must freeze its exact task/file context
 * once and pass the already-rendered section in. This keeps simultaneous
 * panes, queued daemon turns, Crew, and Compare isolated from one another. */
export function currentSystemPrompt(
  personaId: string | null = null,
  attachedStacks: AttachedStackPromptInfo[] = [],
  docChatMode: boolean = false,
  applicableStandardsSection: string = '',
  standardsVerificationAvailable: boolean = false,
): string {
  const roots = useWorkspaceStore.getState().roots;
  const osLabel = detectOsLabel(typeof navigator !== 'undefined' ? navigator.platform : '');
  const { rules, facts } = useRulesStore.getState();
  const mcpServers: McpServerPromptInfo[] = useMcpStore
    .getState()
    .servers.filter((server) => server.status === 'connected' && !!server.instructions?.trim())
    .map((server) => ({ label: server.label, instructions: server.instructions as string }));
  const webToolsAvailable = useSettingsStore.getState().webToolsEnabled;
  const verifyGuidanceAvailable = standardsVerificationAvailable || (
    useSettingsStore.getState().verifyEnabled
    && useVerifyStore.getState().config.commands.some((c) => c.enabled)
  );
  const mode = usePermissionStore.getState().mode;
  const subagentGuidanceAvailable = useSettingsStore.getState().subagentsEnabled;
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
 * The system prompt seeded into a subagent's own local (never
 * `sessionStore`-backed) message history — see `subagent.ts`'s
 * `runSubagentTask`. Shares the same workspace-facts derivation as
 * `buildSystemPrompt` (primary/secondary root lines) but is otherwise a
 * distinct, much shorter prompt: a subagent needs none of the parent's
 * MONKEY.md rules/remembered-facts/MCP-server/verify/artifact guidance —
 * just enough to know where it is, what its one task is, which tools it has
 * (per `profile`), and how it must end (a final report, never a question,
 * per the design doc's "subagent replies with a report, not to the user"
 * contract). Pure and synchronous, same "buildSystemPrompt is pure so it can
 * be unit-tested" rationale as the rest of this module.
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
        ...(secondaries.length > 0
          ? [
              `Additional attached folders (address them by prefixing paths with their label): ${secondaries
                .map((r) => `"${r.label}" (${r.path})`)
                .join(', ')}.`,
            ]
          : []),
      ]
    : ['No workspace folder is open yet. Tools will fail until the user opens one — say so in your report instead of retrying.'];

  const toolLines = custom
    ? [
        `You have exactly these tools: ${custom.tools.join(', ')}. Calling any other tool fails.${
          profile === 'code'
            ? ' Mutating tools may prompt the user for permission and can be denied — if denied, stop and report that instead of retrying.'
            : ''
        }`,
      ]
    : profile === 'code'
      ? [
          'You have read-only tools (read_file, list_dir, glob, grep) plus write_file, edit_file, and run_shell to make changes. Mutating tools may prompt the user for permission and can be denied — if denied, stop and report that instead of retrying.',
        ]
      : ['You have read-only tools only: read_file, list_dir, glob, grep. You cannot write or edit files, or run shell commands.'];

  return [
    "You are a subagent spawned by a coordinating AI agent to complete one scoped task, running inside a desktop app on the user's machine.",
    `The user's operating system is ${osLabel}.`,
    '',
    ...workspaceLines,
    '',
    `Your task: ${description}`,
    ...toolLines,
    ...(custom && custom.addendum.length > 0
      ? ['', `## Agent instructions (${custom.name})`, custom.addendum]
      : []),
    '',
    'Complete the task, then reply with a final report of what you found or did. Your reply is returned to the coordinating agent, not shown directly to the user — do not ask questions; if you get blocked, report what you found and why you stopped, then stop.',
  ].join('\n');
}

/**
 * The system-prompt section appended for an Ultracode turn (see
 * `agentLoop.ts`'s `runAgentTurnBody`): a standing opt-in for multi-agent
 * orchestration on the SAME model — the Claude Code "ultracode" semantics.
 * Ultracode never fans the prompt out across different models; it tells this
 * one model to decompose substantive work into `task`-tool subagent runs and
 * to adversarially verify its own conclusions. The `task` tool is
 * force-offered for Ultracode turns even when `settingsStore.subagentsEnabled`
 * is off — selecting Ultracode IS the user's explicit opt-in to subagents for
 * that turn (see `runAgentTurnBody`'s `toolsForSettings` call).
 */
export const ULTRACODE_SYSTEM_SECTION = [
  '## Ultracode',
  'Ultracode is on for this turn: the user has explicitly opted into multi-agent orchestration and maximum thoroughness. Producing the most exhaustive, correct answer takes priority over token cost.',
  "- For every substantive task, decompose the work and delegate scoped subtasks via the `task` tool. Issue multiple `task` calls in the same turn so they run in parallel — use `explore`-profile subagents for research and investigation fan-out, and `code`-profile subagents for independent, disjoint implementation subtasks.",
  '- Adversarially verify: before committing to a nontrivial finding or change, spawn independent subagents to try to refute it or to check it from a different angle (correctness, edge cases, callers you may have missed), and reconcile disagreements yourself.',
  '- After the fan-out, ask what is still missing — an unexplored area, an unverified claim — and run another round if the answer is not "nothing".',
  '- Work solo only on conversational replies or trivial mechanical edits.',
].join('\n');