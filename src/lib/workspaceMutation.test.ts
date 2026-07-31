import { describe, expect, it } from 'vitest';

import {
  canRetryWithoutTools,
  isExplicitWorkspaceMutationRequest,
  mutationAttemptFailureMessage,
  mutationPlainResponseAction,
  mutationToolFailureReason,
  requiresWorkspaceMutation,
  workspaceMutationPreflightFailure,
  WORKSPACE_MUTATION_FAILURE,
} from './workspaceMutation';

describe('isExplicitWorkspaceMutationRequest', () => {
  it.each([
    'Fix the bug in src/auth.ts.',
    'Please create a new file at src/components/Badge.tsx.',
    'I want you to make the real changes in my codebase, not paste code in chat.',
    'Review this module, then fix the issue in auth.ts.',
    'Go ahead and implement it.',
    'Save this snippet into src/sort.ts.',
    'The app should update the package configuration.',
    'Do not write code in chat; edit src/auth.ts instead.',
    'Without changing app behavior, refactor src/auth.ts.',
    'The plan is approved. Execute it now.',
    'Add dark mode.',
    'Create a login form.',
    'I want users to be able to install Ollama models directly in my app.',
    'I want the app to show thinking and edits like Codex.',
    'Rename src/old.ts to src/new.ts.',
    'Move src/legacy.ts into src/archive/legacy.ts.',
    'Delete the obsolete src/legacy.ts file.',
    'Install dependencies with pnpm.',
    'Do not edit files yet; review first, then fix the bug in src/auth.ts.',
  ])('recognizes an explicit workspace mutation request: %s', (request) => {
    expect(isExplicitWorkspaceMutationRequest(request)).toBe(true);
  });

  it.each([
    'Explain how src/auth.ts works.',
    'Review src/auth.ts for bugs.',
    'Analyze the project architecture.',
    'Show me a code snippet that fixes the parser.',
    'How can I edit src/auth.ts?',
    'Do not edit any files; just explain the issue.',
    'Inspect this read-only and recommend changes.',
    'Change your tone to be friendlier.',
    'Create an illustration of a monkey.',
    'What would you change in src/auth.ts?',
    'Do not edit files; review, then fix the prose in your answer.',
    'How would you inspect and fix this bug?',
    'Can you explain how to review and implement this feature?',
  ])('leaves explanation, review, and snippet requests as normal chat: %s', (request) => {
    expect(isExplicitWorkspaceMutationRequest(request)).toBe(false);
  });
});

describe('requiresWorkspaceMutation', () => {
  it('never enables the contract in Plan Mode', () => {
    expect(requiresWorkspaceMutation('Fix src/auth.ts now.', 'plan')).toBe(false);
    expect(requiresWorkspaceMutation('The plan is approved. Execute it now.', 'plan')).toBe(false);
  });

  it('enables the contract for the same explicit request in an acting mode', () => {
    expect(requiresWorkspaceMutation('Fix src/auth.ts now.', 'manual')).toBe(true);
    expect(requiresWorkspaceMutation('The plan is approved. Execute it now.', 'acceptEdits')).toBe(true);
  });
});

describe('workspace mutation contract decisions', () => {
  it('requires a folder-picker-authorized workspace before an action turn', () => {
    const failure = workspaceMutationPreflightFailure(true, null);
    expect(failure).toContain('No files changed');
    expect(failure).toContain('folder picker');
    expect(failure).toContain('path typed in chat');
    expect(workspaceMutationPreflightFailure(true, '/work/project')).toBeNull();
    expect(workspaceMutationPreflightFailure(false, null)).toBeNull();
  });

  it('blocks mutation turns when a restored chat belongs to a different workspace', () => {
    const failure = workspaceMutationPreflightFailure(true, '/work/other', '/work/project');
    expect(failure).toContain('No files changed');
    expect(failure).toContain('linked to "/work/project"');
    expect(failure).toContain('active workspace is "/work/other"');
    expect(workspaceMutationPreflightFailure(true, '/work/project/', '/work/project')).toBeNull();
  });

  it('never permits the tool-less provider fallback for mutation turns', () => {
    expect(canRetryWithoutTools(true)).toBe(false);
    expect(canRetryWithoutTools(false)).toBe(true);
  });

  it('retries one plain response, then fails unless a real mutation succeeded', () => {
    expect(mutationPlainResponseAction(true, false, false)).toBe('retry');
    expect(mutationPlainResponseAction(true, false, true)).toBe('fail');
    expect(mutationPlainResponseAction(true, false, false, true)).toBe('fail');
    expect(mutationPlainResponseAction(true, true, true)).toBe('accept');
    expect(mutationPlainResponseAction(true, true, true, true)).toBe('fail');
    expect(mutationPlainResponseAction(false, false, false)).toBe('accept');
    expect(mutationPlainResponseAction(false, true, true, true)).toBe('accept');
    expect(WORKSPACE_MUTATION_FAILURE).toMatch(/^No files changed\./);
  });

  it('reports partial mutation failures without claiming that nothing changed', () => {
    expect(mutationAttemptFailureMessage(false, 'Permission denied')).toBe(
      'No files changed. A requested file edit was not applied: Permission denied',
    );
    expect(mutationAttemptFailureMessage(true, 'Permission denied')).toBe(
      'Some files changed, but a requested file edit was not applied: Permission denied',
    );
  });

  it('extracts a bounded reason from a failed or denied mutation tool result', () => {
    expect(mutationToolFailureReason('{"error":"Permission denied by the user"}')).toBe(
      'Permission denied by the user',
    );
    expect(mutationToolFailureReason('{"ok":true}')).toBeNull();
    expect(mutationToolFailureReason('not json')).toBeNull();
    expect(mutationToolFailureReason(JSON.stringify({ error: 'x'.repeat(600) }))).toHaveLength(500);
  });
});
