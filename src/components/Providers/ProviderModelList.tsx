import { useEffect } from "react";
import { ModelListRow } from "../ui";
import { useModelStore } from "../../store/modelStore";
import { useT } from "../../lib/i18n";

/**
 * Sidebar list of every model available across configured cloud AI
 * providers (OpenAI/Anthropic/Gemini/OpenRouter/custom) that currently have
 * a saved key, flattened into one switchable list — mirrors
 * `OllamaModelList` exactly (same `ModelListRow`), just fed from
 * `providerModels` instead of a single Ollama tag list. Adding/removing
 * keys happens in the Settings modal; this is purely the "switch active
 * chat target" surface.
 */
export function ProviderModelList() {
  const providers = useModelStore((s) => s.providers);
  const providerModels = useModelStore((s) => s.providerModels);
  const activeProvider = useModelStore((s) => s.activeProvider);
  const activeProviderId = useModelStore((s) => s.activeProviderId);
  const activeProviderModel = useModelStore((s) => s.activeProviderModel);
  const useProviderModel = useModelStore((s) => s.useProviderModel);
  const refreshProviders = useModelStore((s) => s.refreshProviders);
  const { t } = useT();

  useEffect(() => {
    void refreshProviders();
  }, [refreshProviders]);

  const rows = providers
    .filter((provider) => provider.has_key)
    .flatMap((provider) => (providerModels[provider.id] ?? []).map((model) => ({ provider, model })));

  if (rows.length === 0) {
    return <p className="px-1 text-sm text-faint">{t("ProviderModelList.noCloudModelsEmptyState")}</p>;
  }

  return (
    <div className="flex flex-col gap-2">
      {rows.map(({ provider, model }) => (
        <ModelListRow
          key={`${provider.id}/${model.id}`}
          title={model.id}
          subtitle={provider.label}
          isActive={
            activeProvider === "provider" &&
            activeProviderId === provider.id &&
            activeProviderModel === model.id
          }
          onUse={() => useProviderModel(provider.id, model.id)}
        />
      ))}
    </div>
  );
}
