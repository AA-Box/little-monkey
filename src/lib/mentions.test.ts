import { describe, expect, it } from 'vitest';

import {
  MAX_MENTION_CONTENT_CHARS,
  composeReferencedText,
  extractMentionPaths,
  formatDirListing,
  truncateMentionContent,
} from './mentions';

describe('extractMentionPaths', () => {
  it('extracts a single mention', () => {
    expect(extractMentionPaths('check @src/lib/tools.ts please')).toEqual(['src/lib/tools.ts']);
  });

  it('extracts multiple unique mentions in order', () => {
    expect(extractMentionPaths('@a.ts and @b.ts and @a.ts again')).toEqual(['a.ts', 'b.ts']);
  });

  it('strips a single trailing punctuation character', () => {
    expect(extractMentionPaths('see @README.md, @src/main.tsx. (@vite.config.ts)')).toEqual([
      'README.md',
      'src/main.tsx',
      'vite.config.ts',
    ]);
  });

  it('returns empty for text without mentions', () => {
    expect(extractMentionPaths('no mentions here')).toEqual([]);
  });

  it('ignores a bare @ with nothing after it', () => {
    expect(extractMentionPaths('a bare @ sign')).toEqual([]);
  });
});

describe('formatDirListing', () => {
  it('sorts directories first, then alphabetically', () => {
    const listing = formatDirListing([
      { name: 'zeta.txt', is_dir: false, size: 1 },
      { name: 'beta', is_dir: true, size: 0 },
      { name: 'alpha.txt', is_dir: false, size: 2 },
      { name: 'gamma', is_dir: true, size: 0 },
    ]);
    expect(listing).toBe('- beta/\n- gamma/\n- alpha.txt\n- zeta.txt');
  });
});

describe('truncateMentionContent', () => {
  it('returns short content unchanged', () => {
    expect(truncateMentionContent('short')).toBe('short');
  });

  it('truncates content over the cap and appends a marker', () => {
    const long = 'x'.repeat(MAX_MENTION_CONTENT_CHARS + 10_000);
    const result = truncateMentionContent(long);
    // Exactly the cap's worth of content is kept, then the marker.
    expect(result.slice(0, MAX_MENTION_CONTENT_CHARS)).toBe('x'.repeat(MAX_MENTION_CONTENT_CHARS));
    expect(result.slice(MAX_MENTION_CONTENT_CHARS)).toContain('[Truncated');
    expect(result.length).toBeLessThan(long.length);
  });
});

describe('composeReferencedText', () => {
  it('returns the text verbatim with no references', () => {
    expect(composeReferencedText('hello', [])).toBe('hello');
  });

  it('prepends fenced file sections and a separator', () => {
    const result = composeReferencedText('explain this', [
      { path: 'a.ts', isDir: false, content: 'const a = 1;' },
    ]);
    expect(result).toContain('Referenced files:');
    expect(result).toContain('### a.ts');
    expect(result).toContain('```\nconst a = 1;\n```');
    expect(result).toContain('BEGIN UNTRUSTED DATA');
    expect(result.endsWith('explain this')).toBe(true);
  });

  it('renders directory references without code fences', () => {
    const result = composeReferencedText('what is in here', [
      { path: 'src', isDir: true, content: '- lib/\n- main.tsx' },
    ]);
    expect(result).toContain('### src\n[Untrusted data from workspace directory src]');
    expect(result).toContain('- lib/\n- main.tsx');
    expect(result).not.toContain('```');
  });

  it('labels terminal evidence as untrusted context rather than a workspace file', () => {
    const result = composeReferencedText('review this run', [
      { path: 'terminal://term-1/1.txt', isDir: false, content: 'tests passed', source: 'terminal' },
    ]);
    expect(result).toContain('Referenced context:');
    expect(result).toContain('Untrusted data from terminal evidence terminal://term-1/1.txt');
    expect(result).toContain('tests passed');
  });
});
