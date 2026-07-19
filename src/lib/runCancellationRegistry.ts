/**
 * Process-local bridge from a durable run id to the active engine's real
 * cancellation primitive. Every desktop execution surface registers here;
 * Run Center and another window can therefore stop the exact model/tool
 * request without knowing whether it is chat, Compare, Crew, or a workflow.
 */
const cancellations = new Map<string, () => void>();

export function registerRunCancellation(runId: string, cancel: () => void): () => void {
  cancellations.set(runId, cancel);
  return () => {
    if (cancellations.get(runId) === cancel) cancellations.delete(runId);
  };
}

export function cancelRegisteredRun(runId: string): boolean {
  const cancel = cancellations.get(runId);
  if (!cancel) return false;
  cancel();
  return true;
}

export function hasRegisteredRun(runId: string): boolean {
  return cancellations.has(runId);
}

export function clearRunCancellationRegistryForTests(): void {
  cancellations.clear();
}
