/**
 * Display-side rescue for the tool calls a model wrote as prose.
 *
 * A model that answers with a ```json fence carrying
 * `{"name": "run_shell", "arguments": {"command": "…"}}` meant to run that
 * command — `recoverTextToolCalls` in `llamaClient.ts` now turns those into
 * real tool calls before the turn ends, so new ones execute. Transcripts
 * written before that fix (and any provider that slips one past it) still
 * hold the raw JSON, and rendering it as JSON shows the wire format while
 * hiding the one thing in it the reader wants: the command, the file content,
 * the edit.
 *
 * So each kind is rendered as what it actually is — a shell fence (which also
 * picks up the code block's Run / Open in terminal buttons), the written
 * file's own content, or a diff — with the tool's `path` as the block's label
 * where it has one.
 */
import { recoverTextToolCalls, type ToolDef } from '../../lib/llamaClient';

/** What a fence turned out to really be. `lang` is the fence language to
 * render `body` under; `label` replaces the header's language label when the
 * call named a path. */
export interface TextToolCallFence {
  lang: string;
  body: string;
  label?: string;
}

/** Tool names whose `command` argument is a shell command line. */
const SHELL_TOOLS = ['run_shell', 'shell', 'bash', 'run_command'];
/** Tool names that write a whole file: `path` + `content`. */
const WRITE_TOOLS = ['write_file', 'create_file'];
/** Tool names that replace one span of a file: `path` + `old_string` +
 * `new_string`. */
const EDIT_TOOLS = ['edit_file'];

/** Every tool this module can render, as definitions, so the same strict
 * matcher that recovers a text-emitted call on the wire
 * (`recoverTextToolCalls`) does the matching here. Names are all it needs;
 * nothing on this path reads a schema. */
const TOOL_DEFS: ToolDef[] = [...SHELL_TOOLS, ...WRITE_TOOLS, ...EDIT_TOOLS].map((name) => ({
  type: 'function',
  function: { name, description: '', parameters: {} },
}));

/** Fence languages a text-emitted tool call shows up under — `json` from a
 * fenced block, and no language at all from a bare one. */
const CANDIDATE_LANGS = new Set(['', 'json', 'json5', 'jsonc']);

/** Fence language for a written file, by extension. Only the languages
 * Prism is loaded with and that a model actually writes; anything else falls
 * back to unhighlighted text, which is honest for a file whose type we can't
 * name. */
const LANG_BY_EXTENSION: Record<string, string> = {
  bash: 'bash',
  css: 'css',
  env: 'ini',
  go: 'go',
  html: 'markup',
  ini: 'ini',
  js: 'javascript',
  json: 'json',
  jsx: 'jsx',
  md: 'markdown',
  mjs: 'javascript',
  py: 'python',
  rs: 'rust',
  sh: 'bash',
  toml: 'toml',
  ts: 'typescript',
  tsx: 'tsx',
  yaml: 'yaml',
  yml: 'yaml',
  zsh: 'bash',
};

function nonEmptyString(value: unknown): value is string {
  return typeof value === 'string' && value.trim().length > 0;
}

/** The arguments of one recovered call, or null when they aren't an object. */
function argumentsOf(argumentsJson: string): Record<string, unknown> | null {
  let parsed: unknown;
  try {
    parsed = JSON.parse(argumentsJson || '{}');
  } catch {
    return null;
  }
  if (parsed === null || typeof parsed !== 'object' || Array.isArray(parsed)) return null;
  return parsed as Record<string, unknown>;
}

/** A written file's fence language, from its path. A dotfile with no
 * extension (`.zshrc`) is matched on its whole name. */
function langForPath(path: string): string {
  const name = path.split('/').pop() ?? path;
  const extension = name.includes('.') ? name.slice(name.lastIndexOf('.') + 1) : name.replace(/^\./, '');
  return LANG_BY_EXTENSION[extension.toLowerCase()] ?? '';
}

/** The last path segment, for the header label — the full path is long enough
 * to push the buttons off a narrow bubble, and the block sits under the prose
 * that names the file anyway. */
function pathLabel(path: string): string {
  return path.split('/').filter(Boolean).pop() ?? path;
}

/**
 * What a fence is really carrying, or null when it is ordinary JSON.
 *
 * Strict on purpose, for the same reason `recoverTextToolCalls` is: a fence
 * that merely documents a tool call must keep reading as JSON rather than
 * being redrawn as a command or a file. Every object in it must carry nothing
 * but tool-call keys and one of the names above, the arguments this renderer
 * reads must all be non-empty strings, and the fence must hold nothing else —
 * converting one that also carries prose would delete it.
 *
 * Several shell calls in one fence become one command line each, in order.
 * The file kinds are single-call only: two `write_file` calls cannot both be
 * one block's body without losing one.
 */
export function textToolCallFence(lang: string, body: string): TextToolCallFence | null {
  if (!CANDIDATE_LANGS.has(lang.toLowerCase())) return null;

  const recovered = recoverTextToolCalls(body, TOOL_DEFS);
  // Whatever the matcher left behind is content this function cannot render
  // as anything else. Leave the whole fence as JSON rather than drop it.
  if (recovered.toolCalls.length === 0 || recovered.content !== '') return null;

  const calls = recovered.toolCalls.map((call) => ({
    name: call.function.name,
    args: argumentsOf(call.function.arguments),
  }));
  if (calls.some((call) => call.args === null)) return null;

  if (calls.every((call) => SHELL_TOOLS.includes(call.name))) {
    const commands = calls.map((call) => call.args?.command);
    if (!commands.every(nonEmptyString)) return null;
    return { lang: 'bash', body: commands.join('\n') };
  }

  if (calls.length !== 1) return null;
  const { name, args } = calls[0];
  if (args === null) return null;
  const path = args.path;
  if (!nonEmptyString(path)) return null;

  if (WRITE_TOOLS.includes(name)) {
    const content = args.content;
    // An empty file is a real thing to write, so `content` only has to be a
    // string here — unlike a command or a path, which are useless empty.
    if (typeof content !== 'string') return null;
    return { lang: langForPath(path), body: content, label: pathLabel(path) };
  }

  if (EDIT_TOOLS.includes(name)) {
    const before = args.old_string;
    const after = args.new_string;
    if (typeof before !== 'string' || typeof after !== 'string') return null;
    // Rendered as a diff rather than as either side alone: an edit is the
    // change, and showing only the replacement hides what it replaces.
    const removed = before.split('\n').map((line) => `-${line}`);
    const added = after.split('\n').map((line) => `+${line}`);
    return { lang: 'diff', body: [...removed, ...added].join('\n'), label: pathLabel(path) };
  }

  return null;
}
