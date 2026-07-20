import { describe, expect, it } from 'vitest';

import {
  createLocalDesignSource,
  createReferenceDesignSource,
  designSourceRevision,
  exportDesignProjectJson,
  hydrateDesignSource,
  parseDesignImplementationPlan,
  validateDesignSources,
} from './designToApp';

const PNG = 'data:image/png;base64,AAAA';

function screenshot(id = 'source-shot') {
  return createLocalDesignSource({
    id,
    kind: 'screenshot',
    name: 'home.png',
    mediaType: 'image/png',
    imageDataUrl: PNG,
  });
}

function tokens(id = 'source-tokens') {
  return createLocalDesignSource({
    id,
    kind: 'design_tokens',
    name: 'tokens.json',
    mediaType: 'application/json',
    textContent: JSON.stringify({ color: { brand: '#3366ff' } }),
  });
}

describe('Design-to-App source intake', () => {
  it('accepts bounded local images and valid token JSON', () => {
    const image = screenshot();
    const payload = tokens();

    expect(image.sizeBytes).toBe(3);
    expect(image.availability).toBe('ready');
    expect(payload.textContent).toContain('#3366ff');
    expect(validateDesignSources([image, payload])).toEqual([]);
  });

  it('rejects malformed token payloads and unsupported image URLs', () => {
    expect(() => createLocalDesignSource({
      kind: 'design_tokens',
      name: 'bad.json',
      mediaType: 'application/json',
      textContent: '{bad',
    })).toThrow('must be valid JSON');

    expect(() => createLocalDesignSource({
      kind: 'screenshot',
      name: 'bad.svg',
      mediaType: 'image/svg+xml',
      imageDataUrl: 'data:image/svg+xml;base64,AAAA',
    })).toThrow('PNG, JPEG, GIF, or WebP');
  });

  it('fails closed for a Figma URL until an export is attached', () => {
    const figmaUrl = createReferenceDesignSource({ url: 'https://www.figma.com/design/abc/file' });
    expect(figmaUrl.availability).toBe('requires_export');
    expect(validateDesignSources([figmaUrl])).toContain(
      'A Figma URL cannot be fetched here. Attach a Figma frame image or JSON/token export.',
    );

    const exported = createLocalDesignSource({
      kind: 'figma_export',
      name: 'frame.png',
      mediaType: 'image/png',
      imageDataUrl: PNG,
    });
    expect(validateDesignSources([figmaUrl, exported])).toEqual([]);
  });

  it('rehydrates stripped images as explicit re-import requirements', () => {
    const persisted = { ...screenshot(), imageDataUrl: null };
    const hydrated = hydrateDesignSource(persisted);
    expect(hydrated).toMatchObject({ availability: 'needs_reimport', imageDataUrl: null });
    expect(validateDesignSources([hydrated!])[0]).toMatch(/Re-import 1 image source/);
  });

  it('normalizes reference URLs and blocks credentials or non-http schemes', () => {
    expect(createReferenceDesignSource({ url: 'https://example.com/a#secret' }).sourceUri).toBe('https://example.com/a');
    expect(() => createReferenceDesignSource({ url: 'https://user:pass@example.com' })).toThrow('credentials');
    expect(() => createReferenceDesignSource({ url: 'file:///tmp/mockup.png' })).toThrow('http: or https:');
  });
});

describe('Design-to-App source-mapped plans', () => {
  it('parses a bounded plan and drops unsafe expected paths', () => {
    const sources = [screenshot(), tokens()];
    const plan = parseDesignImplementationPlan(JSON.stringify({
      summary: 'Build the imported landing page.',
      routes: [{ routeId: 'home', path: '/', purpose: 'Landing page', sourceIds: ['source-shot'] }],
      components: [{
        componentId: 'hero',
        name: 'Hero',
        responsibility: 'Render the source hero',
        expectedFiles: ['src/Hero.tsx', '../outside.ts'],
        sourceIds: ['source-shot'],
      }],
      tokens: [{ name: 'brand', value: '#3366ff', sourceIds: ['source-tokens', 'unknown'] }],
      steps: [{
        stepId: 'implement',
        title: 'Implement route',
        details: 'Use the existing router.',
        expectedFiles: ['src/App.tsx'],
        acceptanceCriteria: ['Route renders'],
        sourceIds: ['source-shot', 'source-tokens'],
      }],
      accessibilityChecklist: ['Use one h1'],
      verificationHints: ['Run tests'],
    }), sources, 123);

    expect(plan.generatedAtMs).toBe(123);
    expect(plan.routes[0].sourceIds).toEqual(['source-shot']);
    expect(plan.components[0].expectedFiles).toEqual(['src/Hero.tsx']);
    expect(plan.tokens[0].sourceIds).toEqual(['source-tokens']);
    expect(plan.sourceRevision).toBe(designSourceRevision(sources));
  });

  it('rejects routes and steps that do not map to a supplied source', () => {
    expect(() => parseDesignImplementationPlan(JSON.stringify({
      routes: [{ path: '/', sourceIds: ['invented'] }],
      steps: [{ title: 'Guess', sourceIds: ['invented'] }],
    }), [screenshot()])).toThrow('no usable source-mapped route');
  });

  it('invalidates a plan revision when source content changes', () => {
    const first = tokens();
    const second = createLocalDesignSource({
      ...first,
      kind: 'design_tokens',
      textContent: JSON.stringify({ color: { brand: '#ff0000' } }),
      imageDataUrl: null,
    });
    expect(designSourceRevision([first])).not.toBe(designSourceRevision([second]));
  });

  it('excludes live image bytes from JSON exports', () => {
    const exported = exportDesignProjectJson({ title: 'Demo', sources: [screenshot()] });
    expect(exported).not.toContain(PNG);
    expect(JSON.parse(exported).project.sources[0].imageDataUrl).toBeNull();
  });
});
