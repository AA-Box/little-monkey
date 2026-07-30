import { useEffect, useMemo, useState } from "react";
import { Button } from "../ui";
import { useModelStore, type ProviderModelInfo } from "../../store/modelStore";
import { useSettingsStore, DEFAULT_PROVIDER_MODEL_FILTER } from "../../store/settingsStore";
import { useT } from "../../lib/i18n";

/**
 * Stable "no cached model list yet" fallback — see `ProviderCard.tsx`'s
 * identical `EMPTY_MODELS` comment for why this can't be a fresh `[]`
 * inlined in the selector below (infinite re-render loop).
 */
const EMPTY_MODELS: ProviderModelInfo[] = [];

/**
 * Dedicated Settings tab for curating which connected-provider models show
 * up in model pickers. OpenRouter can return 400+ models, and custom
 * OpenAI-compatible providers may be similarly large, so every connected
 * provider gets this same bounded, searchable selection surface.
 */
interface ProviderModelsPanelProps {
  providerId: string;
  providerLabel: string;
}

export function ProviderModelsPanel({
  providerId,
  providerLabel,
}: ProviderModelsPanelProps) {
  const models = useModelStore((s) => s.providerModels[providerId] ?? EMPTY_MODELS);
  const refreshProviderModels = useModelStore((s) => s.refreshProviderModels);
  const filter = useSettingsStore(
    (s) => s.providerModelFilters[providerId] ?? DEFAULT_PROVIDER_MODEL_FILTER,
  );
  const setSelection = useSettingsStore((s) => s.setProviderModelSelection);
  const toggleSelected = useSettingsStore((s) => s.toggleProviderModelSelected);
  const clearSelection = useSettingsStore((s) => s.clearProviderModelSelection);
  const { t } = useT();

  const [search, setSearch] = useState("");

  useEffect(() => {
    if (models.length === 0) void refreshProviderModels(providerId);
  }, [models.length, providerId, refreshProviderModels]);

  const availableModelIds = useMemo(
    () => models.map((model) => model.id),
    [models],
  );
  const availableModelIdSet = useMemo(
    () => new Set(availableModelIds),
    [availableModelIds],
  );
  const selectedCount = filter.showAll
    ? models.length
    : new Set(
        filter.selectedModelIds.filter((modelId) => availableModelIdSet.has(modelId)),
      ).size;
  const allModelsSelected =
    models.length > 0 &&
    (filter.showAll || selectedCount === models.length);

  const filteredModels = useMemo(() => {
    const needle = search.trim().toLowerCase();
    if (!needle) return models;
    return models.filter((model) => model.id.toLowerCase().includes(needle));
  }, [models, search]);

  return (
    <div className="flex h-full min-h-0 min-w-0 flex-col gap-3 py-2">
      <p className="shrink-0 text-xs text-muted">
        {t("OpenRouterModelsPanel.description", { provider: providerLabel })}
      </p>

      <label className="flex shrink-0 cursor-pointer items-start gap-2 text-sm text-foreground">
        <input
          type="checkbox"
          checked={allModelsSelected}
          disabled={models.length === 0}
          onChange={(event) => {
            if (event.target.checked) {
              setSelection(providerId, availableModelIds, availableModelIds);
            } else {
              clearSelection(providerId);
            }
          }}
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
        <div className="flex min-h-0 flex-1 flex-col gap-3">
          <div className="flex shrink-0 items-center gap-2">
            <input
              type="text"
              value={search}
              onChange={(event) => setSearch(event.target.value)}
              aria-label={t("ProviderCard.filterModelsPlaceholder", { count: models.length })}
              placeholder={t("ProviderCard.filterModelsPlaceholder", { count: models.length })}
              className="h-8 min-w-0 flex-1 rounded-md border border-border bg-surface px-2.5 font-mono text-xs text-foreground placeholder:font-sans placeholder:text-faint focus:outline-none focus:ring-2 focus:ring-accent"
            />
            <Button
              variant="ghost"
              size="sm"
              onClick={() => clearSelection(providerId)}
              disabled={selectedCount === 0}
            >
              {t("OpenRouterModelsPanel.clearSelection")}
            </Button>
          </div>

          <p className="shrink-0 text-xs text-faint" aria-live="polite">
            {t("OpenRouterModelsPanel.selectedCount", { selected: selectedCount, total: models.length })}
          </p>

          <div className="flex min-h-0 flex-1 flex-col gap-0.5 overflow-y-auto rounded-md border border-border/60 p-1 [overscroll-behavior:contain]">
            {filteredModels.map((model) => (
              <label
                key={model.id}
                className="flex min-h-10 cursor-pointer items-center gap-2 rounded-md px-2 py-2 text-sm hover:bg-surface-2"
              >
                <input
                  type="checkbox"
                  checked={filter.showAll || filter.selectedModelIds.includes(model.id)}
                  onChange={() => toggleSelected(providerId, model.id, availableModelIds)}
                  className="accent-accent"
                />
                <span className="truncate font-mono text-xs">{model.id}</span>
              </label>
            ))}
            {filteredModels.length === 0 && (
              <p className="px-1 text-xs text-faint">{t("ProviderCard.noModelsMatch", { filter: search })}</p>
            )}
          </div>
        </div>
      )}
    </div>
  );
}
