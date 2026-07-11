import { useMemo, useState } from "react";
import { Button, StatusPill } from "../ui";
import {
  MAX_CHECKPOINT_RETENTION,
  MIN_CHECKPOINT_RETENTION,
  useSettingsStore,
  type ContextTrimStrategy,
} from "../../store/settingsStore";
import { useModelStore } from "../../store/modelStore";
import { providerModelKey } from "../../lib/visionModels";
import { useT } from "../../lib/i18n";

/** No shared toggle-switch component exists in `ui/` yet — this one is small and specific enough to keep local rather than promote prematurely. */
function Toggle({
  checked,
  onChange,
  label,
  description,
}: {
  checked: boolean;
  onChange: (value: boolean) => void;
  label: string;
  description?: string;
}) {
  return (
    <label className="flex flex-col gap-0.5 py-2.5">
      <span className="flex items-center justify-between gap-3">
        <span className="text-sm font-medium text-foreground">{label}</span>
        <button
          type="button"
          role="switch"
          aria-checked={checked}
          aria-label={label}
          onClick={() => onChange(!checked)}
          className={`relative h-5 w-9 shrink-0 cursor-pointer rounded-full transition-colors ${
            checked ? "bg-accent" : "border border-border bg-surface-2"
          }`}
        >
          <span
            className={`absolute top-0.5 h-4 w-4 rounded-full bg-white shadow transition-[left] ${
              checked ? "left-[18px]" : "left-0.5"
            }`}
          />
        </button>
      </span>
      {description && <p className="pr-12 text-xs text-muted">{description}</p>}
    </label>
  );
}

const STRATEGY_OPTIONS: { value: ContextTrimStrategy; labelKey: string; descriptionKey: string }[] = [
  { value: "summarize", labelKey: "AutomationPanel.strategySummarizeLabel", descriptionKey: "AutomationPanel.strategySummarizeDescription" },
  { value: "trim", labelKey: "AutomationPanel.strategyTrimLabel", descriptionKey: "AutomationPanel.strategyTrimDescription" },
];

/**
 * Settings tab for the client-side reliability behaviors this app ported
 * from the idea of a server-side multi-provider gateway: auto-failover,
 * vision-aware model auto-switch, adaptive context compaction, and
 * rate-limit warnings against caps the user enters themselves (never
 * hardcoded — see `rateLimitTracker.ts`'s doc comment for why).
 */
