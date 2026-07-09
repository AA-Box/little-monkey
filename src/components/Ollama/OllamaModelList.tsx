import { formatBytes } from "../../lib/modelRegistry";
import { ModelListRow, StatusPill } from "../ui";
import { useModelStore } from "../../store/modelStore";
import { useT } from "../../lib/i18n";

/**
 * Lists locally-pulled Ollama models (including cloud tags, which are
 * ordinary tags once pulled), styled to match `ModelCard`'s visual language.
 * Selecting one is instant/local (`useOllamaModel`) — no backend round trip.
 */
export function OllamaModelList() {
  const ollamaModels = useModelStore((s) => s.ollamaModels);
  const activeProvider = useModelStore((s) => s.activeProvider);
  const activeOllamaModel = useModelStore((s) => s.activeOllamaModel);
  const useOllamaModel = useModelStore((s) => s.useOllamaModel);
  const removeOllamaModel = useModelStore((s) => s.removeOllamaModel);
  const { t } = useT();

  if (ollamaModels.length === 0) {
    return <p className="px-1 text-sm text-faint">{t("OllamaModelList.emptyState")}</p>;
  }

  function handleRemove(name: string) {
    if (!window.confirm(t("OllamaModelList.confirmRemove", { name }))) return;
    removeOllamaModel(name).catch((err) => {
      window.alert(t("OllamaModelList.removeFailed", { error: String(err) }));
    });
  }

  return (
    <div className="flex flex-col gap-2">
      {ollamaModels.map((model) => (
        <ModelListRow
          key={model.name}
          title={model.name}
          subtitle={formatBytes(model.size_bytes)}
          badge={model.is_cloud && <StatusPill tone="neutral">{t("OllamaModelList.cloudBadge")}</StatusPill>}
          isActive={activeProvider === "ollama" && activeOllamaModel === model.name}
          onUse={() => useOllamaModel(model.name)}
          onRemove={() => handleRemove(model.name)}
        />
      ))}
    </div>
  );
}
