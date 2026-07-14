/**
 * OpenAI-style tool (function) definitions handed to the model alongside
 * every chat completion request. Names and parameters mirror the
 * `#[tauri::command]` functions in `src-tauri/src/tools.rs` 1:1 — the agent
 * loop invokes `tool_<name>` for whichever of these the model calls.
 */
import type { ToolDef } from './llamaClient';

export const TOOLS: ToolDef[] = [
  {
    type: 'function',
    function: {
      name: 'read_file',
      description:
        'Read the full text contents of a file in the workspace. Path is resolved relative to the workspace root and must not escape it.',
      parameters: {
        type: 'object',
        properties: {
          path: {
            type: 'string',
            description:
              "Path to the file, relative to the primary workspace folder. If a secondary folder is attached, prefix the path with its label to target it instead, e.g. 'other-folder/src/index.ts'.",
          },
        },
        required: ['path'],
        additionalProperties: false,
      },
    },
  },
  {
    type: 'function',
    function: {
      name: 'write_file',
      description:
        'Create or overwrite a file in the workspace with the given content, creating parent directories as needed. Requires user permission.',
      parameters: {
        type: 'object',
        properties: {
          path: {
            type: 'string',
            description:
              "Path to the file, relative to the primary workspace folder. If a secondary folder is attached, prefix the path with its label to target it instead, e.g. 'other-folder/src/index.ts'.",
          },
          content: {
            type: 'string',
            description: 'The full new contents of the file.',
          },
        },
        required: ['path', 'content'],
        additionalProperties: false,
      },
    },
  },
  {
    type: 'function',
    function: {
      name: 'edit_file',
      description:
        'Replace a single unique occurrence of old_string with new_string in an existing file. Fails if old_string is not found or is not unique. Requires user permission.',
      parameters: {
        type: 'object',
        properties: {
          path: {
            type: 'string',
            description:
              "Path to the file, relative to the primary workspace folder. If a secondary folder is attached, prefix the path with its label to target it instead, e.g. 'other-folder/src/index.ts'.",
          },
          old_string: {
            type: 'string',
            description: 'The exact, unique text to find in the file.',
          },
          new_string: {
            type: 'string',
            description: 'The text to replace old_string with.',
          },
        },
        required: ['path', 'old_string', 'new_string'],
        additionalProperties: false,
      },
    },
  },
  {
    type: 'function',
    function: {
      name: 'list_dir',
      description:
        'List the entries of a directory in the workspace, returning each entry\'s name, whether it is a directory, and its size in bytes.',
      parameters: {
        type: 'object',
        properties: {
          path: {
            type: 'string',
            description:
              "Path to the directory, relative to the primary workspace folder. If a secondary folder is attached, prefix the path with its label to target it instead, e.g. 'other-folder/src'.",
          },
        },
        required: ['path'],
        additionalProperties: false,
      },
    },
  },
  {
    type: 'function',
    function: {
      name: 'glob',
      description:
        'Find files by glob pattern (e.g. "**/*.ts", "src/**/test_*.py"), skipping VCS/build/dependency directories, returning workspace-relative paths sorted by most recently modified first, capped at 300 results.',
      parameters: {
        type: 'object',
        properties: {
          pattern: {
            type: 'string',
            description: 'Glob pattern to match file paths against, e.g. "**/*.rs" or "src/components/**".',
          },
          path: {
            type: 'string',
            description:
              "Optional directory to scope the search to, relative to the primary workspace folder. Defaults to the whole primary folder. To search a secondary attached folder, pass its label, e.g. 'other-folder'.",
          },
        },
        required: ['pattern'],
        additionalProperties: false,
      },
    },
  },
  {
    type: 'function',
    function: {
      name: 'grep',
      description:
        'Search for a regular expression pattern across files in the workspace (skipping .git, node_modules, target, and dist), returning matching file, line number, and line text, capped at 200 matches.',
      parameters: {
        type: 'object',
        properties: {
          pattern: {
            type: 'string',
            description: 'Regular expression pattern to search for.',
          },
          path: {
            type: 'string',
            description:
              "Optional directory or file to scope the search to, relative to the primary workspace folder. Defaults to the whole primary folder. To search a secondary attached folder, pass its label (or a label-prefixed subpath), e.g. 'other-folder'.",
          },
        },
        required: ['pattern'],
        additionalProperties: false,
      },
    },
  },
  {
    type: 'function',
    function: {
      name: 'run_shell',
      description:
        'Run a shell command via `sh -c` in the workspace (or a subdirectory of it), with a 120 second timeout. Returns stdout, stderr, and exit code. Requires user permission.',
      parameters: {
        type: 'object',
        properties: {
          command: {
            type: 'string',
            description: 'The shell command to execute.',
          },
          cwd: {
            type: 'string',
            description:
              "Optional working directory, relative to the primary workspace folder. Defaults to the primary folder's root. If a secondary folder is attached, prefix the path with its label to run inside it instead, e.g. 'other-folder'.",
          },
        },
        required: ['command'],
        additionalProperties: false,
      },
    },
  },
  {
    type: 'function',
    function: {
      name: 'remember',
      description:
        "Save a short durable fact about this project or the user's preferences so future conversations remember it. Use for stated preferences, project conventions, and hard-won discoveries (build commands, gotchas). Requires user permission.",
      parameters: {
        type: 'object',
        properties: {
          text: {
            type: 'string',
            description: 'The fact to remember, written as a short standalone statement (max 500 characters).',
          },
        },
        required: ['text'],
        additionalProperties: false,
      },
    },
  },
  {
    type: 'function',
    function: {
      name: 'web_fetch',
      description:
        'Fetch a web page (or plain text/Markdown/JSON/XML document) by URL and return its content as Markdown, with the page title and final URL (after redirects). Long content is windowed to max_chars starting at start_index; the result reports total_chars and truncated so you can page through the rest with a later call. Requires user permission.',
      parameters: {
        type: 'object',
        properties: {
          url: {
            type: 'string',
            description: 'The http(s) URL to fetch.',
          },
          max_chars: {
            type: 'integer',
            description: 'Maximum characters of content to return in this call (default 20000).',
          },
          start_index: {
            type: 'integer',
            description: 'Character offset into the full content to start the returned window at (default 0). Use with total_chars/truncated from a previous call to page through a long page.',
          },
        },
        required: ['url'],
        additionalProperties: false,
      },
    },
  },
  {
    type: 'function',
    function: {
      name: 'web_search',
      description:
        'Search the web (keyless DuckDuckGo by default) and return up to `count` ranked results, each with a title, url, and snippet. Follow up with web_fetch to read a result in full. Requires user permission.',
      parameters: {
        type: 'object',
        properties: {
          query: {
            type: 'string',
            description: 'The search query.',
          },
          count: {
            type: 'integer',
            description: 'Number of results to return, 1-10 (default 10).',
          },
        },
        required: ['query'],
        additionalProperties: false,
      },
    },
  },
];

