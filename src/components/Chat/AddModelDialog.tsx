import { useEffect, useMemo, useRef, useState } from "react";
import { createPortal } from "react-dom";
import { Cloud, Cpu, Search, Server, X } from "lucide-react";

import { useModelStore, type ProviderConfig } from "../../store/modelStore";
import { useSettingsStore, DEFAULT_PROVIDER_MODEL_FILTER } from "../../store/settingsStore";
import { errorMessage } from "../../lib/errors";
import { useT } from "../../lib/i18n";
import { ModelManager } from "../Models";
import { OllamaPanel } from "../Ollama";
import { Button, IconButton, StatusPill } from "../ui";

type SourceTab = "cloud" | "local" | "ollama";

interface AddModelDialogProps {
  open: boolean;
  onClose: () => void;
}

const SOURCE_TABS: Array<{ id: SourceTab; icon: typeof Cloud; labelKey: string }> = [
  { id: "cloud", icon: Cloud, labelKey: "ModelSwitcher.cloudSectionLabel" },
  { id: "local", icon: Cpu, labelKey: "ModelSwitcher.localSectionLabel" },
  { id: "ollama", icon: Server, labelKey: "ModelSwitcher.ollamaSectionLabel" },
];

function providerReady(provider: ProviderConfig): boolean {
  return provider.has_key || provider.is_extension;
}

function providerSort(a: ProviderConfig, b: ProviderConfig): number {
  const aReady = providerReady(a);
  const bReady = providerReady(b);
  if (aReady !== bReady) return aReady ? -1 : 1;
  if (a.is_custom !== b.is_custom) return a.is_custom ? 1 : -1;
  return a.label.localeCompare(b.label);
}

function targetKey(
  activeProvider: "local" | "ollama" | "provider",
  localPath: string | null | undefined,
  ollamaModel: string | null,
  providerId: string | null,
  providerModel: string | null,
): string {
  if (activeProvider === "local") return `local:${localPath ?? ""}`;
  if (activeProvider === "ollama") return `ollama:${ollamaModel ?? ""}`;
  return `provider:${providerId ?? ""}:${providerModel ?? ""}`;
}

/**
 * Point-of-use model setup. This intentionally owns no provider transport or
 * credential logic: it drives the same modelStore actions Settings already
 * uses, so keys still land in the OS keychain and model discovery still goes
 * through the Rust provider proxy. Local and Ollama setup reuse their existing
 * production panels instead of creating a second install/runtime path.
 */
