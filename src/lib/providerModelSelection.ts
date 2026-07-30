export interface ProviderModelSelectionFilter {
  showAll: boolean;
  selectedModelIds: readonly string[];
}

export interface ActiveProviderModelSelection {
  activeProvider: "local" | "ollama" | "provider";
  activeProviderId: string | null;
  activeProviderModel: string | null;
}

/**
 * Applies one provider's persisted model allowlist. The active model is kept
 * visible even after it is unchecked so every switching surface still shows
 * the target the current chat is actually using.
 */
export function visibleProviderModels<T extends { id: string }>(
  models: readonly T[],
  filter: ProviderModelSelectionFilter,
  activeModelId: string | null = null,
): T[] {
  if (filter.showAll) return [...models];
  const selected = new Set(filter.selectedModelIds);
  return models.filter(
    (model) => selected.has(model.id) || model.id === activeModelId,
  );
}

/** Resolves the active-model exception for a particular provider. */
export function visibleProviderModelsForProvider<T extends { id: string }>(
  providerId: string,
  models: readonly T[],
  filter: ProviderModelSelectionFilter,
  active: ActiveProviderModelSelection,
): T[] {
  const activeModelId =
    active.activeProvider === "provider" && active.activeProviderId === providerId
      ? active.activeProviderModel
      : null;
  return visibleProviderModels(models, filter, activeModelId);
}
