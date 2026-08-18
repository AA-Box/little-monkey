/**
 * The reflection pass as a service, not as a closure that dies with the turn
 * that produced the signal.
 *
 * `reflectOnCandidate` needs one model call, and `agentLoop.ts` used to be the
 * only thing that could make one — which meant a detected candidate could only
 * ever be drafted while its own turn was still alive. In `Suggest only` (the
 * default) that made every detected candidate a dead end, and after a restart
 * it made every one of them permanently undraftable.
 *
 * This module owns the standalone call, using the currently configured model
 * target and the app's normal privacy gate, so Settings can draft a candidate
 * days later and get exactly the same bounded, backend-evidenced reflection.
 */
import { resolveTarget } from "./agentLoop";
import { effortForTarget } from "../store/modelStore";
import { attemptStream } from "./turnEngine";
import { reflectOnCandidate, type ReflectionCall, type ReflectionOutcome } from "./skillLearning";
import { skillLearningClient, type LearningCandidate } from "./skillLearningClient";

/** One reflection call against the configured target. Mirrors what
 * `agentLoop.ts` hands the in-turn path, so both routes produce the same
 * proposal shape from the same evidence. */
export function createReflectionCall(): ReflectionCall {
  return async (messages, tools, signal) => {
    const target = await resolveTarget();
    const result = await attemptStream(
      target,
      messages,
      tools,
      signal,
      effortForTarget(target),
      `skill-learning-reflection-${Date.now()}`,
    );
    return { content: result.content, toolCalls: result.toolCalls, streamError: result.streamError };
  };
}

/**
 * Drafts one detected candidate on demand — the action behind "Generate
 * draft" in Settings. Uses the same bounded reflection implementation as
 * automatic staging; the only difference is who asked for it.
 */
export async function draftCandidate(
  candidateId: string,
  signal?: AbortSignal,
): Promise<ReflectionOutcome> {
  const candidate: LearningCandidate = await skillLearningClient.candidate(candidateId);
  return reflectOnCandidate(candidate, createReflectionCall(), { signal });
}
