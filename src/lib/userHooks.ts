/**
 * User-configured lifecycle hooks (Claude-Code-style) — the frontend half of
 * `src-tauri/src/hooks.rs`. The user wires shell commands to agent lifecycle
 * events in Settings; this module decides WHICH hooks fire for an event and
 * interprets what they said. Execution itself is one Rust command
 * (`hook_exec`: 10s timeout, bounded capture, no permission prompt — hooks
 * are user-authored configuration, not model output).
 *
 * Failure posture (the security-relevant part, wired at `turnEngine.ts`'s
 * `executeToolCall`): a hook that RAN and answered "deny" — non-zero exit,
 * or exit 0 with `{"decision":"deny"}` on stdout — blocks the tool call and
 * its reason becomes the tool error. A hook that never answered (spawn
 * failure, timeout) is a console WARN and the call proceeds: hooks are an
 * overlay the user added, and a broken overlay must not brick every tool
 * call of every turn.
 */
import { invoke } from '@tauri-apps/api/core';
import { useUserHooksStore, type UserHookDef, type UserHookEvent } from '../store/userHooksStore';

/** What `hook_exec` (hooks.rs) reports for one hook run. */
export interface HookExecOutcome {
  exit_code: number | null;
  stdout: string;
  stderr: string;
  timed_out: boolean;
}

/** A PreToolUse hook's verdict when it blocked the call. */
export interface HookDenial {
  reason: string;
}

/** The stdin payload every hook receives — one JSON object, same base shape
 * for every event; `PostToolUse` additionally carries the tool `result`. */
interface HookPayload {
  event: UserHookEvent;
  tool_name?: string;
  args?: Record<string, unknown>;
  session_id?: string;
  result?: string;
}

/** Whether a hook's tool-name matcher covers `toolName`. No matcher = every
 * tool. Interpreted as a regular expression (`write_file|edit_file` works);
 * an invalid pattern falls back to exact-name equality rather than silently
 * matching nothing or everything. */
export function matcherMatches(matcher: string | undefined, toolName: string): boolean {
  const trimmed = matcher?.trim() ?? '';
  if (trimmed.length === 0) return true;
  try {
    return new RegExp(`^(?:${trimmed})$`).test(toolName);
  } catch {
    return trimmed === toolName;
  }
}

/** The configured hooks for one event, matcher-filtered when a tool name
 * applies. Reads the store live so a settings edit takes effect on the very
 * next tool call, no restart. */
export function hooksForEvent(event: UserHookEvent, toolName?: string): UserHookDef[] {
  return useUserHooksStore
    .getState()
    .hooks.filter((hook) => hook.event === event && (toolName === undefined || matcherMatches(hook.matcher, toolName)));
}

async function execHook(hook: UserHookDef, payload: HookPayload): Promise<HookExecOutcome> {
  return await invoke<HookExecOutcome>('hook_exec', { command: hook.command, payload: JSON.stringify(payload) });
}

/** Parses an exit-0 hook's stdout for an explicit `{"decision":"deny"}`
 * verdict. Anything else (empty stdout, non-JSON, other decisions) means
 * "allow". */
function stdoutDenial(stdout: string): string | null {
  try {
    const parsed: unknown = JSON.parse(stdout);
    if (parsed && typeof parsed === 'object' && (parsed as { decision?: unknown }).decision === 'deny') {
      const reason = (parsed as { reason?: unknown }).reason;
      return typeof reason === 'string' && reason.trim().length > 0 ? reason : 'Blocked by a PreToolUse hook.';
    }
  } catch {
    // Not JSON — an allow.
  }
  return null;
}

/**
 * Runs every matching PreToolUse hook, in configured order, and returns the
 * first denial — or `null` when every hook allowed (or failed to answer; see
 * the module doc comment for why an unanswering hook proceeds).
 */
export async function evaluatePreToolUseHooks(
  toolName: string,
  args: Record<string, unknown>,
  sessionId: string | undefined,
): Promise<HookDenial | null> {
  const hooks = hooksForEvent('PreToolUse', toolName);
  for (const hook of hooks) {
    let outcome: HookExecOutcome;
    try {
      outcome = await execHook(hook, { event: 'PreToolUse', tool_name: toolName, args, session_id: sessionId });
    } catch (err) {
      console.warn(`PreToolUse hook "${hook.command}" could not run — proceeding:`, err);
      continue;
    }
    if (outcome.timed_out || outcome.exit_code === null) {
      console.warn(`PreToolUse hook "${hook.command}" timed out or was killed — proceeding.`);
      continue;
    }
    if (outcome.exit_code !== 0) {
      const reason = outcome.stderr.trim() || outcome.stdout.trim() || `PreToolUse hook exited with code ${outcome.exit_code}.`;
      return { reason };
    }
    const denial = stdoutDenial(outcome.stdout);
    if (denial !== null) return { reason: denial };
  }
  return null;
}

/** Fires every matching hook for an observe-only event (PostToolUse,
 * SessionStart) and never blocks on, or surfaces, anything they do — errors
 * are console noise by design. */
export function fireObservedHooks(event: 'PostToolUse' | 'SessionStart', payload: Omit<HookPayload, 'event'> = {}): void {
  for (const hook of hooksForEvent(event, payload.tool_name)) {
    void execHook(hook, { event, ...payload }).catch((err) => {
      console.warn(`${event} hook "${hook.command}" failed:`, err);
    });
  }
}

/**
 * Runs every UserPromptSubmit hook and returns their non-empty stdouts
 * joined — appended by `agentLoop.ts` to the turn's system context. A hook
 * that fails or times out contributes nothing (WARN + skip), same posture
 * as everywhere else in this module.
 */
export async function collectUserPromptSubmitContext(sessionId: string | undefined): Promise<string> {
  const hooks = hooksForEvent('UserPromptSubmit');
  const sections: string[] = [];
  for (const hook of hooks) {
    try {
      const outcome = await execHook(hook, { event: 'UserPromptSubmit', session_id: sessionId });
      if (outcome.timed_out || outcome.exit_code !== 0) {
        console.warn(`UserPromptSubmit hook "${hook.command}" did not complete cleanly — skipping its output.`);
        continue;
      }
      const text = outcome.stdout.trim();
      if (text.length > 0) sections.push(text);
    } catch (err) {
      console.warn(`UserPromptSubmit hook "${hook.command}" could not run:`, err);
    }
  }
  return sections.join('\n\n');
}