export function AddModelDialog({ open, onClose }: AddModelDialogProps) {
  const providers = useModelStore((s) => s.providers);
  const providerModels = useModelStore((s) => s.providerModels);
  const providerKeyError = useModelStore((s) => s.providerKeyError);
  const refreshProviders = useModelStore((s) => s.refreshProviders);
  const refreshProviderModels = useModelStore((s) => s.refreshProviderModels);
  const setProviderKey = useModelStore((s) => s.setProviderKey);
  const addCustomProvider = useModelStore((s) => s.addCustomProvider);
  const useProviderModel = useModelStore((s) => s.useProviderModel);
  const activeProvider = useModelStore((s) => s.activeProvider);
  const activeLocalModel = useModelStore((s) => s.active);
  const activeOllamaModel = useModelStore((s) => s.activeOllamaModel);
  const activeProviderId = useModelStore((s) => s.activeProviderId);
  const activeProviderModel = useModelStore((s) => s.activeProviderModel);
  const providerModelFilters = useSettingsStore((s) => s.providerModelFilters);
  const setProviderModelSelection = useSettingsStore((s) => s.setProviderModelSelection);
  const { t } = useT();

  const [source, setSource] = useState<SourceTab>("cloud");
  const [selectedProviderId, setSelectedProviderId] = useState<string | null>(null);
  const [apiKey, setApiKey] = useState("");
  const [modelSearch, setModelSearch] = useState("");
  const [connecting, setConnecting] = useState(false);
  const [refreshing, setRefreshing] = useState(false);
  const [customLabel, setCustomLabel] = useState("");
  const [customBaseUrl, setCustomBaseUrl] = useState("");
  const [customApiKey, setCustomApiKey] = useState("");
  const [customBusy, setCustomBusy] = useState(false);
  const [customError, setCustomError] = useState<string | null>(null);
  const wasOpenRef = useRef(false);
  const activationBaselineRef = useRef("");

  const currentTargetKey = targetKey(
    activeProvider,
    activeLocalModel?.path,
    activeOllamaModel,
    activeProviderId,
    activeProviderModel,
  );

  useEffect(() => {
    if (open && !wasOpenRef.current) {
      activationBaselineRef.current = currentTargetKey;
    }
    wasOpenRef.current = open;
  }, [open, currentTargetKey]);

  useEffect(() => {
    if (!open) return;
    void refreshProviders().catch(() => {
      // Provider-specific errors are already kept in modelStore; the setup
      // surface remains usable for local/Ollama even if provider refresh fails.
    });
  }, [open, refreshProviders]);

  useEffect(() => {
    if (!open) return;
    const handleKeyDown = (event: KeyboardEvent) => {
      if (event.key === "Escape") onClose();
    };
    window.addEventListener("keydown", handleKeyDown);
    return () => window.removeEventListener("keydown", handleKeyDown);
  }, [open, onClose]);

  // Reused local/Ollama panels own their activation actions. Close this dialog
  // once one of those actions changes the active target, matching the cloud
  // flow's select-and-return-to-chat behavior.
  useEffect(() => {
    if (!open || currentTargetKey === activationBaselineRef.current) return;
    if (source === "local" && activeProvider === "local" && activeLocalModel?.path) {
      onClose();
    }
    if (source === "ollama" && activeProvider === "ollama" && activeOllamaModel) {
      onClose();
    }
  }, [
    activeLocalModel?.path,
    activeOllamaModel,
    activeProvider,
    currentTargetKey,
    onClose,
    open,
    source,
  ]);

  const sortedProviders = useMemo(() => [...providers].sort(providerSort), [providers]);
  const selectedProvider = selectedProviderId
    ? providers.find((provider) => provider.id === selectedProviderId) ?? null
    : null;
  const selectedProviderReady = selectedProvider ? providerReady(selectedProvider) : false;
  const selectedModels = selectedProvider ? providerModels[selectedProvider.id] ?? [] : [];
  const filteredModels = useMemo(() => {
    const needle = modelSearch.trim().toLowerCase();
    if (!needle) return selectedModels;
    return selectedModels.filter((model) => model.id.toLowerCase().includes(needle));
  }, [modelSearch, selectedModels]);

  useEffect(() => {
    if (!open || selectedProviderId || sortedProviders.length === 0) return;
    const preferred =
      (activeProvider === "provider" && activeProviderId
        ? sortedProviders.find((provider) => provider.id === activeProviderId)
        : undefined) ??
      sortedProviders.find(providerReady) ??
      sortedProviders[0];
    setSelectedProviderId(preferred.id);
  }, [activeProvider, activeProviderId, open, selectedProviderId, sortedProviders]);

  useEffect(() => {
    if (!open || !selectedProvider || !selectedProviderReady || selectedModels.length > 0) return;
    void refreshProviderModels(selectedProvider.id).catch(() => {
      // Error text is stored in providerKeyError and rendered below.
    });
  }, [open, refreshProviderModels, selectedModels.length, selectedProvider, selectedProviderReady]);

  if (!open) return null;

  const selectProvider = (provider: ProviderConfig) => {
    setSelectedProviderId(provider.id);
    setApiKey("");
    setModelSearch("");
    setCustomError(null);
  };

  const connectSelectedProvider = async () => {
    if (!selectedProvider || selectedProvider.is_extension || !apiKey.trim() || connecting) return;
    setConnecting(true);
    try {
      await setProviderKey(selectedProvider.id, apiKey.trim());
      setApiKey("");
    } catch {
      // modelStore owns and exposes the provider-specific failure message.
    } finally {
      setConnecting(false);
    }
  };

  const refreshSelectedModels = async () => {
    if (!selectedProvider || refreshing) return;
    setRefreshing(true);
    try {
      await refreshProviderModels(selectedProvider.id);
    } catch {
      // modelStore owns and exposes the provider-specific failure message.
    } finally {
      setRefreshing(false);
    }
  };

  const chooseProviderModel = (providerId: string, modelId: string) => {
    const available = providerModels[providerId] ?? [];
    const filter = providerModelFilters[providerId] ?? DEFAULT_PROVIDER_MODEL_FILTER;
    if (!filter.showAll && !filter.selectedModelIds.includes(modelId)) {
      setProviderModelSelection(
        providerId,
        [...filter.selectedModelIds, modelId],
        available.map((model) => model.id),
      );
    }
    useProviderModel(providerId, modelId);
    onClose();
  };

  const connectCustomProvider = async () => {
    if (!customLabel.trim() || !customBaseUrl.trim() || !customApiKey.trim() || customBusy) return;
    setCustomBusy(true);
    setCustomError(null);
    const before = new Set(useModelStore.getState().providers.map((provider) => provider.id));
    try {
      await addCustomProvider(customLabel.trim(), customBaseUrl.trim());
      const added = useModelStore
        .getState()
        .providers.find((provider) => provider.is_custom && !before.has(provider.id));
      if (!added) throw new Error("The provider was created, but its new configuration could not be resolved.");
      setSelectedProviderId(added.id);
      await useModelStore.getState().setProviderKey(added.id, customApiKey.trim());
      setCustomLabel("");
      setCustomBaseUrl("");
      setCustomApiKey("");
    } catch (error) {
      setCustomError(errorMessage(error));
    } finally {
      setCustomBusy(false);
    }
  };

  return createPortal(
    <div
      className="fixed inset-0 z-[80] flex items-center justify-center bg-black/45 p-4 backdrop-blur-[2px]"
      role="presentation"
      onMouseDown={(event) => {
        if (event.target === event.currentTarget) onClose();
      }}
    >
      <div
        role="dialog"
        aria-modal="true"
        aria-label={t("OllamaPanel.addModelLabel")}
        className="flex max-h-[86vh] w-full max-w-4xl flex-col overflow-hidden rounded-xl border border-border bg-background shadow-2xl"
      >
        <div className="flex shrink-0 items-center gap-3 border-b border-border px-5 py-4">
          <div className="min-w-0 flex-1">
            <h2 className="text-base font-semibold text-foreground">{t("OllamaPanel.addModelLabel")}</h2>
            <p className="mt-0.5 text-xs text-muted">Choose where the model runs, then connect or install it here.</p>
          </div>
          <IconButton size="sm" onClick={onClose} aria-label={t("Debate.close")}>
            <X size={16} />
          </IconButton>
        </div>

        <div className="flex shrink-0 gap-1 border-b border-border px-5 pt-3">
          {SOURCE_TABS.map((tab) => {
            const Icon = tab.icon;
            const active = source === tab.id;
            return (
              <button
                key={tab.id}
                type="button"
                onClick={() => setSource(tab.id)}
                className={`flex items-center gap-2 rounded-t-lg border-b-2 px-3 py-2 text-sm transition-colors ${
                  active
                    ? "border-accent font-medium text-foreground"
                    : "border-transparent text-muted hover:text-foreground"
                }`}
              >
                <Icon size={15} />
                {t(tab.labelKey)}
              </button>
            );
          })}
        </div>

        <div className="min-h-0 flex-1 overflow-y-auto p-5 [overscroll-behavior:contain]">
          {source === "local" && <ModelManager />}
          {source === "ollama" && <OllamaPanel />}
          {source === "cloud" && (
            <div className="grid gap-4 lg:grid-cols-[15rem_minmax(0,1fr)]">
              <div className="flex flex-col gap-1">
                {sortedProviders.map((provider) => (
                  <button
                    key={provider.id}
                    type="button"
                    onClick={() => selectProvider(provider)}
                    className={`flex items-center justify-between gap-2 rounded-lg border px-3 py-2.5 text-left text-sm ${
                      selectedProviderId === provider.id
                        ? "border-accent bg-surface-2 text-foreground"
                        : "border-border bg-background text-muted hover:bg-surface-2 hover:text-foreground"
                    }`}
                  >
                    <span className="min-w-0 truncate">{provider.label}</span>
                    {provider.is_extension ? (
                      <StatusPill tone="neutral">{t("ProviderCard.extension")}</StatusPill>
                    ) : provider.has_key ? (
                      <StatusPill tone="success">{t("ProviderCard.connected")}</StatusPill>
                    ) : null}
                  </button>
                ))}

                <div className="mt-3 rounded-lg border border-dashed border-border p-3">
                  <p className="mb-2 text-xs font-semibold uppercase tracking-wider text-faint">
                    {t("AddCustomProviderForm.heading")}
                  </p>
                  <div className="flex flex-col gap-2">
                    <input
                      value={customLabel}
                      onChange={(event) => setCustomLabel(event.target.value)}
                      placeholder={t("AddCustomProviderForm.labelPlaceholder")}
                      className="h-8 rounded-md border border-border bg-surface px-2.5 text-sm text-foreground placeholder:text-faint focus:outline-none focus:ring-2 focus:ring-accent"
                    />
                    <input
                      value={customBaseUrl}
                      onChange={(event) => setCustomBaseUrl(event.target.value)}
                      placeholder={t("AddCustomProviderForm.baseUrlPlaceholder")}
                      className="h-8 rounded-md border border-border bg-surface px-2.5 font-mono text-xs text-foreground placeholder:font-sans placeholder:text-faint focus:outline-none focus:ring-2 focus:ring-accent"
                    />
                    <input
                      type="password"
                      autoComplete="off"
                      value={customApiKey}
                      onChange={(event) => setCustomApiKey(event.target.value)}
                      placeholder={t("ProviderCard.apiKeyPlaceholder")}
                      className="h-8 rounded-md border border-border bg-surface px-2.5 font-mono text-sm text-foreground placeholder:font-sans placeholder:text-faint focus:outline-none focus:ring-2 focus:ring-accent"
                    />
                    <Button
                      variant="secondary"
                      size="sm"
                      onClick={() => void connectCustomProvider()}
                      disabled={!customLabel.trim() || !customBaseUrl.trim() || !customApiKey.trim() || customBusy}
                    >
                      {customBusy ? t("AddCustomProviderForm.addingButton") : t("ProviderCard.save")}
                    </Button>
                    {customError && <p className="text-xs text-danger">{customError}</p>}
                  </div>
                </div>
              </div>

              <div className="min-w-0 rounded-lg border border-border bg-background p-4">
                {!selectedProvider ? (
                  <div className="flex min-h-48 items-center justify-center text-center text-sm text-faint">
                    Choose a provider to connect it and pick a model.
                  </div>
                ) : !selectedProviderReady ? (
                  <div className="mx-auto flex max-w-lg flex-col gap-3 py-6">
                    <div>
                      <h3 className="text-sm font-semibold text-foreground">{selectedProvider.label}</h3>
                      <p className="mt-1 text-xs text-muted">{selectedProvider.base_url}</p>
                    </div>
                    <input
                      type="password"
                      autoComplete="off"
                      value={apiKey}
                      onChange={(event) => setApiKey(event.target.value)}
                      onKeyDown={(event) => {
                        if (event.key === "Enter") void connectSelectedProvider();
                      }}
                      placeholder={t("ProviderCard.apiKeyPlaceholder")}
                      autoFocus
                      className="h-9 rounded-md border border-border bg-surface px-3 font-mono text-sm text-foreground placeholder:font-sans placeholder:text-faint focus:outline-none focus:ring-2 focus:ring-accent"
                    />
                    <Button
                      variant="primary"
                      size="sm"
                      onClick={() => void connectSelectedProvider()}
                      disabled={!apiKey.trim() || connecting}
                    >
                      {connecting ? t("ProviderCard.saving") : t("ProviderCard.save")}
                    </Button>
                    {providerKeyError[selectedProvider.id] && (
                      <p className="text-xs text-danger">{providerKeyError[selectedProvider.id]}</p>
                    )}
                  </div>
                ) : (
                  <div className="flex min-h-0 flex-col gap-3">
                    <div className="flex items-center justify-between gap-3">
                      <div className="min-w-0">
                        <div className="flex items-center gap-2">
                          <h3 className="truncate text-sm font-semibold text-foreground">{selectedProvider.label}</h3>
                          {selectedProvider.is_extension ? (
                            <StatusPill tone="neutral">{t("ProviderCard.extension")}</StatusPill>
                          ) : (
                            <StatusPill tone="success">{t("ProviderCard.connected")}</StatusPill>
                          )}
                        </div>
                        <p className="mt-0.5 text-xs text-muted">Pick a model to use it in this chat.</p>
                      </div>
                      <Button variant="ghost" size="sm" onClick={() => void refreshSelectedModels()} disabled={refreshing}>
                        {refreshing ? t("ProviderCard.refreshing") : t("ProviderCard.refreshModels")}
                      </Button>
                    </div>

                    {selectedModels.length > 0 && (
                      <div className="relative">
                        <Search size={14} className="pointer-events-none absolute left-2.5 top-1/2 -translate-y-1/2 text-faint" />
                        <input
                          value={modelSearch}
                          onChange={(event) => setModelSearch(event.target.value)}
                          placeholder={t("ComparePicker.searchPlaceholder")}
                          className="h-9 w-full rounded-md border border-border bg-surface py-1.5 pl-8 pr-3 text-sm text-foreground placeholder:text-faint focus:outline-none focus:ring-2 focus:ring-accent"
                        />
                      </div>
                    )}

                    <div className="flex max-h-[48vh] flex-col gap-1 overflow-y-auto [overscroll-behavior:contain]">
                      {filteredModels.map((model) => (
                        <button
                          key={model.id}
                          type="button"
                          onClick={() => chooseProviderModel(selectedProvider.id, model.id)}
                          className="flex min-h-10 items-center justify-between gap-3 rounded-md px-3 py-2 text-left hover:bg-surface-2"
                        >
                          <span className="min-w-0 truncate font-mono text-xs text-foreground">{model.id}</span>
                          <span className="shrink-0 text-xs text-accent">Use</span>
                        </button>
                      ))}
                      {selectedModels.length === 0 && !providerKeyError[selectedProvider.id] && (
                        <p className="px-2 py-8 text-center text-xs text-faint">{t("OpenRouterModelsPanel.noModelsLoaded")}</p>
                      )}
                      {selectedModels.length > 0 && filteredModels.length === 0 && (
                        <p className="px-2 py-8 text-center text-xs text-faint">{t("ComparePicker.noResultsTitle")}</p>
                      )}
                    </div>
                    {providerKeyError[selectedProvider.id] && (
                      <p className="text-xs text-danger">{providerKeyError[selectedProvider.id]}</p>
                    )}
                  </div>
                )}
              </div>
            </div>
          )}
        </div>
      </div>
    </div>,
    document.body,
  );
}

export default AddModelDialog;
