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
];
