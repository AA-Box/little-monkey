import { describe, expect, it } from 'vitest';

import { buildSystemPrompt, detectOsLabel } from './systemPrompt';

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
});