/**
 * Builds the `search_docs` tool's description, naming the actual attached
 * stacks so the model knows what's searchable without a separate lookup
 * call — mirrors `stack: Option<String>`'s resolution in
 * `stacks.rs::resolve_search_stack_ids`: pass one of these names to search
 * just that stack, or omit it to search every indexed stack (the model's own
 * visible universe here is exactly the attached stacks this description
 * lists, even though the Rust side itself has no separate notion of
 * "attached" — see that function's doc comment for why).
 */
function searchDocsDescription(attachedStackNames: string[]): string {
  const stackList = attachedStackNames.join(', ');
  return `Search the attached knowledge stack(s) for passages relevant to a query, returning the top matches with their source file path and a relevance score. Attached stacks: ${stackList}. Pass "stack" to search only one of them by name, or omit it to search across all of them. Cite source paths when using results in your answer.`;
}

/**
 * A `search_docs` `ToolDef` naming `attachedStackNames` in its description —
 * built fresh per turn (see `buildTools` below) rather than a fixed constant,
 * since the whole point is that the model sees the actual current stack
 * names, not a generic placeholder.
 */
function searchDocsTool(attachedStackNames: string[]): ToolDef {
  return {
    type: 'function',
    function: {
      name: 'search_docs',
      description: searchDocsDescription(attachedStackNames),
      parameters: {
        type: 'object',
        properties: {
          query: {
            type: 'string',
            description: 'The search query — a question or phrase to find relevant passages for.',
          },
          stack: {
            type: 'string',
            description: 'Optional: the name of one specific attached stack to search. Omit to search across all attached stacks.',
          },
          max_results: {
            type: 'integer',
            description: 'Maximum number of passages to return (default 6).',
          },
        },
        required: ['query'],
        additionalProperties: false,
      },
    },
  };
}

