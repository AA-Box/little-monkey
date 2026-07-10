import { useCallback, useMemo, useState } from "react";
import { Button, ModelListRow, StatusPill } from "../ui";
import { useModelStore, type ProviderConfig, type ProviderModelInfo } from "../../store/modelStore";
import { isVisionCapableProviderModel } from "../../lib/visionModels";
import { useSettingsStore } from "../../store/settingsStore";
import { useT } from "../../lib/i18n";

/** Model lists past this size get a filter input above them (OpenRouter alone returns 400+). */
const FILTER_THRESHOLD = 8;

/**
 * Stable fallback for "this provider has no cached model list yet". Must be
 * a module-level constant, not `[]` inlined in the selector below — a fresh
 * array literal on every call makes Zustand/`useSyncExternalStore` see a
 * "changed" snapshot on every render and spin into an infinite re-render
 * loop (confirmed live: it blanks the whole app).
 */
const EMPTY_MODELS: ProviderModelInfo[] = [];

interface ProviderCardProps {
  provider: ProviderConfig;
}

/**
 * One provider's connection card in the Settings modal: label + base URL,
 * an API key input (or a "saved" indicator + refresh/remove actions once
 * connected), and — once connected — a filterable list of its models that
 * can be switched to right from here (same `ModelListRow` the sidebar's
 * `ProviderModelList` uses, so switching works identically in both places).
 */
