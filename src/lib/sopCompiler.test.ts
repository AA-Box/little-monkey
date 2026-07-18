import { describe, expect, it } from 'vitest';

import {
  buildSopCompilerMessages,
  compileSop,
  parseSopCompilerResponse,
  renderCompiledSkillInstructions,
  slugifyCommand,
  type CompiledWorkflowDraft,
  type SopCompilerCallResult,
} from './sopCompiler';

const WELL_FORMED_REPLY = JSON.stringify({
  name: 'Rotate API Credentials',
  summary: 'Rotates the payments API key and confirms the new key is live.',
  suggestedCommand: 'rotate-api-credentials',
  steps: [
    { order: 1, action: 'Generate a new API key in the provider dashboard.' },
    { order: 2, action: 'Update the secret in the deployment vault.' },
  ],
  inputs: [{ name: 'environment', description: 'Which environment to rotate.', required: true }],
  policyGates: [{ label: 'Requires on-call approval', description: 'Rotating prod keys needs sign-off.', riskLevel: 'high' }],
  tests: [{ label: 'New key authenticates successfully', expected: 'A test request with the new key returns 200.' }],
  evidence: [{ label: 'Vault audit entry', description: 'Screenshot of the updated vault secret version.' }],
});

describe('slugifyCommand', () => {
  it('lowercases and hyphenates a human name', () => {
    expect(slugifyCommand('Rotate API Credentials')).toBe('rotate-api-credentials');
  });

  it('strips leading/trailing separators', () => {
    expect(slugifyCommand('  --Weird Name!!--  ')).toBe('weird-name');
  });

  it('falls back to a safe default when nothing usable survives', () => {
    expect(slugifyCommand('!!!')).toBe('sop-compiled-workflow');
  });

  it('caps length at 32 characters', () => {
    const long = slugifyCommand('a'.repeat(80));
    expect(long.length).toBeLessThanOrEqual(32);
  });
});

describe('buildSopCompilerMessages', () => {
  it('produces a system + user message pair with the source text embedded', () => {
    const messages = buildSopCompilerMessages('Step 1: do the thing.', 'runbook.md');
    expect(messages).toHaveLength(2);
    expect(messages[0].role).toBe('system');
    expect(messages[1].role).toBe('user');
    expect(messages[1].content).toContain('Step 1: do the thing.');
    expect(messages[1].content).toContain('runbook.md');
  });

  it('truncates a very long source document', () => {
    const huge = 'x'.repeat(30_000);
    const messages = buildSopCompilerMessages(huge);
    expect(messages[1].content.length).toBeLessThan(30_000);
    expect(messages[1].content).toContain('…');
  });
});

describe('parseSopCompilerResponse', () => {
  it('parses a well-formed compiler reply', () => {
    const draft = parseSopCompilerResponse(WELL_FORMED_REPLY);
    expect(draft).not.toBeNull();
    expect(draft?.name).toBe('Rotate API Credentials');
    expect(draft?.suggestedCommand).toBe('rotate-api-credentials');
    expect(draft?.steps).toHaveLength(2);
    expect(draft?.inputs).toEqual([{ name: 'environment', description: 'Which environment to rotate.', required: true }]);
    expect(draft?.policyGates[0].riskLevel).toBe('high');
  });

  it('salvages a JSON object embedded in extra prose', () => {
    const draft = parseSopCompilerResponse(`Sure, here you go:\n${WELL_FORMED_REPLY}\nHope that helps!`);
    expect(draft?.name).toBe('Rotate API Credentials');
  });

  it('fails closed on malformed JSON', () => {
    expect(parseSopCompilerResponse('not json at all')).toBeNull();
  });

  it('fails closed when name is missing', () => {
    expect(parseSopCompilerResponse('{"summary":"missing a name"}')).toBeNull();
  });

  it('fails closed when summary is missing', () => {
    expect(parseSopCompilerResponse('{"name":"missing a summary"}')).toBeNull();
  });

  it('backstops missing inputs/policyGates/tests/evidence with non-empty fallbacks', () => {
    const draft = parseSopCompilerResponse(JSON.stringify({ name: 'Bare SOP', summary: 'A minimal SOP with nothing extracted.' }));
    expect(draft).not.toBeNull();
    expect(draft?.inputs.length).toBeGreaterThan(0);
    expect(draft?.policyGates.length).toBeGreaterThan(0);
    expect(draft?.tests.length).toBeGreaterThan(0);
    expect(draft?.evidence.length).toBeGreaterThan(0);
  });

  it('defaults an out-of-enum risk level to medium rather than dropping the gate', () => {
    const draft = parseSopCompilerResponse(
      JSON.stringify({
        name: 'X',
        summary: 'Y',
        policyGates: [{ label: 'Some gate', riskLevel: 'catastrophic' }],
      }),
    );
    expect(draft?.policyGates[0].riskLevel).toBe('medium');
  });

  it('drops a malformed entry within an array but keeps the rest', () => {
    const draft = parseSopCompilerResponse(
      JSON.stringify({
        name: 'X',
        summary: 'Y',
        inputs: [{ name: 'good_input' }, { description: 'no name here' }],
      }),
    );
    expect(draft?.inputs).toHaveLength(1);
    expect(draft?.inputs[0].name).toBe('good_input');
  });
});

