import { describe, expect, it, vi } from 'vitest';
import {
  artifactVersions,
  containsScriptTag,
  detectFenceKind,
  extractArtifacts,
  findArtifact,
  fingerprintArtifact,
  renderMermaidToSvg,
  wrapArtifactDocument,
} from './artifacts';
import type { ChatMessage } from './llamaClient';

function assistant(content: string): ChatMessage {
  return { role: 'assistant', content };
}

// `renderMermaidToSvg` lazily `import()`s the real `mermaid` package — mocked
// here so these tests exercise `artifacts.ts`'s own contract (initialize
// exactly once with the design doc's required options, a fresh id per call,
// and a rejected promise — never a synchronous throw — for malformed
// diagrams) without depending on mermaid's actual DOM-based rendering, which
// vitest's `node` environment (see vitest.config.ts) can't run anyway.
const mermaidInitialize = vi.fn();
const mermaidRender = vi.fn();
vi.mock('mermaid', () => ({
  default: {
    initialize: (...args: unknown[]) => mermaidInitialize(...args),
    render: (...args: unknown[]) => mermaidRender(...args),
  },
}));

describe('detectFenceKind', () => {
  it('recognizes html/htm', () => {
    expect(detectFenceKind('html', '<div></div>')).toBe('html');
    expect(detectFenceKind('HTML', '<div></div>')).toBe('html');
    expect(detectFenceKind('htm', '<div></div>')).toBe('html');
  });

  it('recognizes svg', () => {
    expect(detectFenceKind('svg', '<svg></svg>')).toBe('svg');
  });

  it('recognizes mermaid', () => {
    expect(detectFenceKind('mermaid', 'graph TD; A-->B;')).toBe('mermaid');
  });

  it('reclassifies xml as svg only when the body starts with <svg', () => {
    expect(detectFenceKind('xml', '<svg viewBox="0 0 1 1"></svg>')).toBe('svg');
    expect(detectFenceKind('xml', '  \n<svg></svg>')).toBe('svg');
    expect(detectFenceKind('xml', '<?xml version="1.0"?><root/>')).toBeNull();
  });

  it('returns null for everything else', () => {
    expect(detectFenceKind('ts', 'const x = 1;')).toBeNull();
    expect(detectFenceKind('', 'plain text')).toBeNull();
    expect(detectFenceKind('bash', 'echo hi')).toBeNull();
  });
});

describe('containsScriptTag', () => {
  it('detects an inline <script> tag case-insensitively', () => {
    expect(containsScriptTag('<html><script>alert(1)</script></html>')).toBe(true);
    expect(containsScriptTag('<HTML><SCRIPT>alert(1)</SCRIPT></HTML>')).toBe(true);
  });

  it('is false for content with no script tag', () => {
    expect(containsScriptTag('<html><body>hi</body></html>')).toBe(false);
    expect(containsScriptTag('a description of a <script> tag')).toBe(true); // still a literal tag-looking substring
  });
});

