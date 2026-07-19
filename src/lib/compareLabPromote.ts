/**
 * Model Compare Lab "promote" actions (ROADMAP.md Phase 2): take a winning
 * response, prompt, or model from a suite run's report and apply it back
 * into normal chat/workflows. Every function here is called from a single
 * explicit user click in `CompareLabView.tsx` — nothing in this module (or
 * anywhere in `compareLabRunner.ts`) calls these automatically.
 */
import { usePromptStore, slugify, type PromptEntry } from "../store/promptStore";
import { useSessionStore } from "../store/sessionStore";
import { useModelStore } from "../store/modelStore";
import type { LabPrompt, LabResult } from "./compareLab";
import type { ModelTargetSnapshot } from "./modelTargets";

const PROMOTED_COMMAND_MAX_LENGTH = 32;

/** Builds a command slug for a promoted prompt that doesn't collide with any
 * existing prompt-library entry — mirrors `promptStore.ts`'s internal
 * (unexported) `uniqueCommand` dedup shape, since a promoted prompt goes
 * through the same public `addEntry` API any other library entry does and
 * gets no special uniqueness handling from the store itself. */
function uniquePromotedCommand(base: string, existing: readonly PromptEntry[]): string {
  const taken = new Set(existing.map((entry) => entry.command));
  const root = (base || "compare-lab-prompt").slice(0, PROMOTED_COMMAND_MAX_LENGTH);
  if (!taken.has(root)) return root;
  let n = 2;
  let candidate = "";
  do {
    const suffix = `-${n}`;
    candidate = `${root.slice(0, Math.max(0, PROMOTED_COMMAND_MAX_LENGTH - suffix.length))}${suffix}`;
    n += 1;
  } while (taken.has(candidate));
  return candidate;
}

/** Promotes a winning response into a brand-new normal chat session, pinned
 * to the exact model that produced it, seeded with the prompt and its
 * answer already in the transcript — the user can continue the conversation
 * normally from there under ordinary chat rules (tools included, subject to
 * the usual permission gates; nothing special about a promoted session).
 * Returns the new session's id. */
export function promoteLabResponse(prompt: LabPrompt, result: LabResult, target: ModelTargetSnapshot): string {
  const store = useSessionStore.getState();
  store.newSession();
  const sessionId = useSessionStore.getState().activeSessionId;
  store.setSessionModelTarget(sessionId, target);
  store.addMessage(sessionId, { role: "user", content: prompt.text });
  store.addMessage(sessionId, { role: "assistant", content: result.content });
  const title = prompt.text.trim().slice(0, 60) || "Promoted from Compare Lab";
  store.renameSession(sessionId, title);
  return sessionId;
}

/** Saves a suite prompt as a reusable prompt-library snippet, insertable
 * anywhere in normal chat via its generated `/command`. Returns the created
 * entry. */
export function promoteLabPrompt(prompt: LabPrompt, suiteName: string): PromptEntry {
  const promptStore = usePromptStore.getState();
  const command = uniquePromotedCommand(slugify(suiteName) || "compare-lab", promptStore.entries);
  return promptStore.addEntry({
    kind: "snippet",
    name: `${suiteName} prompt`.slice(0, 80),
    command,
    content: prompt.text,
    description: `Promoted from a Model Compare Lab run of "${suiteName}".`,
  });
}

/** Sets a winning target as the app's active model, so the very next normal
 * chat turn uses it — provider/Ollama switches take effect immediately;
 * activating a local llama.cpp target additionally starts the managed
 * runtime, so this is async. Throws if the local model is no longer
 * installed rather than silently leaving the previous model active. */
export async function promoteLabModel(target: ModelTargetSnapshot): Promise<void> {
  const modelStore = useModelStore.getState();
  if (target.kind === "provider") {
    modelStore.useProviderModel(target.providerId, target.model);
    return;
  }
  if (target.kind === "ollama") {
    modelStore.useOllamaModel(target.model);
    return;
  }
  const info = modelStore.installed.find((model) => model.id === target.modelId);
  if (!info) throw new Error(`${target.displayName} is no longer installed locally.`);
  await modelStore.start(info);
}
