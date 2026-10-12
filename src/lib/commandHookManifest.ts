/**
 * Safe, explicit import of command-hook manifests into
 * Little Monkey's existing FOUR lifecycle hook events.
 *
 * No action occurs during parsing. Importing command hooks is an explicit
 * operator action, not model-triggered capability expansion. Other hook
 * kinds/events are refused rather than mapped to a misleading equivalent.
 */
import type { UserHookEvent } from '../store/userHooksStore';

export interface CompatibleCommandHook {
  event: UserHookEvent;
  command: string;
  matcher?: string;
}
export interface CommandHookImport {
  supported: CompatibleCommandHook[];
  unsupported: string[];
}
const EVENTS: readonly UserHookEvent[] = ['PreToolUse', 'PostToolUse', 'SessionStart', 'UserPromptSubmit'];
const MAX_MANIFEST = 128 * 1024;
const MAX_HOOKS = 32;
const MAX_COMMAND_CHARS = 2_000;
const MAX_MATCHER_CHARS = 250;

export function parseCommandHooksManifest(raw: string): CommandHookImport {
  if (raw.length > MAX_MANIFEST) throw new Error('Command-hook manifest exceeds 128 KiB.');
  let document: unknown;
  try { document = JSON.parse(raw); }
  catch { throw new Error('Hooks manifest must be valid JSON.'); }
  if (!document || typeof document !== 'object' || Array.isArray(document))
    throw new Error('Command-hook manifest must be a JSON object.');
  const hooks = (document as { hooks?: unknown }).hooks;
  if (!hooks || typeof hooks !== 'object' || Array.isArray(hooks))
    throw new Error('Expected a compatible hooks object.');
  const supported: CompatibleCommandHook[] = [];
  const unsupported: string[] = [];
  for (const [event, groups] of Object.entries(hooks)) {
    if (!EVENTS.includes(event as UserHookEvent)) {
      unsupported.push('Unsupported lifecycle event: ' + event.slice(0, 100));
      continue;
    }
    if (!Array.isArray(groups) || groups.length > MAX_HOOKS) {
      unsupported.push('Invalid hook groups for: ' + event);
      continue;
    }
    for (const group of groups) {
      if (!group || typeof group !== 'object' || Array.isArray(group)) {
        unsupported.push('Invalid hook group: ' + event);
        continue;
      }
      // Imported manifests must not silently discard policy or execution settings.
      // A command with ignored background-execution metadata could behave
      // very differently from the operator's preview.
      if (Object.keys(group).some(key => !['matcher', 'hooks'].includes(key))) {
        unsupported.push('Unsupported hook-group settings: ' + event); continue;
      }
      const rawGroup = group as { matcher?: unknown; hooks?: unknown };
      const matcher = rawGroup.matcher;
      if (matcher !== undefined && (typeof matcher !== 'string' || matcher.length > MAX_MATCHER_CHARS ||
          /[\x00-\x1f]/.test(matcher))) {
        unsupported.push('Invalid matcher: ' + event); continue;
      }
      if (!Array.isArray(rawGroup.hooks) || rawGroup.hooks.length > MAX_HOOKS) {
        unsupported.push('Invalid nested command hooks: ' + event); continue;
      }
      for (const rawHook of rawGroup.hooks) {
        if (!rawHook || typeof rawHook !== 'object' || Array.isArray(rawHook)) {
          unsupported.push('Unsupported hook entry: ' + event); continue;
        }
        const hook = rawHook as { type?: unknown; command?: unknown; async?: unknown; timeout?: unknown };
        const unknownOptions = Object.keys(rawHook).some(key =>
          !['type', 'command', 'async', 'timeout'].includes(key));
        if (unknownOptions || hook.type !== 'command' || typeof hook.command !== 'string' ||
            !hook.command.trim() || hook.command.length > MAX_COMMAND_CHARS ||
            /\x00/.test(hook.command) || (hook.async !== undefined && hook.async !== false) ||
            (hook.timeout !== undefined && (
              typeof hook.timeout !== 'number' ||
              !Number.isSafeInteger(hook.timeout) ||
              hook.timeout <= 0 || hook.timeout > 10_000
            ))) {
          unsupported.push('Unsupported or unbounded command hook: ' + event); continue;
        }
        if (supported.length >= MAX_HOOKS) throw new Error('Hook import exceeds 32 supported commands.');
        supported.push({
          event: event as UserHookEvent,
          command: hook.command,
          ...(typeof matcher === 'string' && matcher.trim() ? { matcher: matcher.trim() } : {}),
        });
      }
    }
  }
  return { supported, unsupported };
}
