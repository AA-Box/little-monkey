import { describe, expect, it } from 'vitest';
import { parseCommandHooksManifest } from './commandHookManifest';

describe('Native command-hook manifest compatibility', () => {
  it('parses a bounded supported command-hook manifest without execution', () => {
    const parsed = parseCommandHooksManifest(JSON.stringify({ hooks: {
      PreToolUse: [{ matcher: 'read_file|write_file', hooks: [{ type: 'command', command: 'node ./lint.mjs', timeout: 5000 }] }],
      SessionStart: [{ hooks: [{ type: 'command', command: 'node ./session.mjs' }] }],
    }}));
    expect(parsed).toEqual({ supported: [
      { event: 'PreToolUse', command: 'node ./lint.mjs', matcher: 'read_file|write_file' },
      { event: 'SessionStart', command: 'node ./session.mjs' },
    ], unsupported: [] });
  });
  it('refuses unsupported events and arbitrary prompt/subagent hook kinds', () => {
    const parsed = parseCommandHooksManifest(JSON.stringify({ hooks: {
      PermissionRequest: [{ hooks: [{ type: 'command', command: 'echo no' }] }],
      PreToolUse: [{ hooks: [{ type: 'prompt', prompt: 'allow all' }, { type: 'agent', command: 'bypass' }] }],
    }}));
    expect(parsed.supported).toEqual([]);
    expect(parsed.unsupported).toHaveLength(3);
  });
  it('rejects oversized manifests and unbounded async or timeout hooks', () => {
    expect(() => parseCommandHooksManifest(' '.repeat(129 * 1024))).toThrow(/128 KiB/);
    const parsed = parseCommandHooksManifest(JSON.stringify({ hooks: {
      PostToolUse: [{ hooks: [
        { type: 'command', command: 'echo hi', timeout: 99_000 },
        { type: 'command', command: 'echo hi', async: true },
      ] }],
    }}));
    expect(parsed.supported).toEqual([]);
  });
  it('fails closed for unsupported hook policy fields and malformed timeouts', () => {
    const parsed = parseCommandHooksManifest(JSON.stringify({ hooks: {
      PreToolUse: [
        { matcher: 'read_file', async: true, hooks: [{ type: 'command', command: 'node hidden.mjs' }] },
        { hooks: [
          { type: 'command', command: 'node hidden.mjs', timeout: -1 },
          { type: 'command', command: 'node hidden.mjs', timeout: '5000' },
          { type: 'command', command: 'node hidden.mjs', async: 'false' },
          { type: 'command', command: 'node hidden.mjs', onError: 'continue' },
          { type: 'command', command: 'node allowed.mjs', async: false, timeout: 5000 },
        ] },
      ],
    }}));
    expect(parsed.supported).toEqual([{ event: 'PreToolUse', command: 'node allowed.mjs' }]);
    expect(parsed.unsupported).toHaveLength(5);
  });

  it('rejects non-JSON manifests without executing them', () => {
    expect(() => parseCommandHooksManifest('{')).toThrow(/valid JSON/);
    expect(() => parseCommandHooksManifest('[]')).toThrow(/JSON object/);
  });
});
