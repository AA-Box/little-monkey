import { describe, expect, it } from 'vitest';

import { buildSystemPrompt, detectOsLabel } from './systemPrompt';
import type { MemoryFact, RuleFile } from '../store/rulesStore';

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
});
