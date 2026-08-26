import { lazy, Suspense, useEffect, useRef, useState } from "react";
import { Check, ChevronDown, Plus, Search, TriangleAlert } from "lucide-react";

import { useModelStore, type CloudModelRetirementWarning } from "../../store/modelStore";
import type { ModelInfo, OllamaModelInfo } from "../../store/modelStore";
import { useSettingsStore, DEFAULT_PROVIDER_MODEL_FILTER } from "../../store/settingsStore";
import { cloudModelRetirementWarning } from "../../lib/modelRetirement";
import { useT } from "../../lib/i18n";
import { visibleProviderModelsForProvider } from "../../lib/providerModelSelection";

const AddModelDialog = lazy(() =>
  import("./AddModelDialog").then((module) => ({ default: module.AddModelDialog })),
);

/** Connected provider + loaded inventory but no curated rows is a selection
 * state, not a missing API-key/configuration state. */
export function providerModelsEmptyStateKey(
  availableModelCount: number,
): "ModelSwitcher.noCloudModelsSelected" | "ModelSwitcher.noCloudModelsConfigured" {
  return availableModelCount > 0
    ? "ModelSwitcher.noCloudModelsSelected"
    : "ModelSwitcher.noCloudModelsConfigured";
}

/** Shared search predicate kept pure so picker filtering remains cheap/testable. */
export function modelMatchesQuery(query: string, ...values: Array<string | null | undefined>): boolean {
  const needle = query.trim().toLowerCase();
  if (!needle) return true;
  return values.some((value) => value?.toLowerCase().includes(needle));
}

/** Model Retirement and Compatibility Warnings (ROADMAP.md Phase 8, item 14): a plain-language tooltip for a flagged cloud model, favoring a concrete "switch to this" suggestion when one is available. */
function retirementTooltip(
  t: (key: string, vars?: Record<string, string | number>) => string,
  warning: CloudModelRetirementWarning,
): string {
  return warning.suggested_replacement_model_id
    ? t("ModelSwitcher.retiredTooltipWithReplacement", {
        reason: warning.reason,
        replacement: warning.suggested_replacement_model_id,
      })
    : t("ModelSwitcher.retiredTooltipNoReplacement", { reason: warning.reason, note: warning.replacement_note });
}

/**
 * Point-of-use chat model picker. In addition to switching already available
 * targets, it owns discovery/search and opens a lazy-loaded setup dialog, so a
 * user who reaches "No model" never has to know which Settings subsection
 * configures which runtime/provider.
 *
 * `placement` defaults to "up" for ChatWindow's bottom composer; panels near
 * the top of a scroll container pass "down" so the dropdown is not clipped.
 */
