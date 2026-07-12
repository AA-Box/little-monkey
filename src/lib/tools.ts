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
 * three-way-registry-drift exception (TS tools.ts / Rust tools.rs / lm-cli
 * tools_def.rs normally mirror each other 1:1 — see this module's top
 * doc comment) called out explicitly in the Plan/Act design doc
 * (docs/roadmap/p2-plan-act-safety.md) as a known, accepted risk: a reader
 * scanning tools.rs for `present_plan` and finding nothing should look here,
 * not assume a missing Rust command.
 */
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
