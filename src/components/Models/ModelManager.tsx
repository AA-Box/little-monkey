import { useCallback, useEffect, useMemo } from "react";
import { ChevronDown, Plus } from "lucide-react";
import { CURATED_MODELS, type ModelInfo } from "../../lib/modelRegistry";
import { ModelCard } from "./ModelCard";
import { AddCustomModelForm } from "./AddCustomModelForm";
import { useModelStore } from "../../store/modelStore";
import { useT } from "../../lib/i18n";

const CURATED_IDS = new Set(CURATED_MODELS.map((model) => model.id));

/** Overlay backend-reported install state (from `installed`) onto the curated list. */
function mergeInstalled(curated: ModelInfo[], installed: ModelInfo[]): ModelInfo[] {
  if (installed.length === 0) return curated;
  const byId = new Map(installed.map((model) => [model.id, model]));
  const byFile = new Map(installed.map((model) => [model.file, model]));
  return curated.map((model) => {
    const match = byId.get(model.id) ?? byFile.get(model.file);
    return match ? { ...model, installed: true, path: match.path } : model;
  });
}

/** Active model first, then installed-but-idle, then not-yet-downloaded. */
function sortModels(models: ModelInfo[], activeId: string | undefined): ModelInfo[] {
  const rank = (model: ModelInfo) => (model.id === activeId ? 0 : model.installed ? 1 : 2);
  return [...models].sort((a, b) => rank(a) - rank(b));
}

/**
 * Top-level Models tab/panel: lists the curated catalog (decorated with
 * install state from the backend), any custom local models (pulled from a
 * non-curated Hugging Face repo, or opened from disk via `AddCustomModelForm`),
 * live download progress bars, and install / delete / start / stop actions
 * plus an active-model indicator tied to the running `llama-server` process.
 */
export function ModelManager() {
  const curated = useModelStore((s) => s.curated);
  const installed = useModelStore((s) => s.installed);
  const active = useModelStore((s) => s.active);
  const downloadProgress = useModelStore((s) => s.downloadProgress);
  const llamaStatus = useModelStore((s) => s.llamaStatus);
  const refresh = useModelStore((s) => s.refresh);
  const download = useModelStore((s) => s.download);
  const start = useModelStore((s) => s.start);
  const stop = useModelStore((s) => s.stop);
  const removeModel = useModelStore((s) => s.removeModel);
  const { t } = useT();

  useEffect(() => {
    void refresh();
  }, [refresh]);

  const models = useMemo(() => {
    const base = curated.length > 0 ? curated : CURATED_MODELS;
    return sortModels(mergeInstalled(base, installed), active?.id);
  }, [curated, installed, active?.id]);

  const customModels = useMemo(() => {
    const custom = installed.filter((model) => !CURATED_IDS.has(model.id));
    return sortModels(custom, active?.id);
  }, [installed, active?.id]);

  const handleDelete = useCallback(
    async (model: ModelInfo) => {
      if (!model.path) return;
      const isRunningActive = model.id === active?.id && llamaStatus !== "stopped";
      if (isRunningActive) {
        window.alert(t("ModelManager.stopModelBeforeDelete"));
        return;
      }
      const verb = model.is_external ? t("ModelManager.removeVerb") : t("ModelManager.deleteVerb");
      const detail = model.is_external
        ? t("ModelManager.confirmRemoveDetail")
        : t("ModelManager.confirmDeleteDetail");
      const confirmed = window.confirm(
        t("ModelManager.confirmDeleteModel", { verb, name: model.name, detail }),
      );
      if (!confirmed) return;
      try {
        await removeModel(model);
      } catch (err) {
        window.alert(t("ModelManager.deleteFailed", { verb: verb.toLowerCase(), error: String(err) }));
      }
    },
    [active?.id, llamaStatus, removeModel],
  );

  return (
    <div className="flex flex-col gap-2 p-2">
      {models.length === 0 ? (
        <p className="p-3 text-center text-sm text-faint">{t("ModelManager.noModelsAvailable")}</p>
      ) : (
        models.map((model) => (
          <ModelCard
            key={model.id}
            model={model}
            isActive={active?.id === model.id}
            llamaStatus={llamaStatus}
            downloadProgress={downloadProgress[model.file]}
            onInstall={() => void download(model)}
            onDelete={() => void handleDelete(model)}
            onStart={() => void start(model)}
            onStop={() => void stop()}
          />
        ))
      )}

      {customModels.length > 0 && (
        <>
          <p className="mt-2 px-1 text-[11px] font-semibold uppercase tracking-wider text-faint">
            {t("ModelManager.customLocalModels")}
          </p>
          {customModels.map((model) => (
            <ModelCard
              key={model.id}
              model={model}
              isActive={active?.id === model.id}
              llamaStatus={llamaStatus}
              downloadProgress={downloadProgress[model.file]}
              onInstall={() => void download(model)}
              onDelete={() => void handleDelete(model)}
              onStart={() => void start(model)}
              onStop={() => void stop()}
            />
          ))}
        </>
      )}

      <details className="group mt-2 rounded-lg border border-border">
        <summary className="flex cursor-pointer list-none items-center gap-1.5 px-3 py-2 text-sm text-muted [&::-webkit-details-marker]:hidden">
          <Plus size={14} />
          {t("ModelManager.addCustomModel")}
          <ChevronDown size={14} className="ml-auto transition-transform group-open:rotate-180" />
        </summary>
        <div className="border-t border-border p-2">
          <AddCustomModelForm />
        </div>
      </details>
    </div>
  );
}