export function ModelSwitcher({ placement = "up" }: { placement?: "up" | "down" } = {}) {
  const installed = useModelStore((s) => s.installed);
  const ollamaModels = useModelStore((s) => s.ollamaModels);
  const active = useModelStore((s) => s.active);
  const activeProvider = useModelStore((s) => s.activeProvider);
  const activeOllamaModel = useModelStore((s) => s.activeOllamaModel);
  const start = useModelStore((s) => s.start);
  const useOllamaModel = useModelStore((s) => s.useOllamaModel);
  const providers = useModelStore((s) => s.providers);
  const providerModels = useModelStore((s) => s.providerModels);
  // Subscribe so retirement badges update when the async check resolves.
  useModelStore((s) => s.providerModelRetirements);
  const activeProviderId = useModelStore((s) => s.activeProviderId);
  const activeProviderModel = useModelStore((s) => s.activeProviderModel);
  const useProviderModel = useModelStore((s) => s.useProviderModel);
  const providerModelFilters = useSettingsStore((s) => s.providerModelFilters);
  const { t } = useT();

  // Extension-contributed providers authenticate inside their sandbox and do
  // not own a key, so `is_extension` is just as selectable as `has_key` here.
  const connectedProviders = providers.filter((provider) => provider.has_key || provider.is_extension);
  // Embedding-only GGUFs can be installed for Knowledge Stacks, but they
  // cannot answer chat requests and must never appear as chat targets.
  const installedChatModels = installed.filter((model) => model.kind === "chat");

  const [open, setOpen] = useState(false);
  const [addModelOpen, setAddModelOpen] = useState(false);
  const [query, setQuery] = useState("");
  const containerRef = useRef<HTMLDivElement>(null);

  useEffect(() => {
    if (!open) return;
    function handlePointerDown(event: PointerEvent) {
      if (containerRef.current && !containerRef.current.contains(event.target as Node)) {
        setOpen(false);
      }
    }
    window.addEventListener("pointerdown", handlePointerDown);
    return () => window.removeEventListener("pointerdown", handlePointerDown);
  }, [open]);

  useEffect(() => {
    if (!open) setQuery("");
  }, [open]);

  let label: string | null = null;
  if (activeProvider === "local" && active) {
    label = active.name;
  } else if (activeProvider === "ollama" && activeOllamaModel) {
    label = activeOllamaModel;
  } else if (activeProvider === "provider" && activeProviderModel) {
    label = activeProviderModel;
  }

  const activeRetirement =
    activeProvider === "provider" && activeProviderId && activeProviderModel
      ? cloudModelRetirementWarning(activeProviderId, activeProviderModel)
      : null;

  const filteredLocalModels = installedChatModels.filter((model) =>
    modelMatchesQuery(query, model.id, model.name),
  );
  const providerGroups = connectedProviders
    .map((provider) => {
      const filter = providerModelFilters[provider.id] ?? DEFAULT_PROVIDER_MODEL_FILTER;
      const availableModels = providerModels[provider.id] ?? [];
      const models = visibleProviderModelsForProvider(
        provider.id,
        availableModels,
        filter,
        { activeProvider, activeProviderId, activeProviderModel },
      ).filter((model) => modelMatchesQuery(query, provider.label, model.id));
      return { provider, models };
    })
    .filter((group) => group.models.length > 0);
  const filteredOllamaModels = ollamaModels.filter((model) =>
    modelMatchesQuery(query, "Ollama", model.name),
  );
  const hasAnyVisibleModel =
    filteredLocalModels.length > 0 || providerGroups.length > 0 || filteredOllamaModels.length > 0;
  const totalAvailableModelCount =
    installedChatModels.length +
    ollamaModels.length +
    connectedProviders.reduce((count, provider) => {
      const filter = providerModelFilters[provider.id] ?? DEFAULT_PROVIDER_MODEL_FILTER;
      const availableModels = providerModels[provider.id] ?? [];
      return count + visibleProviderModelsForProvider(
        provider.id,
        availableModels,
        filter,
        { activeProvider, activeProviderId, activeProviderModel },
      ).length;
    }, 0);

  function handleSelectLocal(model: ModelInfo) {
    start(model).catch((error) => {
      console.error("Failed to start local model", error);
    });
    setOpen(false);
  }

  function handleSelectOllama(model: OllamaModelInfo) {
    useOllamaModel(model.name);
    setOpen(false);
  }

  function handleSelectProvider(providerId: string, modelId: string) {
    useProviderModel(providerId, modelId);
    setOpen(false);
  }

  function openAddModel() {
    setOpen(false);
    setAddModelOpen(true);
  }

  return (
    <>
      <div ref={containerRef} className="relative inline-block min-w-0 max-w-[10rem]">
        <button
          type="button"
          onClick={() => setOpen((prev) => !prev)}
          aria-haspopup="true"
          aria-expanded={open}
          className="flex w-full cursor-pointer items-center gap-1 text-xs font-mono text-muted hover:text-foreground"
        >
          {label ? <span className="truncate">{label}</span> : <span className="truncate text-faint">{t("ModelSwitcher.noModel")}</span>}
          {activeRetirement && (
            <span className="shrink-0" title={retirementTooltip(t, activeRetirement)}>
              <TriangleAlert size={12} className="text-warning" aria-label={t("ModelSwitcher.retiredBadge")} />
            </span>
          )}
          <ChevronDown size={12} className="shrink-0" />
        </button>

        {open && (
          <div
            className={`absolute right-0 z-20 flex max-h-[70vh] w-80 flex-col overflow-hidden rounded-lg border border-border bg-background shadow-lg ${
              placement === "down" ? "top-full mt-1" : "bottom-full mb-1"
            }`}
          >
            {totalAvailableModelCount > 0 && (
              <div className="shrink-0 border-b border-border p-2">
                <div className="relative">
                  <Search size={14} className="pointer-events-none absolute left-2.5 top-1/2 -translate-y-1/2 text-faint" />
                  <input
                    type="text"
                    value={query}
                    onChange={(event) => setQuery(event.target.value)}
                    onKeyDown={(event) => event.stopPropagation()}
                    placeholder={t("ComparePicker.searchPlaceholder")}
                    autoFocus
                    className="h-8 w-full rounded-md border border-border bg-surface py-1.5 pl-8 pr-3 text-sm text-foreground placeholder:text-faint focus:outline-none focus:ring-2 focus:ring-accent"
                  />
                </div>
              </div>
            )}

            <div className="min-h-0 flex-1 overflow-y-auto py-1 [overscroll-behavior:contain]">
              {filteredLocalModels.length > 0 && (
                <>
                  <p className="px-3 pb-1 pt-2 text-[11px] font-semibold uppercase tracking-wider text-faint">{t("ModelSwitcher.localSectionLabel")}</p>
                  {filteredLocalModels.map((model) => {
                    const isActive = activeProvider === "local" && active?.path === model.path;
                    return (
                      <button
                        key={model.id}
                        type="button"
                        onClick={() => handleSelectLocal(model)}
                        className="flex w-full cursor-pointer items-center justify-between px-3 py-2 text-left text-sm hover:bg-surface-2"
                      >
                        <span className="truncate">{model.name}</span>
                        {isActive && <Check size={14} className="shrink-0 text-accent" />}
                      </button>
                    );
                  })}
                </>
              )}

              {providerGroups.map(({ provider, models }) => (
                <div key={provider.id}>
                  <p className="px-3 pb-1 pt-2 text-[11px] font-semibold uppercase tracking-wider text-faint">{provider.label}</p>
                  {models.map((model) => {
                    const isActive =
                      activeProvider === "provider" && activeProviderId === provider.id && activeProviderModel === model.id;
                    const retirement = cloudModelRetirementWarning(provider.id, model.id);
                    return (
                      <button
                        key={`${provider.id}/${model.id}`}
                        type="button"
                        onClick={() => handleSelectProvider(provider.id, model.id)}
                        className="flex w-full cursor-pointer items-center justify-between gap-1.5 px-3 py-2 text-left text-sm hover:bg-surface-2"
                      >
                        <span className="truncate">{model.id}</span>
                        <span className="flex shrink-0 items-center gap-1.5">
                          {retirement && (
                            <span title={retirementTooltip(t, retirement)}>
                              <TriangleAlert size={13} className="text-warning" aria-label={t("ModelSwitcher.retiredBadge")} />
                            </span>
                          )}
                          {isActive && <Check size={14} className="text-accent" />}
                        </span>
                      </button>
                    );
                  })}
                </div>
              ))}

              {filteredOllamaModels.length > 0 && (
                <>
                  <p className="px-3 pb-1 pt-2 text-[11px] font-semibold uppercase tracking-wider text-faint">{t("ModelSwitcher.ollamaSectionLabel")}</p>
                  {filteredOllamaModels.map((model) => {
                    const isActive = activeProvider === "ollama" && activeOllamaModel === model.name;
                    return (
                      <button
                        key={model.name}
                        type="button"
                        onClick={() => handleSelectOllama(model)}
                        className="flex w-full cursor-pointer items-center justify-between px-3 py-2 text-left text-sm hover:bg-surface-2"
                      >
                        <span className="flex min-w-0 items-center gap-1.5">
                          <span className="truncate">{model.name}</span>
                          {model.is_cloud && <span className="shrink-0 text-[10px] text-faint">{t("ModelSwitcher.cloudBadge")}</span>}
                        </span>
                        {isActive && <Check size={14} className="shrink-0 text-accent" />}
                      </button>
                    );
                  })}
                </>
              )}

              {!hasAnyVisibleModel && (
                <div className="px-4 py-6 text-center">
                  <p className="text-sm font-medium text-foreground">
                    {query.trim() ? t("ComparePicker.noResultsTitle") : t("ModelSwitcher.noModel")}
                  </p>
                  {!query.trim() && (
                    <p className="mt-1 text-xs text-faint">Add a cloud, local, or Ollama model without leaving chat.</p>
                  )}
                </div>
              )}
            </div>

            <div className="shrink-0 border-t border-border p-1.5">
              <button
                type="button"
                onClick={openAddModel}
                className="flex w-full items-center gap-2 rounded-md px-2.5 py-2 text-left text-sm font-medium text-foreground hover:bg-surface-2"
              >
                <Plus size={15} className="text-accent" />
                {t("OllamaPanel.addModelLabel")}
              </button>
            </div>
          </div>
        )}
      </div>

      {addModelOpen && (
        <Suspense
          fallback={
            <div className="fixed inset-0 z-[80] flex items-center justify-center bg-black/45 backdrop-blur-[2px]">
              <div className="h-5 w-5 animate-spin rounded-full border-2 border-border border-t-accent" />
            </div>
          }
        >
          <AddModelDialog open onClose={() => setAddModelOpen(false)} />
        </Suspense>
      )}
    </>
  );
}

export default ModelSwitcher;