/**
 * Wraps the base `TOOLS` array with `search_docs` appended, ONLY when at
 * least one knowledge stack is attached to the session (`attachedStackNames`
 * non-empty) — an unattached session has nothing for the tool to search, so
 * offering it would just invite a confusing "no stacks" error. Called once
 * per turn by `agentLoop.ts`'s `runAgentTurnBody`, the same place
 * `toolsForMode`/`toolsForSettings` already shape the per-turn tool list —
 * see that module's doc comment for where this slots into the composition
 * chain (`toolsForSettings(toolsForMode(buildTools(...), mode), ...)`).
 */
export function buildTools(attachedStackNames: string[]): ToolDef[] {
  return attachedStackNames.length > 0 ? [...TOOLS, searchDocsTool(attachedStackNames)] : TOOLS;
}

/**
 * A frontend-only tool: presenting a structured plan for the user to
 * approve before switching out of Plan Mode (see `agentLoop.ts`'s
 * `toolsForMode`/`PLAN_NOTE_PREFIX`/`PlanNotice`). Deliberately kept OUT of
 * the `TOOLS` array above — it is appended to the per-turn tool list only
 * while the active permission mode is `'plan'` — and, unlike every other
 * entry in `TOOLS`, it has NO `tool_present_plan` counterpart in
 * `src-tauri/src/tools.rs`: `turnEngine.ts`'s `executeToolCall` short-circuits
 * this name before it ever reaches `invoke`. This is an intentional
 * three-way-registry-drift exception (TS tools.ts / Rust tools.rs / monkey-cli
 * tools_def.rs normally mirror each other 1:1 — see this module's top
 * doc comment) called out explicitly in the Plan/Act design doc
 * (docs/roadmap/p2-plan-act-safety.md) as a known, accepted risk: a reader
 * scanning tools.rs for `present_plan` and finding nothing should look here,
 * not assume a missing Rust command.
 */
/**
 * Tool names offered to a subagent's own tool-calling loop, keyed by
 * `profile` — see `subagent.ts`'s `runSubagentTask` and the "Restricted tool
 * sets" section of `docs/roadmap/p3-subagents.md`. `explore` is every
 * ungated read-only tool (a subagent that can only read needs no new trust
 * beyond what the parent already has); `code` (slice 3) adds the mutating
 * tools, which still go through the exact same Rust commands and permission
 * gate as the parent's own calls.
 *
 * CRITICAL: `'task'` must never appear in either list — this is what caps
 * delegation depth at 1 structurally (a subagent can never spawn another
 * subagent) rather than via a runtime recursion guard. `TASK_TOOL` is kept
 * as its own constant below, deliberately never added to `TOOLS`, so there
 * is no name in `TOOLS` for these filters to ever accidentally let through.
 * See `tools.test.ts` for the test proving this by construction.
 */
