/**
 * The model a channel answers on, as a recipe target.
 *
 * A channel route names a *recipe*, and the model lives inside that recipe's
 * `target:` block — chosen once, at account-creation time, by
 * `channels_cli::starter_recipe_target`, and never surfaced again. Nothing in
 * the app could show which model a route would answer on, let alone change it.
 * This maps between the app's existing model inventory and `RecipeTarget` so
 * the routes UI can do both.
 *
 * Deliberately not `buildModelTargetInventory`: that builds the inventory for a
 * chat turn *starting now*, so its local group holds only the currently-active
 * model and only while `llamaStatus === "ready"`. A recipe target is resolved
 * later, by the runner, which starts the managed runtime itself for the life of
 * the run (see `RecipeTarget::managed_model`'s doc comment) — so every
 * installed chat model is a legal choice here, ready or not.
 */
import type { RecipeTarget } from "../store/recipeStore";
import {
  providerIsConnected,
  type ModelInfo,
  type OllamaModelInfo,
  type ProviderConfig,
  type ProviderModelInfo,
} from "../store/modelStore";

/** One selectable model, flattened for a picker. */
export interface RecipeTargetOption {
  /** Stable identity for React keys and equality — not persisted. */
  readonly key: string;
  /** Backend group ("Local", "Ollama", or the provider's label). */
  readonly group: string;
  /** Model name inside that group. */
  readonly displayName: string;
  readonly target: RecipeTarget;
}

export interface RecipeTargetOptionsInput {
  readonly installed: readonly ModelInfo[];
  readonly ollamaModels: readonly OllamaModelInfo[];
  readonly ollamaReachable: boolean;
  readonly providers: readonly ProviderConfig[];
  readonly providerModels: Readonly<Record<string, readonly ProviderModelInfo[]>>;
}

/**
 * The inventory lists as something safe to iterate.
 *
 * `modelStore` writes each list straight from its backend command, so a
 * command that answers with anything but an array — an older shell, a partial
 * failure, a stub — puts a non-array into the store. That is a picker with
 * nothing in it, which the empty state already covers; it is not a reason to
 * throw out of render and take the whole settings screen down with it.
 */
function iterable<T>(value: readonly T[] | null | undefined): readonly T[] {
  return Array.isArray(value) ? value : [];
}

/** Exactly one field is set, mirroring `RecipeTarget::validate`'s XOR. */
export function managedTarget(modelId: string): RecipeTarget {
  return { managed_model: modelId };
}

export function ollamaTarget(model: string): RecipeTarget {
  return { ollama: model };
}

export function providerTarget(providerId: string, model: string): RecipeTarget {
  return { provider: providerId, model };
}

/**
 * Every model this machine could answer a channel message on.
 *
 * Providers appear only when they are connected, because a provider target
 * with no credential fails at run time with the provider's own 401 — see
 * `providers::read_key`. Connected is `providerIsConnected`, not `has_key`: an
 * extension provider authenticates inside its own sandbox and owns no key
 * here, so `has_key` alone would hide models that do in fact work.
 */
export function buildRecipeTargetOptions(input: RecipeTargetOptionsInput): RecipeTargetOption[] {
  const options: RecipeTargetOption[] = [];
  const seen = new Set<string>();
  const push = (option: RecipeTargetOption) => {
    if (seen.has(option.key)) return;
    seen.add(option.key);
    options.push(option);
  };

  for (const model of iterable(input.installed)) {
    if (model.kind !== "chat" || !model.installed) continue;
    if (!model.id.trim()) continue;
    push({
      key: `managed:${model.id}`,
      group: "Local",
      displayName: model.id,
      target: managedTarget(model.id),
    });
  }

  if (input.ollamaReachable) {
    for (const model of iterable(input.ollamaModels)) {
      if (!model.name.trim()) continue;
      push({
        key: `ollama:${model.name}`,
        group: "Ollama",
        displayName: model.name,
        target: ollamaTarget(model.name),
      });
    }
  }

  for (const provider of iterable(input.providers)) {
    if (!providerIsConnected(provider) || !provider.id.trim()) continue;
    for (const model of iterable(input.providerModels?.[provider.id])) {
      if (!model.id.trim()) continue;
      push({
        key: `provider:${provider.id}/${model.id}`,
        group: provider.label || provider.id,
        displayName: model.id,
        target: providerTarget(provider.id, model.id),
      });
    }
  }

  return options;
}

/** Stable identity for a target, so a saved recipe can be matched to an option. */
export function recipeTargetKey(target: RecipeTarget | null | undefined): string | null {
  if (!target) return null;
  if (target.provider && target.model) return `provider:${target.provider}/${target.model}`;
  if (target.ollama) return `ollama:${target.ollama}`;
  if (target.managed_model) return `managed:${target.managed_model}`;
  if (target.local_url) return `local_url:${target.local_url}`;
  return null;
}

/**
 * What to show for a route's current model.
 *
 * A target naming something this machine no longer has still renders its own
 * name — the recipe says what it says, and showing "unknown" would hide the
 * very thing an operator opened this to find out.
 */
export function recipeTargetLabel(target: RecipeTarget | null | undefined): string {
  if (!target) return "No model set";
  if (target.provider) return target.model ? `${target.provider} · ${target.model}` : target.provider;
  if (target.ollama) return `Ollama · ${target.ollama}`;
  if (target.managed_model) return `Local · ${target.managed_model}`;
  if (target.local_url) return `Custom · ${target.local_url}`;
  return "No model set";
}

/** Whether a target is still offered by this machine's inventory. */
export function isTargetAvailable(
  target: RecipeTarget | null | undefined,
  options: readonly RecipeTargetOption[],
): boolean {
  const key = recipeTargetKey(target);
  if (key === null) return false;
  return options.some((option) => option.key === key);
}
