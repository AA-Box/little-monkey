import type { WorkflowRunMeta } from '../store/sessionStore';

/** Creates a reviewed chat draft; never invokes a tool or grants permissions.
 * A failed graph may have already modified files before interruption. */
export function prepareSwarmResumeDraft(
  runId: string,
  meta: WorkflowRunMeta,
): { text: string; requiresMutationReview: boolean } {
  if (!runId.trim() || runId.length > 200 ||
      !/^[a-zA-Z0-9_.:#-]+$/.test(runId)) {
    throw new Error('Invalid saved workflow run identifier.');
  }
  if (meta.status !== 'error' && meta.status !== 'cancelled') {
    throw new Error('Only interrupted, failed or cancelled workflows can be resumed.');
  }
  if (!meta.spec || !Array.isArray(meta.spec.phases) || !meta.spec.phases.length) {
    throw new Error('This older workflow has no recoverable specification.');
  }
  const agents = meta.spec.phases.flatMap(phase => phase.agents);
  if (agents.length > 16 || agents.some(agent =>
    typeof agent.prompt !== 'string' || agent.prompt.length > 8_000 ||
    typeof agent.profile !== 'string')) {
    throw new Error('Saved workflow must be validated again before resume.');
  }
  const requiresMutationReview = agents.some(agent => agent.profile !== 'explore' || Boolean(agent.isolation));
  const instruction = requiresMutationReview
    ? 'Before resuming, inspect the current repository status, diff, and previously executed changes. Do not repeat file, shell, Git, or other mutations unless I approve them under Little Monkey’s existing permission policy.'
    : 'Resume the read-only investigation. Verify previous evidence against the current repository before accepting it.';
  return {
    requiresMutationReview,
    text: 'Please resume this interrupted Little Monkey workflow in this conversation. ' +
      instruction +
      '\n\nUse the workflow tool with exactly these arguments (no new workflow):\n' +
      '```json\n' + JSON.stringify({ resume: runId }, null, 2) + '\n```' +
      '\n\nReport which completed reports were reused and which agents actually ran. Stop on authorization or persistence errors.',
  };
}