export function ProviderCard({ provider }: ProviderCardProps) {
  const providerModels = useModelStore((s) => s.providerModels[provider.id] ?? EMPTY_MODELS);
  const errorMessage = useModelStore((s) => s.providerKeyError[provider.id]);
  const activeProvider = useModelStore((s) => s.activeProvider);
  const activeProviderId = useModelStore((s) => s.activeProviderId);
  const activeProviderModel = useModelStore((s) => s.activeProviderModel);
  const setProviderKey = useModelStore((s) => s.setProviderKey);
  const removeProviderKey = useModelStore((s) => s.removeProviderKey);
  const refreshProviderModels = useModelStore((s) => s.refreshProviderModels);
  const removeCustomProvider = useModelStore((s) => s.removeCustomProvider);
  const useProviderModel = useModelStore((s) => s.useProviderModel);
  // Not read directly below — subscribing is what makes this card re-render
  // when a vision override changes in the Automation settings tab, since
  // `isVisionCapableProviderModel` otherwise reads a point-in-time snapshot.
  useSettingsStore((s) => s.visionOverrides);
  const { t } = useT();

  const [apiKey, setApiKey] = useState("");
  const [saving, setSaving] = useState(false);
  const [refreshing, setRefreshing] = useState(false);
  const [filter, setFilter] = useState("");

  const handleSave = useCallback(async () => {
    if (!apiKey.trim() || saving) return;
    setSaving(true);
    try {
      // Deliberately NOT cleared on success: the field stays filled while the
      // card flips to its connected state, so a save never reads as "my key
      // vanished". It is cleared in `handleRemoveKey` instead, so the secret
      // can't silently repopulate the input after a later key removal.
      await setProviderKey(provider.id, apiKey);
    } catch {
      // Failure message is captured in `providerKeyError[provider.id]` by the store.
    } finally {
      setSaving(false);
    }
  }, [apiKey, saving, provider.id, setProviderKey]);

  const handleRemoveKey = useCallback(async () => {
    if (!window.confirm(t("ProviderCard.confirmRemoveKey", { label: provider.label }))) return;
    await removeProviderKey(provider.id);
    setApiKey("");
  }, [provider.id, provider.label, removeProviderKey, t]);

  const handleRefresh = useCallback(async () => {
    setRefreshing(true);
    try {
      await refreshProviderModels(provider.id);
    } catch {
      // Failure message is captured in `providerKeyError[provider.id]` by the store.
    } finally {
      setRefreshing(false);
    }
  }, [provider.id, refreshProviderModels]);

  const handleRemoveProvider = useCallback(async () => {
    if (!window.confirm(t("ProviderCard.confirmRemoveProvider", { label: provider.label }))) return;
    await removeCustomProvider(provider.id);
  }, [provider.id, provider.label, removeCustomProvider, t]);

  const filteredModels = useMemo(() => {
    const needle = filter.trim().toLowerCase();
    if (!needle) return providerModels;
    return providerModels.filter((model) => model.id.toLowerCase().includes(needle));
  }, [providerModels, filter]);

  return (
    <div className="flex flex-col gap-3 rounded-lg border border-border bg-background p-3">
      <div className="flex items-start justify-between gap-3">
        <div className="min-w-0">
          <div className="flex flex-wrap items-center gap-2">
            <h3 className="truncate text-sm font-semibold text-foreground">{provider.label}</h3>
            {provider.has_key && <StatusPill tone="success">{t("ProviderCard.connected")}</StatusPill>}
            {provider.is_custom && <StatusPill tone="neutral">{t("ProviderCard.custom")}</StatusPill>}
          </div>
          <p className="mt-0.5 truncate font-mono text-xs text-muted">{provider.base_url}</p>
        </div>
        {provider.is_custom && (
          <Button
            variant="ghost"
            size="sm"
            onClick={() => void handleRemoveProvider()}
            className="shrink-0 text-danger hover:bg-danger-soft"
          >
            {t("ProviderCard.remove")}
          </Button>
        )}
      </div>

      {!provider.has_key ? (
        <div className="flex items-center gap-2">
          <input
            type="password"
            value={apiKey}
            onChange={(event) => setApiKey(event.target.value)}
            placeholder={t("ProviderCard.apiKeyPlaceholder")}
            autoComplete="off"
            className="h-8 min-w-0 flex-1 rounded-md border border-border bg-surface px-2.5 font-mono text-sm text-foreground placeholder:font-sans placeholder:text-faint focus:outline-none focus:ring-2 focus:ring-accent"
          />
          <Button variant="primary" size="sm" onClick={() => void handleSave()} disabled={!apiKey.trim() || saving}>
            {saving ? t("ProviderCard.saving") : t("ProviderCard.save")}
          </Button>
        </div>
      ) : (
        <div className="flex flex-wrap items-center gap-2">
          <span className="font-mono text-xs text-muted">{t("ProviderCard.keySaved")}</span>
          <Button variant="secondary" size="sm" onClick={() => void handleRefresh()} disabled={refreshing}>
            {refreshing
              ? t("ProviderCard.refreshing")
              : providerModels.length
                ? t("ProviderCard.refreshModelsWithCount", { count: providerModels.length })
                : t("ProviderCard.refreshModels")}
          </Button>
          <Button
            variant="ghost"
            size="sm"
            onClick={() => void handleRemoveKey()}
            className="text-danger hover:bg-danger-soft"
          >
            {t("ProviderCard.removeKey")}
          </Button>
        </div>
      )}

      {errorMessage && <p className="text-xs text-danger">{errorMessage}</p>}

      {provider.has_key && providerModels.length > 0 && (
        <div className="flex flex-col gap-2">
          {providerModels.length > FILTER_THRESHOLD && (
            <input
              type="text"
              value={filter}
              onChange={(event) => setFilter(event.target.value)}
              placeholder={t("ProviderCard.filterModelsPlaceholder", { count: providerModels.length })}
              className="h-8 rounded-md border border-border bg-surface px-2.5 font-mono text-xs text-foreground placeholder:font-sans placeholder:text-faint focus:outline-none focus:ring-2 focus:ring-accent"
            />
          )}
          <div className="flex max-h-48 flex-col gap-1.5 overflow-y-auto">
            {filteredModels.map((model) => (
              <ModelListRow
                key={model.id}
                title={model.id}
                badge={
                  isVisionCapableProviderModel(provider.id, model.id) && (
                    <StatusPill tone="neutral">{t("ProviderCard.vision")}</StatusPill>
                  )
                }
                isActive={
                  activeProvider === "provider" &&
                  activeProviderId === provider.id &&
                  activeProviderModel === model.id
                }
                onUse={() => useProviderModel(provider.id, model.id)}
              />
            ))}
            {filteredModels.length === 0 && (
              <p className="px-1 text-xs text-faint">{t("ProviderCard.noModelsMatch", { filter })}</p>
            )}
          </div>
        </div>
      )}
    </div>
  );
}
