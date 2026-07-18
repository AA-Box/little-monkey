import type { AttachmentRef } from "./agentLoop";
import { startComparison, startComparisonSynthesis, type ComparisonRunHandle } from "./compareRunner";
import {
  assertValidComparisonTargets,
  buildModelTargetInventory,
  findActiveModelTarget,
  MAX_COMPARISON_TARGETS,
  MIN_COMPARISON_TARGETS,
  type ModelTargetSnapshot,
} from "./modelTargets";
import type { SkillInvocationSnapshot } from "./skills";
import { useModelStore } from "../store/modelStore";
import { useSessionStore } from "../store/sessionStore";
import { DEFAULT_PROVIDER_MODEL_FILTER, useSettingsStore } from "../store/settingsStore";

/** Whether `target` should be offered to Ultracode's auto-picker — mirrors
 * `CompareTargetPicker.tsx`'s own curation filter so Ultracode never fans
 * out to a provider model the user explicitly hid via its "showAll"/
 * "selectedModelIds" filter in Settings. Non-provider targets (local,
 * Ollama) have no such filter and are always eligible. */
function isCurated(target: ModelTargetSnapshot, providerModelFilters: ReturnType<typeof useSettingsStore.getState>["providerModelFilters"]): boolean {
  if (target.kind !== "provider") return true;
  const filter = providerModelFilters[target.providerId] ?? DEFAULT_PROVIDER_MODEL_FILTER;
  return filter.showAll || filter.selectedModelIds.length === 0 || filter.selectedModelIds.includes(target.model);
}

/** Auto-selects up to `MAX_COMPARISON_TARGETS` distinct available model
 * targets for an Ultracode run: the active target first (if available),
 * then other available/curated targets, capping local llama.cpp targets at
 * 1 (the same "only one local model" rule `validateComparisonTargets`
 * already enforces for manual Compare runs). */
function pickUltracodeTargets(): ModelTargetSnapshot[] {
  const modelState = useModelStore.getState();
  const inventory = buildModelTargetInventory({
    installed: modelState.installed,
    active: modelState.active,
    llamaStatus: modelState.llamaStatus,
    ollamaModels: modelState.ollamaModels,
    ollamaReachable: modelState.ollamaReachable,
    providers: modelState.providers,
    providerModels: modelState.providerModels,
    effortByTarget: modelState.effortByTarget,
  });
  const active = findActiveModelTarget(inventory, modelState);
  const providerModelFilters = useSettingsStore.getState().providerModelFilters;

  const available = inventory.targets.filter(
    (target) => target.availability.status === "available" && isCurated(target, providerModelFilters),
  );
  const ordered = active && available.some((target) => target.key === active.key)
    ? [active, ...available.filter((target) => target.key !== active.key)]
    : available;

  const picked: ModelTargetSnapshot[] = [];
  let localCount = 0;
  for (const target of ordered) {
    if (picked.length >= MAX_COMPARISON_TARGETS) break;
    if (target.kind === "local") {
      if (localCount >= 1) continue;
      localCount += 1;
    }
    picked.push(target);
  }
  return picked;
}

/** Reads back which of this comparison's branches actually completed, and
 * picks the one to run the synthesis — preferring `preferredKey` (the
 * target that led the fan-out) to mirror `CompareView.tsx`'s
 * `SynthesisPanel`, which defaults its own picker to the first completed
 * branch. Returns `null` when fewer than 2 branches completed, since
 * `startComparisonSynthesis` requires at least 2 source responses. */
function pickSynthesisTarget(groupId: string, preferredKey: string): ModelTargetSnapshot | null {
  const completed = useSessionStore
    .getState()
    .sessions.filter(
      (session): session is typeof session & { modelTarget: ModelTargetSnapshot } =>
        session.comparisonBranch?.comparisonId === groupId &&
        session.comparisonBranch.status === "completed" &&
        session.modelTarget !== null,
    );
  if (completed.length < MIN_COMPARISON_TARGETS) return null;
  const preferred = completed.find((session) => session.modelTarget.key === preferredKey);
  return (preferred ?? completed[0]).modelTarget;
}

/** Ultracode: auto-runs the user's prompt across up to `MAX_COMPARISON_TARGETS`
 * available models (see `pickUltracodeTargets`) through the unmodified
 * Compare pipeline (`startComparison`), then auto-fires a synthesis
 * (`startComparisonSynthesis`) once every branch settles — the user lands in
 * the same `CompareView`/`SynthesisPanel` a manual Compare would show, just
 * without having to pick targets or click "Synthesize" themselves. */
export async function startUltracode(
  sessionId: string,
  prompt: string,
  attachments: readonly AttachmentRef[],
  skillInvocations: readonly SkillInvocationSnapshot[] = [],
): Promise<ComparisonRunHandle> {
  const targets = pickUltracodeTargets();
  if (targets.length < MIN_COMPARISON_TARGETS) {
    throw new Error(
      `Ultracode needs at least ${MIN_COMPARISON_TARGETS} available models — connect another provider or local model first.`,
    );
  }
  assertValidComparisonTargets(targets);

  const handle = await startComparison(sessionId, prompt, attachments, targets, skillInvocations);

  // Fire-and-forget: branches run in the background (same as a manual
  // Compare); once they settle, auto-trigger the synthesis pass instead of
  // requiring the user to click "Synthesize" in CompareView.
  void (async () => {
    await handle.done;
    const synthesisTarget = pickSynthesisTarget(handle.groupId, targets[0].key);
    if (!synthesisTarget) return;
    try {
      await startComparisonSynthesis(handle.groupId, synthesisTarget).done;
    } catch {
      // Swallowed: CompareView's SynthesisPanel already surfaces branch
      // failures and offers a manual retry — nothing else to surface here.
    }
  })();

  return handle;
}
