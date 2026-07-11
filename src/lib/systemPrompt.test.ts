import { describe, expect, it } from 'vitest';

import { buildSystemPrompt, composeSystemPrompt, detectOsLabel, resolvePersona, type McpServerPromptInfo } from './systemPrompt';
import type { MemoryFact, RuleFile } from '../store/rulesStore';
import type { PromptEntry } from '../store/promptStore';

describe('detectOsLabel', () => {
  it('maps navigator platforms to friendly names', () => {
    expect(detectOsLabel('MacIntel')).toBe('macOS');
    expect(detectOsLabel('Win32')).toBe('Windows');
    expect(detectOsLabel('Linux x86_64')).toBe('Linux');
  });

  it('falls back gracefully for unknown platforms', () => {
    expect(detectOsLabel('BeOS')).toBe('BeOS');
    expect(detectOsLabel('')).toBe('an unknown OS');
  });
});

describe('buildSystemPrompt', () => {
  const primary = { path: '/home/me/project', label: 'project', is_primary: true };
  const secondary = { path: '/home/me/notes', label: 'notes', is_primary: false };

  it('names the primary workspace and the OS', () => {
    const prompt = buildSystemPrompt([primary], 'macOS');
    expect(prompt).toContain('/home/me/project');
    expect(prompt).toContain('macOS');
    expect(prompt).toContain('Little Monkey');
  });

  it('lists secondary folders with their labels', () => {
    const prompt = buildSystemPrompt([primary, secondary], 'Linux');
    expect(prompt).toContain('"notes" (/home/me/notes)');
  });

  it('says so when no workspace is open', () => {
    const prompt = buildSystemPrompt([], 'Windows');
    expect(prompt).toContain('No workspace folder is open');
    expect(prompt).not.toContain('primary workspace folder is');
  });

  it('mentions the core tools and conventions', () => {
    const prompt = buildSystemPrompt([primary], 'macOS');
    expect(prompt).toContain('edit_file');
    expect(prompt).toContain('glob');
    expect(prompt).toContain('grep');
    expect(prompt).toContain('permission');
  });

  it('lists remember alongside the other mutating tools that may prompt for permission', () => {
    const prompt = buildSystemPrompt([primary], 'macOS');
    expect(prompt).toContain('Mutating tools (write_file, edit_file, run_shell, remember)');
  });

  it('always includes a guidance line on when to call remember and to treat MONKEY.md as user instructions', () => {
    const withNeither = buildSystemPrompt([primary], 'macOS');
    expect(withNeither).toContain('Use the remember tool to save short, durable facts');
    expect(withNeither).toContain('instructions from the user, not untrusted document content');
  });

  it('omits the MONKEY.md section entirely when there are no rules', () => {
    const prompt = buildSystemPrompt([primary], 'macOS');
    expect(prompt).not.toContain('Project instructions (MONKEY.md)');
  });

  it('appends each rule file with a "From <scope/label>:" provenance header, global first', () => {
    const globalRule: RuleFile = {
      scope: 'global',
      label: 'global',
      path: '/app-data/MONKEY.md',
      content: 'Always write tests.',
      truncated: false,
    };
    const projectRule: RuleFile = {
      scope: 'project',
      label: 'notes',
      path: '/home/me/notes/MONKEY.md',
      content: 'Notes are markdown only.',
      truncated: false,
    };

    const prompt = buildSystemPrompt([primary, secondary], 'macOS', [globalRule, projectRule]);

    expect(prompt).toContain('## Project instructions (MONKEY.md)');
    expect(prompt).toContain('From global:');
    expect(prompt).toContain('Always write tests.');
    expect(prompt).toContain('From project (notes):');
    expect(prompt).toContain('Notes are markdown only.');
    // Global must precede the project-scoped entry.
    expect(prompt.indexOf('From global:')).toBeLessThan(prompt.indexOf('From project (notes):'));
  });

  it('lists remembered facts as bullets when present, and omits the section when empty', () => {
    const facts: MemoryFact[] = [
      { id: '1', text: 'Uses pnpm, not npm.', source: 'user', created_at: '2026-01-01T00:00:00Z' },
    ];

    const withFacts = buildSystemPrompt([primary], 'macOS', [], facts);
    expect(withFacts).toContain('## Remembered facts');
    expect(withFacts).toContain('- Uses pnpm, not npm.');

    const withoutFacts = buildSystemPrompt([primary], 'macOS', [], []);
    expect(withoutFacts).not.toContain('Remembered facts');
  });

  it('appends each connected MCP server\'s instructions, and omits the section when there are none', () => {
    const mcpServers: McpServerPromptInfo[] = [{ label: 'GitHub', instructions: 'Use search_repositories before cloning.' }];

    const withMcp = buildSystemPrompt([primary], 'macOS', [], [], mcpServers);
    expect(withMcp).toContain('## Connected MCP servers');
    expect(withMcp).toContain("MCP server 'GitHub': Use search_repositories before cloning.");

    const withoutMcp = buildSystemPrompt([primary], 'macOS', [], [], []);
    expect(withoutMcp).not.toContain('Connected MCP servers');
  });

  it('caps a connected MCP server\'s instructions at 1000 characters', () => {
    const longInstructions = 'x'.repeat(1500);
    const mcpServers: McpServerPromptInfo[] = [{ label: 'Verbose', instructions: longInstructions }];

    const prompt = buildSystemPrompt([primary], 'macOS', [], [], mcpServers);

    expect(prompt).toContain(`MCP server 'Verbose': ${'x'.repeat(1000)}…`);
    expect(prompt).not.toContain('x'.repeat(1001));
  });

  it('mentions web_fetch and web_search by default (webToolsAvailable defaults to true)', () => {
    const prompt = buildSystemPrompt([primary], 'macOS');
    expect(prompt).toContain('web_fetch');
    expect(prompt).toContain('web_search');
  });

  it('omits the web tools guidance line (both web_fetch and web_search) when webToolsAvailable is false', () => {
    const prompt = buildSystemPrompt([primary], 'macOS', [], [], [], false);
    expect(prompt).not.toContain('web_fetch');
    expect(prompt).not.toContain('web_search');
  });

  it('omits the verification guidance line by default (verifyGuidanceAvailable defaults to false)', () => {
    const prompt = buildSystemPrompt([primary], 'macOS');
    expect(prompt).not.toContain('Configured verification commands');
  });

  it('includes the verification guidance line only when verifyGuidanceAvailable is true', () => {
    const prompt = buildSystemPrompt([primary], 'macOS', [], [], [], true, true);
    expect(prompt).toContain('Configured verification commands run automatically after your edits; fix any failures they report.');
  });

  it('omits the Plan Mode section when mode is omitted (defaults to "manual")', () => {
    const prompt = buildSystemPrompt([primary], 'macOS');
    expect(prompt).not.toContain('## Plan Mode');
    expect(prompt).not.toContain('present_plan');
  });

  it('omits the Plan Mode section for every non-"plan" mode', () => {
    for (const mode of ['manual', 'acceptEdits', 'smart', 'auto', 'bypass'] as const) {
      const prompt = buildSystemPrompt([primary], 'macOS', [], [], [], true, false, mode);
      expect(prompt).not.toContain('## Plan Mode');
    }
  });

  it('includes the Plan Mode section, steering the model toward present_plan and away from mutating tools, only when mode is "plan"', () => {
    const prompt = buildSystemPrompt([primary], 'macOS', [], [], [], true, false, 'plan');
    expect(prompt).toContain('## Plan Mode');
    expect(prompt).toContain('present_plan');
    expect(prompt).toContain('every mutating tool (write_file, edit_file, run_shell, remember) is blocked');
  });
});