describe('compileSop', () => {
  it('returns a validated draft from a successful call', async () => {
    const callModel = async (): Promise<SopCompilerCallResult> => ({ content: WELL_FORMED_REPLY, streamError: null });
    const draft = await compileSop('Some SOP text describing rotating credentials.', callModel);
    expect(draft.name).toBe('Rotate API Credentials');
  });

  it('throws before ever calling the model when the source is empty', async () => {
    let called = false;
    const callModel = async (): Promise<SopCompilerCallResult> => {
      called = true;
      return { content: WELL_FORMED_REPLY, streamError: null };
    };
    await expect(compileSop('   ', callModel)).rejects.toThrow(/paste or import/i);
    expect(called).toBe(false);
  });

  it('surfaces a stream error rather than swallowing it', async () => {
    const callModel = async (): Promise<SopCompilerCallResult> => ({ content: '', streamError: 'model unreachable' });
    await expect(compileSop('Some SOP text.', callModel)).rejects.toThrow('model unreachable');
  });

  it('throws a descriptive error when the model reply is unparseable', async () => {
    const callModel = async (): Promise<SopCompilerCallResult> => ({ content: 'not json', streamError: null });
    await expect(compileSop('Some SOP text.', callModel)).rejects.toThrow(/did not return a compilable workflow/i);
  });
});

describe('renderCompiledSkillInstructions', () => {
  const draft: CompiledWorkflowDraft = {
    name: 'Rotate API Credentials',
    summary: 'Rotates the payments API key.',
    suggestedCommand: 'rotate-api-credentials',
    steps: [{ order: 1, action: 'Generate a new key.' }],
    inputs: [{ name: 'environment', description: 'Target environment.', required: true }],
    policyGates: [{ label: 'On-call approval', description: 'Needs sign-off.', riskLevel: 'high' }],
    tests: [{ label: 'New key works', expected: 'Test request returns 200.' }],
    evidence: [{ label: 'Vault entry', description: 'Screenshot of the new secret version.' }],
  };

  it('includes every declared section and an explicit inactive-until-reviewed notice', () => {
    const rendered = renderCompiledSkillInstructions(draft, 'Original SOP excerpt.');
    expect(rendered).toContain('Rotate API Credentials');
    expect(rendered).toContain('stays inactive until reviewed');
    expect(rendered).toContain('## Steps');
    expect(rendered).toContain('Generate a new key.');
    expect(rendered).toContain('## Required inputs');
    expect(rendered).toContain('`environment`');
    expect(rendered).toContain('## Policy / permission gates');
    expect(rendered).toContain('[HIGH] On-call approval');
    expect(rendered).toContain('## Acceptance / test checklist');
    expect(rendered).toContain('New key works');
    expect(rendered).toContain('## Required evidence');
    expect(rendered).toContain('Vault entry');
    expect(rendered).toContain('Original SOP excerpt.');
  });
});