describe('extractArtifacts', () => {
  it('extracts a single closed html fence', () => {
    const messages = [assistant('Here is a page:\n\n```html\n<html><body>hi</body></html>\n```\n')];
    const blocks = extractArtifacts(messages);
    expect(blocks).toHaveLength(1);
    expect(blocks[0]).toMatchObject({
      ref: { messageIndex: 0, blockIndex: 0 },
      kind: 'html',
      content: '<html><body>hi</body></html>',
    });
  });

  it('ignores fences in user messages', () => {
    const messages: ChatMessage[] = [
      { role: 'user', content: '```html\n<div></div>\n```' },
    ];
    expect(extractArtifacts(messages)).toHaveLength(0);
  });

  it('ignores non-previewable languages', () => {
    const messages = [assistant('```ts\nconst x = 1;\n```')];
    expect(extractArtifacts(messages)).toHaveLength(0);
  });

  it('never returns an artifact for an unterminated (still-streaming) fence', () => {
    // Simulates a partial stream: the model has started an html fence but
    // the closing ``` hasn't arrived yet.
    const partial = 'Building a page:\n\n```html\n<html><body>work in progress';
    const blocks = extractArtifacts([assistant(partial)]);
    expect(blocks).toHaveLength(0);
  });

  it('produces the artifact once the same content later closes the fence', () => {
    // The next streamed chunk appended the closing fence — this is the
    // "button appears once the fence closes and re-parses" behavior the
    // design doc calls out.
    const completed = 'Building a page:\n\n```html\n<html><body>work in progress</body></html>\n```\n';
    const blocks = extractArtifacts([assistant(completed)]);
    expect(blocks).toHaveLength(1);
    expect(blocks[0].kind).toBe('html');
  });

  it('stops scanning at the first unterminated fence but keeps earlier closed ones', () => {
    const content = [
      '```html',
      '<div>one</div>',
      '```',
      '',
      '```html',
      '<div>still streaming...',
    ].join('\n');
    const blocks = extractArtifacts([assistant(content)]);
    expect(blocks).toHaveLength(1);
    expect(blocks[0].content).toBe('<div>one</div>');
  });

  it('titles from the nearest preceding heading', () => {
    const content = ['# Landing Page', '', '```html', '<div>hi</div>', '```'].join('\n');
    const blocks = extractArtifacts([assistant(content)]);
    expect(blocks[0].title).toBe('Landing Page');
  });

  it('falls back to a <title> element when there is no preceding heading', () => {
    const content = ['```html', '<html><head><title>My Doc</title></head></html>', '```'].join('\n');
    const blocks = extractArtifacts([assistant(content)]);
    expect(blocks[0].title).toBe('My Doc');
  });

  it('prefers a preceding heading over an in-body <title>', () => {
    const content = [
      '## Actual Heading',
      '```html',
      '<html><head><title>Ignored</title></head></html>',
      '```',
    ].join('\n');
    const blocks = extractArtifacts([assistant(content)]);
    expect(blocks[0].title).toBe('Actual Heading');
  });

  it('falls back to a localized numbered placeholder with no heading or <title>', () => {
    const content = ['```svg', '<svg></svg>', '```'].join('\n');
    const blocks = extractArtifacts([assistant(content)], (n) => `Artefact n°${n}`);
    expect(blocks[0].title).toBe('Artefact n°1');
  });

  it('numbers untitled fallbacks as a running count across the whole transcript', () => {
    const messages = [
      assistant(['```svg', '<svg></svg>', '```'].join('\n')),
      assistant(['```svg', '<svg></svg>', '```'].join('\n')),
    ];
    const blocks = extractArtifacts(messages);
    expect(blocks.map((b) => b.title)).toEqual(['Artifact 1', 'Artifact 2']);
  });

  it('assigns blockIndex only over previewable fences within a message', () => {
    const content = [
      '```ts',
      'const x = 1;',
      '```',
      '',
      '```html',
      '<div>first</div>',
      '```',
      '',
      '```html',
      '<div>second</div>',
      '```',
    ].join('\n');
    const blocks = extractArtifacts([assistant(content)]);
    expect(blocks).toHaveLength(2);
    expect(blocks[0].ref).toMatchObject({ messageIndex: 0, blockIndex: 0 });
    expect(blocks[1].ref).toMatchObject({ messageIndex: 0, blockIndex: 1 });
  });

  it('recognizes a previewable fence even when its info string has a second token, matching how react-markdown derives className (MessageBubble.tsx only sees the first word)', () => {
    const content = ['```html data-foo', '<p>content</p>', '```'].join('\n');
    const blocks = extractArtifacts([assistant(content)]);
    expect(blocks).toHaveLength(1);
    expect(blocks[0].kind).toBe('html');
  });

  it('reads messageIndex from the real position in the transcript, not the assistant-only count', () => {
    const messages: ChatMessage[] = [
      { role: 'user', content: 'make me a page' },
      assistant(['```html', '<div>hi</div>', '```'].join('\n')),
    ];
    const blocks = extractArtifacts(messages);
    expect(blocks[0].ref.messageIndex).toBe(1);
  });
});

describe('findArtifact / artifactVersions', () => {
  const messages = [
    assistant(['# Widget', '```html', '<div>v1</div>', '```'].join('\n')),
    assistant(['# Widget', '```html', '<div>v2</div>', '```'].join('\n')),
  ];
  const blocks = extractArtifacts(messages);

  it('findArtifact resolves an existing ref', () => {
    const found = findArtifact(blocks, blocks[1].ref);
    expect(found?.content).toBe('<div>v2</div>');
  });

  it('findArtifact returns null for a ref that no longer matches anything', () => {
    expect(findArtifact(blocks, { messageIndex: 99, blockIndex: 0, fingerprint: 'deadbeef' })).toBeNull();
  });

  it('artifactVersions groups same-titled artifacts across messages', () => {
    const versions = artifactVersions(blocks, blocks[0]);
    expect(versions).toHaveLength(2);
    expect(versions.map((v) => v.content)).toEqual(['<div>v1</div>', '<div>v2</div>']);
  });

  it('findArtifact returns null when the slot still exists but its content changed underneath it (edit-and-resubmit reusing the same index)', () => {
    // Reproduces the review finding directly: an edit-and-resubmit truncates
    // and regenerates, and the new reply can coincidentally land its own
    // fence at the exact same {messageIndex, blockIndex} an already-open
    // ArtifactRef points at. Positional matching alone would wrongly resolve
    // this to the new, unrelated content — the fingerprint must catch it.
    const before = extractArtifacts([assistant(['```html', '<div>Version A chart</div>', '```'].join('\n'))]);
    const staleRef = before[0].ref;

    const after = extractArtifacts([assistant(['```html', '<div>Version B game</div>', '```'].join('\n'))]);

    expect(findArtifact(after, staleRef)).toBeNull();
  });

  it('findArtifact still resolves when the slot is re-extracted with identical content', () => {
    const first = extractArtifacts([assistant(['```html', '<div>same</div>', '```'].join('\n'))]);
    const second = extractArtifacts([assistant(['```html', '<div>same</div>', '```'].join('\n'))]);
    expect(findArtifact(second, first[0].ref)?.content).toBe('<div>same</div>');
  });
});

