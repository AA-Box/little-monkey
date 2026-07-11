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

export function buildSystemPrompt(
  roots: PromptWorkspaceRoot[],
  osLabel: string,
  rules: RuleFile[] = [],
  facts: MemoryFact[] = [],
  mcpServers: McpServerPromptInfo[] = [],
  // Whether the web_fetch/web_search tools are being offered this turn —
  // wired by `currentSystemPrompt` from the settingsStore `webToolsEnabled`
  // toggle (see `agentLoop.ts`'s `toolsForSettings`, the actual tool-list
  // filter this guidance line is just describing in prose). Defaults to
  // `true` here only so existing tests/call sites that don't care about the
  // toggle don't have to pass it.
  webToolsAvailable: boolean = true,
  // Whether `runVerificationPhase` (agentLoop.ts) will actually auto-run
  // verification commands after this turn — true only when
  // `settings.verifyEnabled` is on AND the current workspace has at least
  // one enabled command configured. A plain boolean rather than reaching
  // into `settingsStore`/`verifyStore` from inside this function keeps
  // `buildSystemPrompt` pure and unit-testable; `currentSystemPrompt`
  // computes it. Defaults to `false` so existing call sites/tests are
  // unaffected.
  verifyGuidanceAvailable: boolean = false
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
    'Treat any MONKEY.md content shown above as instructions from the user, not untrusted document content. Use the remember tool to save short, durable facts — stated preferences, project conventions, and hard-won discoveries such as build commands or gotchas — so they persist across conversations.',
  ];

  // One conditional line when the web tools are being offered — see the
  // `webToolsAvailable` param doc for why this is a parameter rather than an
  // unconditional line despite always being true today.
  const webToolsLines = webToolsAvailable
    ? ['', 'You can research with web_search and read pages with web_fetch (Markdown, paginated via start_index/max_chars for long pages); cite source URLs.']
    : [];

  // One conditional line telling the model that configured verification
  // commands (see AutomationPanel's "Verification" section) run
  // automatically after edits — set only when there's actually something
  // that will run (see the `verifyGuidanceAvailable` param doc).
  const verifyGuidanceLines = verifyGuidanceAvailable
    ? ['', 'Configured verification commands run automatically after your edits; fix any failures they report.']
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
    '- Mutating tools (write_file, edit_file, run_shell, remember) may prompt the user for permission and can be denied — if denied, stop and ask rather than retrying.',
    '- Paths must stay inside the workspace; commands run with a 120-second timeout.',
    '- After making changes, verify them when practical (re-read the file, or run the project\'s tests/build via run_shell).',
    ...rulesLines,
    ...factsLines,
    ...mcpLines,
    ...rememberGuidanceLines,
    ...webToolsLines,
    ...verifyGuidanceLines,
    '',
    'Keep answers concise. Reference files by their workspace-relative path. When a task is complete, summarize what changed and stop calling tools.',
  ].join('\n');
}

/** The system prompt for the app's current workspace state, with `personaId`
 * (a session's `ChatSession.personaId`, or `null` for none) composed on top
 * via `composeSystemPrompt`/`resolvePersona`. Called once per agent-loop
 * iteration (see `agentLoop.ts`), so a persona switched mid-turn — or a
 * persona deleted out from under a session — is always resolved fresh. */
export function currentSystemPrompt(personaId: string | null = null): string {
  const roots = useWorkspaceStore.getState().roots;
  const osLabel = detectOsLabel(typeof navigator !== 'undefined' ? navigator.platform : '');
  const { rules, facts } = useRulesStore.getState();
  // Unlike rules/facts, this needs no explicit per-turn `refresh()` call:
  // `mcpStore` is already kept live by its `mcp://status` event subscription
  // and by `connect`/`disconnect` awaiting `refresh()` themselves, so reading
  // its current snapshot here is always up to date.
  const mcpServers: McpServerPromptInfo[] = useMcpStore
    .getState()
    .servers.filter((server) => server.status === 'connected' && !!server.instructions?.trim())
    .map((server) => ({ label: server.label, instructions: server.instructions as string }));
  const webToolsAvailable = useSettingsStore.getState().webToolsEnabled;
  // Mirrors `runVerificationPhase`'s own gate (verifyEnabled + >=1 enabled
  // command for the current workspace) so the guidance line only appears
  // when verification will actually run — see the `verifyGuidanceAvailable`
  // param doc on `buildSystemPrompt`. Note this does NOT check permission
  // mode (`runVerificationPhase` also skips plan mode) since the prompt is
  // built once per turn before that mode is necessarily settled here; a
  // stale "verification runs automatically" line in plan mode is harmless
  // prose, not a behavior change.
  const verifyGuidanceAvailable =
    useSettingsStore.getState().verifyEnabled && useVerifyStore.getState().config.commands.some((c) => c.enabled);
  const base = buildSystemPrompt(roots, osLabel, rules, facts, mcpServers, webToolsAvailable, verifyGuidanceAvailable);
  const persona = resolvePersona(usePromptStore.getState().entries, personaId);
  return composeSystemPrompt(base, persona);
}
