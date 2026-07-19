import { useEffect, useMemo, useState } from "react";
import { Button } from "../ui";
import { useModelStore, type ProviderModelInfo } from "../../store/modelStore";
import { useSettingsStore, DEFAULT_PROVIDER_MODEL_FILTER } from "../../store/settingsStore";
import { useT } from "../../lib/i18n";

const PROVIDER_ID = "openrouter";

/**
 * Stable "no cached model list yet" fallback — see `ProviderCard.tsx`'s
 * identical `EMPTY_MODELS` comment for why this can't be a fresh `[]`
 * inlined in the selector below (infinite re-render loop).
 */
const EMPTY_MODELS: ProviderModelInfo[] = [];

/**
 * Dedicated Settings tab for curating which OpenRouter models show up in
 * the chat toolbar's `ModelSwitcher` — OpenRouter alone returns 400+ models
 * (see `ProviderCard.tsx`'s `FILTER_THRESHOLD`), so surfacing all of them
 * there unfiltered makes that dropdown unusable. Only rendered by
 * `SettingsModal` while OpenRouter has a saved key; the curated selection
 * itself lives in `settingsStore.providerModelFilters` so it survives a
 * restart the same way vision overrides and rate limits do.
 */
export function OpenRouterModelsPanel() {
  const models = useModelStore((s) => s.providerModels[PROVIDER_ID] ?? EMPTY_MODELS);
  const refreshProviderModels = useModelStore((s) => s.refreshProviderModels);
  const filter = useSettingsStore((s) => s.providerModelFilters[PROVIDER_ID] ?? DEFAULT_PROVIDER_MODEL_FILTER);
  const setShowAll = useSettingsStore((s) => s.setProviderModelShowAll);
  const toggleSelected = useSettingsStore((s) => s.toggleProviderModelSelected);
  const clearSelection = useSettingsStore((s) => s.clearProviderModelSelection);
  const { t } = useT();

  const [search, setSearch] = useState("");

  useEffect(() => {
    if (models.length === 0) void refreshProviderModels(PROVIDER_ID);
  }, [models.length, refreshProviderModels]);

  const filteredModels = useMemo(() => {
    const needle = search.trim().toLowerCase();
    if (!needle) return models;
    return models.filter((model) => model.id.toLowerCase().includes(needle));
  }, [models, search]);

  return (
    <div className="flex flex-col gap-3 py-2">
      <p className="text-xs text-muted">{t("OpenRouterModelsPanel.description")}</p>

      <label className="flex items-start gap-2 text-sm text-foreground">
        <input
          type="checkbox"
          checked={filter.showAll}
          onChange={(event) => setShowAll(PROVIDER_ID, event.target.checked)}
          className="mt-0.5 accent-accent"
        />
        <span>
          {t("OpenRouterModelsPanel.showAllToggle")}
          <span className="mt-0.5 block text-xs text-faint">{t("OpenRouterModelsPanel.showAllDescription")}</span>
        </span>
      </label>

      {models.length === 0 ? (
        <p className="px-1 text-xs text-faint">{t("OpenRouterModelsPanel.noModelsLoaded")}</p>
      ) : (
        <>
          <div className="flex items-center gap-2">
            <input
              type="text"
              value={search}
              onChange={(event) => setSearch(event.target.value)}
              placeholder={t("ProviderCard.filterModelsPlaceholder", { count: models.length })}
              className="h-8 min-w-0 flex-1 rounded-md border border-border bg-surface px-2.5 font-mono text-xs text-foreground placeholder:font-sans placeholder:text-faint focus:outline-none focus:ring-2 focus:ring-accent"
            />
            <Button
              variant="ghost"
              size="sm"
              onClick={() => clearSelection(PROVIDER_ID)}
              disabled={filter.selectedModelIds.length === 0}
            >
              {t("OpenRouterModelsPanel.clearSelection")}
            </Button>
          </div>

          <p className="text-xs text-faint">
            {t("OpenRouterModelsPanel.selectedCount", { selected: filter.selectedModelIds.length, total: models.length })}
          </p>

          <div className="flex max-h-96 flex-col gap-0.5 overflow-y-auto">
            {filteredModels.map((model) => (
              <label key={model.id} className="flex items-center gap-2 rounded-md px-2 py-1.5 text-sm hover:bg-surface-2">
                <input
                  type="checkbox"
                  checked={filter.selectedModelIds.includes(model.id)}
                  onChange={() => toggleSelected(PROVIDER_ID, model.id)}
                  className="accent-accent"
                />
                <span className="truncate font-mono text-xs">{model.id}</span>
              </label>
            ))}
            {filteredModels.length === 0 && (
              <p className="px-1 text-xs text-faint">{t("ProviderCard.noModelsMatch", { filter: search })}</p>
            )}
          </div>
        </>
      )}
    </div>
  );
}