describe('fingerprintArtifact', () => {
  it('is deterministic for the same kind + content', () => {
    expect(fingerprintArtifact('html', '<div>x</div>')).toBe(fingerprintArtifact('html', '<div>x</div>'));
  });

  it('differs when content differs', () => {
    expect(fingerprintArtifact('html', '<div>x</div>')).not.toBe(fingerprintArtifact('html', '<div>y</div>'));
  });

  it('differs when kind differs for the same content string', () => {
    expect(fingerprintArtifact('html', 'same')).not.toBe(fingerprintArtifact('svg', 'same'));
  });
});

describe('wrapArtifactDocument', () => {
  it('passes html content through unchanged', () => {
    const html = '<html><body>hi</body></html>';
    expect(wrapArtifactDocument('html', html)).toBe(html);
  });

  it('wraps svg content in a minimal centered shell', () => {
    const svg = '<svg><circle r="1"/></svg>';
    const wrapped = wrapArtifactDocument('svg', svg);
    expect(wrapped).toContain(svg);
    expect(wrapped).toContain('<!doctype html>');
  });

  it('never strips or escapes an inline <script> tag inside the content being wrapped', () => {
    // wrapArtifactDocument does no sanitization of its own — see its doc
    // comment. The ONLY thing that makes a <script> tag inert is the
    // consuming iframe's empty sandbox="" attribute plus the app's CSP (see
    // ArtifactPane.tsx and the manual browser verification recorded there).
    // This test pins that "pass-through, not sanitize" contract so a future
    // change doesn't silently start relying on string-sanitizing the
    // content instead of the sandbox attribute.
    const withScript = '<html><body><script>window.exfiltrated = true;</script></body></html>';
    expect(wrapArtifactDocument('html', withScript)).toBe(withScript);
    expect(containsScriptTag(wrapArtifactDocument('html', withScript))).toBe(true);
  });
});

describe('renderMermaidToSvg', () => {
  it('initializes mermaid once with startOnLoad:false, securityLevel:"strict", then renders', async () => {
    mermaidRender.mockResolvedValueOnce({ svg: '<svg>diagram</svg>' });
    const svg = await renderMermaidToSvg('graph TD; A-->B;');
    expect(svg).toBe('<svg>diagram</svg>');
    expect(mermaidInitialize).toHaveBeenCalledWith({ startOnLoad: false, securityLevel: 'strict' });
    expect(mermaidRender).toHaveBeenCalledWith(expect.stringMatching(/^mermaid-artifact-/), 'graph TD; A-->B;');
  });

  it('only calls mermaid.initialize once across multiple renders', async () => {
    // Depends on the previous test having already run in this file (vitest
    // runs `it`s within a file in declaration order) and set the module's
    // `mermaidInitialized` flag — asserts the design doc's "set at most
    // once" requirement, not merely that it was called at all.
    mermaidInitialize.mockClear();
    mermaidRender.mockResolvedValueOnce({ svg: '<svg>a</svg>' }).mockResolvedValueOnce({ svg: '<svg>b</svg>' });
    await renderMermaidToSvg('graph TD; A-->B;');
    await renderMermaidToSvg('graph TD; C-->D;');
    expect(mermaidInitialize).not.toHaveBeenCalled();
  });

  it('uses a distinct render id for every call', async () => {
    mermaidRender.mockResolvedValueOnce({ svg: '<svg>a</svg>' }).mockResolvedValueOnce({ svg: '<svg>b</svg>' });
    await renderMermaidToSvg('graph TD; A-->B;');
    await renderMermaidToSvg('graph TD; A-->B;');
    const calls = mermaidRender.mock.calls;
    const [firstId] = calls[calls.length - 2];
    const [secondId] = calls[calls.length - 1];
    expect(firstId).not.toBe(secondId);
  });

  it('rejects (never throws synchronously) for malformed diagram syntax', async () => {
    // Mirrors what mermaid itself does for unparseable input — the design
    // doc requires this to surface as a visible render-error state in
    // ArtifactPane, never an uncaught crash.
    mermaidRender.mockRejectedValueOnce(new Error('Parse error on line 1'));
    await expect(renderMermaidToSvg('this is not a valid diagram (')).rejects.toThrow('Parse error on line 1');
  });
});