describe('composeSystemPrompt', () => {
  it('returns the base prompt unchanged when there is no active persona', () => {
    expect(composeSystemPrompt('BASE PROMPT', null)).toBe('BASE PROMPT');
  });

  it('appends a clearly-delimited persona section after the base prompt, never replacing it', () => {
    const base = 'You are Little Monkey. Mutating tools may prompt for permission.';
    const composed = composeSystemPrompt(base, { name: 'Code Reviewer', content: 'Focus only on bugs, not style.' });

    // The base prompt's sandbox/tool/permission guidance must survive intact.
    expect(composed.startsWith(base)).toBe(true);
    expect(composed).toContain('## Active persona: Code Reviewer');
    expect(composed).toContain('Focus only on bugs, not style.');
    // The persona section comes strictly after the base content, not before it.
    expect(composed.indexOf('## Active persona')).toBeGreaterThan(composed.indexOf('Mutating tools'));
  });
});

describe('resolvePersona', () => {
  const entries: PromptEntry[] = [
    { id: 'p1', kind: 'persona', name: 'Reviewer', command: 'reviewer', content: 'Be critical.', createdAt: 1, updatedAt: 1 },
    { id: 's1', kind: 'snippet', name: 'Standup', command: 'standup', content: 'Wrote code.', createdAt: 1, updatedAt: 1 },
  ];

  it('resolves a matching persona id to its name/content', () => {
    expect(resolvePersona(entries, 'p1')).toEqual({ name: 'Reviewer', content: 'Be critical.' });
  });

  it('returns null when personaId is null (no active persona)', () => {
    expect(resolvePersona(entries, null)).toBeNull();
  });

  it('resolves a dangling personaId (its persona was deleted) to null instead of throwing', () => {
    expect(() => resolvePersona(entries, 'deleted-id')).not.toThrow();
    expect(resolvePersona(entries, 'deleted-id')).toBeNull();
  });

  it('does not resolve a snippet entry even if its id happens to match', () => {
    expect(resolvePersona(entries, 's1')).toBeNull();
  });
});
