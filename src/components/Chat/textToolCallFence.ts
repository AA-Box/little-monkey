/**
 * Display-side rescue for the tool calls a model wrote as prose.
 *
 * A model that answers with a ```json fence carrying
 * `{"name": "run_shell", "arguments": {"command": "…"}}` meant to run that
 * command — `recoverTextToolCalls` in `llamaClient.ts` now turns those into
 * real tool calls before the turn ends, so new ones execute. Transcripts
 * written before that fix (and any provider that slips one past it) still
 * hold the raw JSON, and rendering it as JSON hides the one useful thing in
 * it: the command. Rendered as a shell fence instead, it reads as the command
 * it always was and picks up the code block's Run / Open in terminal buttons.
 *
 * Only shell tools qualify. An `edit_file` call written as prose is not a
 * command and has nothing to run, so it stays the JSON it is.
 */
import { recoverTextToolCalls, type ToolDef } from '../../lib/llamaClient';

/** Tool names whose `command` argument is a shell command line. */
const SHELL_TOOL_NAMES = ['run_shell', 'shell', 'bash', 'run_command'];

/** The shell tools as definitions, so the same strict matcher that recovers a
 * text-emitted call on the wire (`recoverTextToolCalls`) does the matching
 * here. Names are all it needs; nothing on this path reads a schema. */
const SHELL_TOOL_DEFS: ToolDef[] = SHELL_TOOL_NAMES.map((name) => ({
  type: 'function',
  function: { name, description: '', parameters: {} },
}));

/** Fence languages a text-emitted tool call shows up under — `json` from a
 * fenced block, and no language at all from a bare one. */
const CANDIDATE_LANGS = new Set(['', 'json', 'json5', 'jsonc']);

/**
 * The shell command a fence is really asking for, or null when the fence is
 * anything other than shell tool calls.
 *
 * Strict on purpose, for the same reason `recoverTextToolCalls` is: a fence
 * that merely documents a tool call must keep reading as JSON rather than
 * gaining a Run button. Every object in it must carry nothing but tool-call
 * keys, a shell tool's name, and a non-empty string `command` — and the fence
 * must hold nothing else, since converting one that also carries prose or a
 * non-shell call would delete what it can't render as a command.
 *
 * A fence holding several calls (a model restating a whole plan in one block)
 * becomes one command per line, in order.
 */
export function textToolCallShellCommand(lang: string, body: string): string | null {
  if (!CANDIDATE_LANGS.has(lang.toLowerCase())) return null;

  const recovered = recoverTextToolCalls(body, SHELL_TOOL_DEFS);
  // Whatever the matcher left behind is content this function cannot express
  // as a command line. Leave the whole fence as JSON rather than drop it.
  if (recovered.toolCalls.length === 0 || recovered.content !== '') return null;

  const commands: string[] = [];
  for (const call of recovered.toolCalls) {
    let args: unknown;
    try {
      args = JSON.parse(call.function.arguments || '{}');
    } catch {
      return null;
    }
    const command = (args as { command?: unknown }).command;
    if (typeof command !== 'string' || !command.trim()) return null;
    commands.push(command);
  }
  return commands.join('\n');
}