const EXPLORE_PROFILE_TOOL_NAMES: ReadonlySet<string> = new Set(['read_file', 'list_dir', 'glob', 'grep']);
const CODE_PROFILE_TOOL_NAMES: ReadonlySet<string> = new Set([...EXPLORE_PROFILE_TOOL_NAMES, 'write_file', 'edit_file', 'run_shell']);

export function toolsForProfile(profile: 'explore' | 'code'): ToolDef[] {
  const names = profile === 'code' ? CODE_PROFILE_TOOL_NAMES : EXPLORE_PROFILE_TOOL_NAMES;
  return TOOLS.filter((tool) => names.has(tool.function.name));
}

/**
 * The `task` tool: delegates a scoped subtask to a subagent that runs its
 * own isolated tool-calling loop (see `subagent.ts`'s `runSubagentTask`) and
 * returns only a final report to the parent turn — the child's own
 * exploration noise never touches the parent's context. Deliberately kept
 * OUT of the `TOOLS` array above (see `toolsForProfile`'s doc comment for
 * why) and only appended to the per-turn tool list by `agentLoop.ts`'s
 * `toolsForSettings` when `settingsStore.subagentsEnabled` is on — a weak
 * local model that never had this toggle turned on should never even see
 * the schema.
 *
 * `profile` allows both `'explore'` and `'code'` as of slice 3 — a `'code'`
 * subagent can write/edit/run shell through the exact same permission gate
 * and checkpoint hooks as the parent (see `executeToolCall`'s `task` branch
 * and `subagent.ts`'s `runSubagentTask` for the parent-checkpoint-id +
 * child-own-turn-id pairing that makes this safe). `executeToolCall`
 * (`turnEngine.ts`) intercepts this name before the `invoke('tool_'+name)`
 * dispatch, exactly like `present_plan` — it has no `tool_task` Rust command
 * either.
 */
export const TASK_TOOL: ToolDef = {
  type: 'function',
  function: {
    name: 'task',
    description:
      'Delegate a scoped subtask to a subagent with its own isolated tool-calling loop and restricted tool set. The subagent cannot see this conversation, so give it a fully self-contained prompt. Only its final report is returned to you — use this for broad exploration or an independent subtask you want kept out of your own context. Multiple task calls in the same turn may run in parallel.',
    parameters: {
      type: 'object',
      properties: {
        description: {
          type: 'string',
          description: 'A short (3-6 word) label for this subtask, shown to the user.',
        },
        prompt: {
          type: 'string',
          description: "Full, self-contained instructions for the subagent — it has no access to this conversation, so include all necessary context (file paths, what to look for, what to report back).",
        },
        profile: {
          type: 'string',
          enum: ['explore', 'code'],
          description:
            "Tool access profile for the subagent. 'explore' gives read-only tools (read_file, list_dir, glob, grep) — use it for research and investigation. 'code' additionally allows write_file, edit_file, and run_shell — its edits land in this turn's own checkpoint and go through the same permission prompts as your own edits — use it for an independent, disjoint implementation subtask.",
        },
      },
      required: ['description', 'prompt', 'profile'],
      additionalProperties: false,
    },
  },
};

/**
 * The `skill` tool: lets the model invoke one of the skills listed in the
 * turn's "## Available skills" catalog (see `skills.ts`'s
 * `composeSkillCatalog`) on its own initiative, instead of only ever loading
 * a skill the user explicitly typed `/command` for. Deliberately kept OUT of
 * the `TOOLS` array above (only appended to the per-turn tool list by
 * `agentLoop.ts`'s `toolsForSettings` when `settingsStore.skillAutoInvokeEnabled`
 * is on AND at least one skill remains uninvoked this turn — a user who
 * hasn't opted in should never even see the schema, same posture as
 * `TASK_TOOL`'s `subagentsEnabled` gate). Frontend-only, same as `TASK_TOOL`/
 * `PRESENT_PLAN_TOOL`: it has no `tool_skill` Rust command — `turnEngine.ts`'s
 * `executeToolCall` intercepts this name before the `invoke` dispatch and
 * resolves it against the turn's own `SkillToolContext.availableSkills`
 * instead, returning the matched skill's instructions (and any bundled
 * `resource_files` listing) as the tool result.
 */
