import { useEffect, useRef, useState } from "react";
import { Check, ChevronDown, TriangleAlert } from "lucide-react";

import { useModelStore, type CloudModelRetirementWarning } from "../../store/modelStore";
import type { ModelInfo, OllamaModelInfo } from "../../store/modelStore";
import { useSettingsStore, DEFAULT_PROVIDER_MODEL_FILTER } from "../../store/settingsStore";
import { cloudModelRetirementWarning } from "../../lib/modelRetirement";
import { useT } from "../../lib/i18n";
import { visibleProviderModelsForProvider } from "../../lib/providerModelSelection";

/** Connected provider + loaded inventory but no curated rows is a selection
 * state, not a missing API-key/configuration state. */
export function providerModelsEmptyStateKey(
  availableModelCount: number,
): "ModelSwitcher.noCloudModelsSelected" | "ModelSwitcher.noCloudModelsConfigured" {
  return availableModelCount > 0
    ? "ModelSwitcher.noCloudModelsSelected"
    : "ModelSwitcher.noCloudModelsConfigured";
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
 * Small pill + dropdown for switching the active chat model between an
 * installed local (llama.cpp) model and a pulled Ollama tag. Rendered in
 * ChatWindow's bottom input row, mirroring ModeSelector's floating-panel
 * idiom (absolute dropdown, outside-pointerdown-to-close).
 *
 * `placement` defaults to "up" for that bottom-row use; panels that render it
 * near the TOP of a scroll container (e.g. `PmCopilotPanel`, whose generation
 * runs against this same active target) pass "down" so the dropdown isn't
 * clipped by the container's overflow.
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
  // Model Retirement and Compatibility Warnings (ROADMAP.md Phase 8, item
  // 14): not read directly below — subscribing is what makes this dropdown
  // re-render once the async retirement check resolves, since
  // `cloudModelRetirementWarning` otherwise reads a point-in-time snapshot.
  useModelStore((s) => s.providerModelRetirements);
  const activeProviderId = useModelStore((s) => s.activeProviderId);
  const activeProviderModel = useModelStore((s) => s.activeProviderModel);
  const useProviderModel = useModelStore((s) => s.useProviderModel);
  const providerModelFilters = useSettingsStore((s) => s.providerModelFilters);
  const { t } = useT();

  const connectedProviders = providers.filter((provider) => provider.has_key);
  // Embedding-only GGUFs can be installed for Knowledge Stacks, but they
  // cannot answer chat requests and must never appear as chat targets.
  const installedChatModels = installed.filter((model) => model.kind === "chat");

  const [open, setOpen] = useState(false);
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

  let label: string | null = null;
  if (activeProvider === "local" && active) {
    label = active.name;
  } else if (activeProvider === "ollama" && activeOllamaModel) {
    label = activeOllamaModel;
  } else if (activeProvider === "provider" && activeProviderModel) {
    label = activeProviderModel;
  }

  // Model Retirement and Compatibility Warnings (ROADMAP.md Phase 8, item
  // 14): surfaced right on the always-visible switcher pill, so a retired
  // active selection is visible before a run starts — not just while
  // picking a model from the dropdown.
  const activeRetirement =
    activeProvider === "provider" && activeProviderId && activeProviderModel
      ? cloudModelRetirementWarning(activeProviderId, activeProviderModel)
      : null;

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

  return (
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
          className={`absolute right-0 z-20 max-h-[70vh] w-64 overflow-y-auto rounded-lg border border-border bg-background py-1 shadow-lg ${
            placement === "down" ? "top-full mt-1" : "bottom-full mb-1"
          }`}
        >
          {installedChatModels.length > 0 && (
            <>
              <p className="px-3 pb-1 pt-2 text-[11px] font-semibold uppercase tracking-wider text-faint">{t("ModelSwitcher.localSectionLabel")}</p>
              {installedChatModels.map((model) => {
                const isActive = activeProvider === "local" && active?.path === model.path;
                return (
                  <button
                    key={model.id}
                    type="button"
                    onClick={() => handleSelectLocal(model)}
                    className="flex w-full cursor-pointer items-center justify-between px-3 py-1.5 text-left text-sm hover:bg-surface-2"
                  >
                    <span className="truncate">{model.name}</span>
                    {isActive && <Check size={14} className="shrink-0 text-accent" />}
                  </button>
                );
              })}
            </>
          )}

          {connectedProviders.map((provider) => {
            const filter = providerModelFilters[provider.id] ?? DEFAULT_PROVIDER_MODEL_FILTER;
            const availableModels = providerModels[provider.id] ?? [];
            const models = visibleProviderModelsForProvider(
              provider.id,
              availableModels,
              filter,
              { activeProvider, activeProviderId, activeProviderModel },
            );
            if (models.length === 0) return null;
            return (
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
                      className="flex w-full cursor-pointer items-center justify-between gap-1.5 px-3 py-1.5 text-left text-sm hover:bg-surface-2"
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
            );
          })}

          {ollamaModels.length > 0 && (
            <>
              <p className="px-3 pb-1 pt-2 text-[11px] font-semibold uppercase tracking-wider text-faint">{t("ModelSwitcher.ollamaSectionLabel")}</p>
              {ollamaModels.map((model) => {
                const isActive = activeProvider === "ollama" && activeOllamaModel === model.name;
                return (
                  <button
                    key={model.name}
                    type="button"
                    onClick={() => handleSelectOllama(model)}
                    className="flex w-full cursor-pointer items-center justify-between px-3 py-1.5 text-left text-sm hover:bg-surface-2"
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

          {installedChatModels.length === 0 &&
            connectedProviders.every((provider) => {
              const filter = providerModelFilters[provider.id] ?? DEFAULT_PROVIDER_MODEL_FILTER;
              const availableModels = providerModels[provider.id] ?? [];
              return (
                visibleProviderModelsForProvider(provider.id, availableModels, filter, {
                  activeProvider,
                  activeProviderId,
                  activeProviderModel,
                }).length === 0
              );
            }) &&
            ollamaModels.length === 0 && (
              <p className="px-3 py-1.5 text-xs text-faint">{t("ModelSwitcher.noModel")}</p>
            )}
        </div>
      )}
    </div>
  );
}

export default ModelSwitcher;
