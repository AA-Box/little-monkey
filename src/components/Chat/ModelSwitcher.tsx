import { useEffect, useRef, useState } from "react";
import { Check, ChevronDown } from "lucide-react";

import { useModelStore } from "../../store/modelStore";
import type { ModelInfo, OllamaModelInfo } from "../../store/modelStore";
import { useT } from "../../lib/i18n";

/**
 * Small pill + dropdown for switching the active chat model between an
 * installed local (llama.cpp) model and a pulled Ollama tag. Rendered in
 * ChatWindow's bottom input row, mirroring ModeSelector's floating-panel
 * idiom (absolute dropdown, outside-pointerdown-to-close).
 */
export function ModelSwitcher() {
  const installed = useModelStore((s) => s.installed);
  const ollamaModels = useModelStore((s) => s.ollamaModels);
  const active = useModelStore((s) => s.active);
  const activeProvider = useModelStore((s) => s.activeProvider);
  const activeOllamaModel = useModelStore((s) => s.activeOllamaModel);
  const start = useModelStore((s) => s.start);
  const useOllamaModel = useModelStore((s) => s.useOllamaModel);
  const { t } = useT();

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
  }

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

  return (
    <div ref={containerRef} className="relative inline-block">
      <button
        type="button"
        onClick={() => setOpen((prev) => !prev)}
        aria-haspopup="true"
        aria-expanded={open}
        className="flex cursor-pointer items-center gap-1 text-xs font-mono text-muted hover:text-foreground"
      >
        {label ? label : <span className="text-faint">{t("ModelSwitcher.noModel")}</span>}
        <ChevronDown size={12} className="shrink-0" />
      </button>

      {open && (
        <div className="absolute bottom-full right-0 z-20 mb-1 max-h-72 w-64 overflow-y-auto rounded-lg border border-border bg-background py-1 shadow-lg">
          <p className="px-3 pb-1 pt-2 text-[11px] font-semibold uppercase tracking-wider text-faint">{t("ModelSwitcher.localSectionLabel")}</p>
          {installed.length === 0 ? (
            <p className="px-3 py-1.5 text-xs text-faint">{t("ModelSwitcher.noLocalModelsInstalled")}</p>
          ) : (
            installed.map((model) => {
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
            })
          )}

          <p className="px-3 pb-1 pt-2 text-[11px] font-semibold uppercase tracking-wider text-faint">{t("ModelSwitcher.ollamaSectionLabel")}</p>
          {ollamaModels.length === 0 ? (
            <p className="px-3 py-1.5 text-xs text-faint">{t("ModelSwitcher.noOllamaModelsPulled")}</p>
          ) : (
            ollamaModels.map((model) => {
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
            })
          )}
        </div>
      )}
    </div>
  );
}

export default ModelSwitcher;