export const SKILL_INVOKE_TOOL: ToolDef = {
  type: 'function',
  function: {
    name: 'skill',
    description:
      "Invoke one of the skills listed in the \"## Available skills\" section of the system prompt by its command name, when it matches what the user is asking for. The skill's full instructions are returned as this call's result — apply them for the rest of this turn. Do not invoke a skill the request doesn't actually need, and never invoke the same skill twice in one turn.",
    parameters: {
      type: 'object',
      properties: {
        command: {
          type: 'string',
          description: 'The skill\'s command name, without the leading slash, exactly as listed in the catalog (e.g. "review", not "/review").',
        },
        arguments: {
          type: 'string',
          description: 'Optional free-text arguments/context to pass to the skill — usually the relevant part of the user\'s request.',
        },
      },
      required: ['command'],
      additionalProperties: false,
    },
  },
};

/**
 * The `read_skill_resource` tool: reads one bundled file (other than
 * `SKILL.md` itself) from a native skill's folder — the progressive-
 * disclosure counterpart to a skill's `resource_files` listing (see
 * `skills.ts`'s `SlashSkill.resourceFiles` and the instructions block
 * `composeSkillSystemPrompt`/the `skill` tool's result both append it to).
 * Unlike `SKILL_INVOKE_TOOL`, this DOES have a real Rust command
 * (`tool_read_skill_resource` in `src-tauri/src/tools.rs`, delegating to
 * `NativeSkillManager::read_resource`), so it flows through the ordinary
 * `invoke('tool_' + name, args)` dispatch in `turnEngine.ts` — no special
 * interception needed. Appended to the per-turn tool list by
 * `agentLoop.ts`'s `toolsForSettings` whenever any currently available skill
 * has at least one bundled resource file, independent of
 * `skillAutoInvokeEnabled` — explicit `/command` invocation should be able to
 * read bundled files too, not just an auto-invoked skill.
 */
export const READ_SKILL_RESOURCE_TOOL: ToolDef = {
  type: 'function',
  function: {
    name: 'read_skill_resource',
    description:
      "Read one bundled file from a skill's folder, by the skill's command name and the file's path as listed in its \"Bundled files\" line. Only works for a skill that has already been invoked (explicitly or via the skill tool) this turn.",
    parameters: {
      type: 'object',
      properties: {
        command: {
          type: 'string',
          description: 'The skill\'s command name, without the leading slash.',
        },
        path: {
          type: 'string',
          description: 'The bundled file\'s path, exactly as listed in the skill\'s "Bundled files" line (e.g. "references/info.md").',
        },
      },
      required: ['command', 'path'],
      additionalProperties: false,
    },
  },
};

export const PRESENT_PLAN_TOOL: ToolDef = {
  type: 'function',
  function: {
    name: 'present_plan',
    description:
      "Present a structured plan to the user for approval. Call this exactly once, after investigating with read-only tools, then stop and wait — do not call it more than once per turn, and do not attempt to make changes until the user approves.",
    parameters: {
      type: 'object',
      properties: {
        title: {
          type: 'string',
          description: 'A short (few words) title summarizing the plan.',
        },
        plan: {
          type: 'string',
          description: 'The full plan, written as Markdown, describing the changes you intend to make and why.',
        },
        open_questions: {
          type: 'array',
          items: { type: 'string' },
          description: 'Optional list of questions to ask the user before proceeding, if anything is ambiguous.',
        },
      },
      required: ['title', 'plan'],
      additionalProperties: false,
    },
  },
};