export function AutomationPanel() {
  const { t } = useT();
  const autoFailoverEnabled = useSettingsStore((s) => s.autoFailoverEnabled);
  const setAutoFailoverEnabled = useSettingsStore((s) => s.setAutoFailoverEnabled);
  const autoVisionSwitchEnabled = useSettingsStore((s) => s.autoVisionSwitchEnabled);
  const setAutoVisionSwitchEnabled = useSettingsStore((s) => s.setAutoVisionSwitchEnabled);
  const contextTrimEnabled = useSettingsStore((s) => s.contextTrimEnabled);
  const setContextTrimEnabled = useSettingsStore((s) => s.setContextTrimEnabled);
  const contextTrimThreshold = useSettingsStore((s) => s.contextTrimThreshold);
  const setContextTrimThreshold = useSettingsStore((s) => s.setContextTrimThreshold);
  const contextTrimStrategy = useSettingsStore((s) => s.contextTrimStrategy);
  const setContextTrimStrategy = useSettingsStore((s) => s.setContextTrimStrategy);
  const checkpointRetention = useSettingsStore((s) => s.checkpointRetention);
  const setCheckpointRetention = useSettingsStore((s) => s.setCheckpointRetention);
  const rateLimitWarningsEnabled = useSettingsStore((s) => s.rateLimitWarningsEnabled);
  const setRateLimitWarningsEnabled = useSettingsStore((s) => s.setRateLimitWarningsEnabled);
  const providerRateLimits = useSettingsStore((s) => s.providerRateLimits);
  const setProviderRateLimit = useSettingsStore((s) => s.setProviderRateLimit);
  const visionOverrides = useSettingsStore((s) => s.visionOverrides);
  const setVisionOverride = useSettingsStore((s) => s.setVisionOverride);
  const clearVisionOverride = useSettingsStore((s) => s.clearVisionOverride);

  const providers = useModelStore((s) => s.providers);
  const providerModels = useModelStore((s) => s.providerModels);
  const connectedProviders = useMemo(() => providers.filter((p) => p.has_key), [providers]);

  const [overrideProviderId, setOverrideProviderId] = useState("");
  const [overrideModelId, setOverrideModelId] = useState("");
  const overrideModels = overrideProviderId ? providerModels[overrideProviderId] ?? [] : [];
  const overrideEntries = Object.entries(visionOverrides);

  return (
    <div className="flex flex-col gap-6 p-2">
      <section>
        <h3 className="mb-1 text-xs font-semibold uppercase tracking-wide text-faint">{t("AutomationPanel.reliabilityHeading")}</h3>
        <div className="divide-y divide-border rounded-lg border border-border bg-background px-3">
          <Toggle
            checked={autoFailoverEnabled}
            onChange={setAutoFailoverEnabled}
            label={t("AutomationPanel.autoFailoverLabel")}
            description={t("AutomationPanel.autoFailoverDescription")}
          />
          <Toggle
            checked={autoVisionSwitchEnabled}
            onChange={setAutoVisionSwitchEnabled}
            label={t("AutomationPanel.autoVisionSwitchLabel")}
            description={t("AutomationPanel.autoVisionSwitchDescription")}
          />
        </div>
      </section>

      <section>
        <h3 className="mb-1 text-xs font-semibold uppercase tracking-wide text-faint">{t("AutomationPanel.contextManagementHeading")}</h3>
        <div className="rounded-lg border border-border bg-background px-3">
          <Toggle
            checked={contextTrimEnabled}
            onChange={setContextTrimEnabled}
            label={t("AutomationPanel.autoCompactLabel")}
            description={t("AutomationPanel.autoCompactDescription")}
          />
          <div className={`flex flex-col gap-3 border-t border-border py-3 ${contextTrimEnabled ? "" : "opacity-50"}`}>
            <label className="flex items-center justify-between gap-3 text-sm">
              <span className="text-foreground">{t("AutomationPanel.triggerThresholdLabel")}</span>
              <span className="flex items-center gap-1.5">
                <input
                  type="number"
                  min={1}
                  max={100}
                  value={contextTrimThreshold}
                  disabled={!contextTrimEnabled}
                  onChange={(event) => setContextTrimThreshold(Number(event.target.value))}
                  className="h-8 w-16 rounded-md border border-border bg-surface px-2 text-right text-sm text-foreground focus:outline-none focus:ring-2 focus:ring-accent disabled:cursor-not-allowed"
                />
                <span className="text-muted">{t("AutomationPanel.percentOfContextWindow")}</span>
              </span>
            </label>
            <div className="flex flex-col gap-1.5">
              <span className="text-sm text-foreground">{t("AutomationPanel.strategyLabel")}</span>
              {STRATEGY_OPTIONS.map((option) => (
                <label key={option.value} className="flex items-start gap-2">
                  <input
                    type="radio"
                    name="context-trim-strategy"
                    checked={contextTrimStrategy === option.value}
                    disabled={!contextTrimEnabled}
                    onChange={() => setContextTrimStrategy(option.value)}
                    className="mt-1"
                  />
                  <span>
                    <span className="text-sm text-foreground">{t(option.labelKey)}</span>
                    <p className="text-xs text-muted">{t(option.descriptionKey)}</p>
                  </span>
                </label>
              ))}
            </div>
          </div>
        </div>
      </section>

      <section>
        <h3 className="mb-1 text-xs font-semibold uppercase tracking-wide text-faint">{t("AutomationPanel.checkpointsHeading")}</h3>
        <div className="rounded-lg border border-border bg-background px-3 py-2.5">
          <label className="flex items-center justify-between gap-3 text-sm">
            <span className="flex flex-col">
              <span className="text-foreground">{t("AutomationPanel.checkpointRetentionLabel")}</span>
              <span className="text-xs text-muted">{t("AutomationPanel.checkpointRetentionDescription")}</span>
            </span>
            <span className="flex shrink-0 items-center gap-1.5">
              <input
                type="number"
                min={MIN_CHECKPOINT_RETENTION}
                max={MAX_CHECKPOINT_RETENTION}
                value={checkpointRetention}
                onChange={(event) => setCheckpointRetention(Number(event.target.value))}
                className="h-8 w-16 rounded-md border border-border bg-surface px-2 text-right text-sm text-foreground focus:outline-none focus:ring-2 focus:ring-accent"
              />
              <span className="text-muted">{t("AutomationPanel.checkpointRetentionUnit")}</span>
            </span>
          </label>
        </div>
      </section>

      <section>
        <h3 className="mb-1 text-xs font-semibold uppercase tracking-wide text-faint">{t("AutomationPanel.rateLimitWarningsHeading")}</h3>
        <p className="mb-2 text-xs text-muted">
          {t("AutomationPanel.rateLimitWarningsIntro")}
        </p>
        <div className="divide-y divide-border rounded-lg border border-border bg-background px-3">
          <Toggle checked={rateLimitWarningsEnabled} onChange={setRateLimitWarningsEnabled} label={t("AutomationPanel.warnNearRateLimitsLabel")} />
          {connectedProviders.length === 0 ? (
            <p className="py-3 text-xs text-faint">{t("AutomationPanel.connectProviderEmptyState")}</p>
          ) : (
            connectedProviders.map((provider) => {
              const limit = providerRateLimits[provider.id] ?? {};
              return (
                <div key={provider.id} className="flex items-center justify-between gap-3 py-2">
                  <span className="truncate text-sm text-foreground">{provider.label}</span>
                  <div className="flex items-center gap-2 text-xs text-muted">
                    <input
                      type="number"
                      min={0}
                      placeholder={t("AutomationPanel.reqPerMinPlaceholder")}
                      value={limit.rpm ?? ""}
                      disabled={!rateLimitWarningsEnabled}
                      onChange={(event) =>
                        setProviderRateLimit(provider.id, { ...limit, rpm: event.target.value ? Number(event.target.value) : undefined })
                      }
                      className="h-8 w-20 rounded-md border border-border bg-surface px-2 text-foreground focus:outline-none focus:ring-2 focus:ring-accent disabled:cursor-not-allowed"
                    />
                    <input
                      type="number"
                      min={0}
                      placeholder={t("AutomationPanel.reqPerDayPlaceholder")}
                      value={limit.rpd ?? ""}
                      disabled={!rateLimitWarningsEnabled}
                      onChange={(event) =>
                        setProviderRateLimit(provider.id, { ...limit, rpd: event.target.value ? Number(event.target.value) : undefined })
                      }
                      className="h-8 w-20 rounded-md border border-border bg-surface px-2 text-foreground focus:outline-none focus:ring-2 focus:ring-accent disabled:cursor-not-allowed"
                    />
                  </div>
                </div>
              );
            })
          )}
        </div>
      </section>

      <section>
        <h3 className="mb-1 text-xs font-semibold uppercase tracking-wide text-faint">{t("AutomationPanel.visionOverridesHeading")}</h3>
        <p className="mb-2 text-xs text-muted">
          {t("AutomationPanel.visionOverridesIntro")}
        </p>
        <div className="rounded-lg border border-border bg-background p-3">
          <div className="flex flex-wrap items-center gap-2">
            <select
              value={overrideProviderId}
              onChange={(event) => {
                setOverrideProviderId(event.target.value);
                setOverrideModelId("");
              }}
              className="h-8 rounded-md border border-border bg-surface px-2 text-sm text-foreground focus:outline-none focus:ring-2 focus:ring-accent"
            >
              <option value="">{t("AutomationPanel.providerPlaceholderOption")}</option>
              {connectedProviders.map((provider) => (
                <option key={provider.id} value={provider.id}>
                  {provider.label}
                </option>
              ))}
            </select>
            <select
              value={overrideModelId}
              onChange={(event) => setOverrideModelId(event.target.value)}
              disabled={!overrideProviderId}
              className="h-8 min-w-0 flex-1 rounded-md border border-border bg-surface px-2 text-sm text-foreground focus:outline-none focus:ring-2 focus:ring-accent disabled:cursor-not-allowed"
            >
              <option value="">{t("AutomationPanel.modelPlaceholderOption")}</option>
              {overrideModels.map((model) => (
                <option key={model.id} value={model.id}>
                  {model.id}
                </option>
              ))}
            </select>
            <Button
              size="sm"
              variant="secondary"
              disabled={!overrideProviderId || !overrideModelId}
              onClick={() => setVisionOverride(providerModelKey(overrideProviderId, overrideModelId), true)}
            >
              {t("AutomationPanel.markVisionButton")}
            </Button>
            <Button
              size="sm"
              variant="secondary"
              disabled={!overrideProviderId || !overrideModelId}
              onClick={() => setVisionOverride(providerModelKey(overrideProviderId, overrideModelId), false)}
            >
              {t("AutomationPanel.markTextOnlyButton")}
            </Button>
          </div>

          {overrideEntries.length > 0 && (
            <div className="mt-3 flex flex-col gap-1.5 border-t border-border pt-3">
              {overrideEntries.map(([key, value]) => (
                <div key={key} className="flex items-center justify-between gap-2 text-xs">
                  <span className="truncate font-mono text-muted">{key}</span>
                  <span className="flex shrink-0 items-center gap-2">
                    <StatusPill tone={value ? "success" : "neutral"}>{value ? t("AutomationPanel.visionBadge") : t("AutomationPanel.textOnlyBadge")}</StatusPill>
                    <button type="button" onClick={() => clearVisionOverride(key)} className="cursor-pointer text-faint hover:text-danger">
                      {t("AutomationPanel.clearButton")}
                    </button>
                  </span>
                </div>
              ))}
            </div>
          )}
        </div>
      </section>
    </div>
  );
}

export default AutomationPanel;
