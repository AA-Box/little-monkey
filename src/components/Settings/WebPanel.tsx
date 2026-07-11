import { useEffect, useState } from "react";
import { Button, StatusPill } from "../ui";
import { useWebStore, type SearchProvider } from "../../store/webStore";
import { useSettingsStore } from "../../store/settingsStore";
import { useT } from "../../lib/i18n";

/** No shared toggle-switch component exists in `ui/` yet — cloned from
 * `AutomationPanel.tsx`'s local `Toggle` (the description-supporting
 * variant) rather than promoted prematurely. */
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

const PROVIDER_OPTIONS: { value: SearchProvider; labelKey: string; descriptionKey: string }[] = [
  { value: "duckduckgo", labelKey: "WebPanel.providerDuckduckgoLabel", descriptionKey: "WebPanel.providerDuckduckgoDescription" },
  { value: "brave", labelKey: "WebPanel.providerBraveLabel", descriptionKey: "WebPanel.providerBraveDescription" },
  { value: "searxng", labelKey: "WebPanel.providerSearxngLabel", descriptionKey: "WebPanel.providerSearxngDescription" },
];

/**
 * Settings "Web" tab: the master `webToolsEnabled` toggle (mirrors
 * `memoryEnabled`'s "disabled = not offered to the model" treatment — the
 * permission prompt shown on every call is the real per-call gate either
 * way), a `search_provider` picker with each provider's own connection
 * fields (Brave key via the OS keychain, SearXNG base URL), and the
 * `allow_local_network` escape hatch with an explicit warning — this toggle
 * re-opens the exact loopback targets (llama-server, Ollama) the SSRF guard
 * in `web.rs` exists to close off.
 */
export function WebPanel() {
  const { t } = useT();
  const webToolsEnabled = useSettingsStore((s) => s.webToolsEnabled);
  const setWebToolsEnabled = useSettingsStore((s) => s.setWebToolsEnabled);

  const settings = useWebStore((s) => s.settings);
  const hasBraveKey = useWebStore((s) => s.hasBraveKey);
  const loaded = useWebStore((s) => s.loaded);
  const refresh = useWebStore((s) => s.refresh);
  const setSettings = useWebStore((s) => s.setSettings);
  const setBraveKey = useWebStore((s) => s.setBraveKey);
  const removeBraveKey = useWebStore((s) => s.removeBraveKey);

  useEffect(() => {
    void refresh();
  }, [refresh]);

  const [settingsError, setSettingsError] = useState<string | null>(null);

  const [braveKeyInput, setBraveKeyInput] = useState("");
  const [savingBraveKey, setSavingBraveKey] = useState(false);
  const [removingBraveKey, setRemovingBraveKey] = useState(false);
  const [braveKeyError, setBraveKeyError] = useState<string | null>(null);

  const [searxngUrlInput, setSearxngUrlInput] = useState(settings.searxng_base_url ?? "");
  const [savingSearxngUrl, setSavingSearxngUrl] = useState(false);

  // Keep the (locally-edited) SearXNG input in sync whenever the backend's
  // value actually changes — e.g. after `refresh()` resolves post-mount, or
  // after a save round-trips through the backend's own normalization
  // (trailing-slash stripping, blank -> null).
  useEffect(() => {
    setSearxngUrlInput(settings.searxng_base_url ?? "");
  }, [settings.searxng_base_url]);

  async function handleProviderChange(provider: SearchProvider) {
    setSettingsError(null);
    try {
      await setSettings({ ...settings, search_provider: provider });
    } catch (err) {
      setSettingsError(err instanceof Error ? err.message : String(err));
    }
  }

  async function handleSaveSearxngUrl() {
    setSavingSearxngUrl(true);
    setSettingsError(null);
    try {
      await setSettings({ ...settings, searxng_base_url: searxngUrlInput.trim() || null });
    } catch (err) {
      setSettingsError(err instanceof Error ? err.message : String(err));
    } finally {
      setSavingSearxngUrl(false);
    }
  }

  async function handleAllowLocalNetworkChange(value: boolean) {
    setSettingsError(null);
    try {
      await setSettings({ ...settings, allow_local_network: value });
    } catch (err) {
      setSettingsError(err instanceof Error ? err.message : String(err));
    }
  }

  async function handleSaveBraveKey() {
    if (!braveKeyInput.trim()) return;
    setSavingBraveKey(true);
    setBraveKeyError(null);
    try {
      await setBraveKey(braveKeyInput.trim());
      setBraveKeyInput("");
    } catch (err) {
      setBraveKeyError(err instanceof Error ? err.message : String(err));
    } finally {
      setSavingBraveKey(false);
    }
  }

  async function handleRemoveBraveKey() {
    setRemovingBraveKey(true);
    setBraveKeyError(null);
    try {
      await removeBraveKey();
    } catch (err) {
      setBraveKeyError(err instanceof Error ? err.message : String(err));
    } finally {
      setRemovingBraveKey(false);
    }
  }

  return (
    <div className="flex flex-col gap-4 p-2">
      <p className="text-xs text-muted">{t("WebPanel.description")}</p>

      <div className="rounded-lg border border-border bg-background px-3">
        <Toggle
          checked={webToolsEnabled}
          onChange={setWebToolsEnabled}
          label={t("WebPanel.enableToggleLabel")}
          description={t("WebPanel.enableToggleDescription")}
        />
      </div>

      <section className={webToolsEnabled ? "" : "pointer-events-none opacity-50"}>
        <h3 className="mb-1 text-xs font-semibold uppercase tracking-wide text-faint">{t("WebPanel.providerHeading")}</h3>
        <div className="flex flex-col gap-2.5 rounded-lg border border-border bg-background p-3">
          <div className="flex flex-col gap-2">
            {PROVIDER_OPTIONS.map((option) => (
              <label key={option.value} className="flex items-start gap-2">
                <input
                  type="radio"
                  name="web-search-provider"
                  checked={settings.search_provider === option.value}
                  onChange={() => void handleProviderChange(option.value)}
                  className="mt-1"
                />
                <span>
                  <span className="text-sm text-foreground">{t(option.labelKey)}</span>
                  <p className="text-xs text-muted">{t(option.descriptionKey)}</p>
                </span>
              </label>
            ))}
          </div>

          {settingsError && <p className="text-xs text-danger">{settingsError}</p>}

          <div className="flex flex-col gap-1.5 border-t border-border pt-2.5">
            <span className="text-sm text-foreground">{t("WebPanel.braveKeyLabel")}</span>
            {loaded && hasBraveKey ? (
              <div className="flex flex-wrap items-center gap-2">
                <StatusPill tone="success">{t("WebPanel.braveKeySaved")}</StatusPill>
                <Button
                  variant="ghost"
                  size="sm"
                  onClick={() => void handleRemoveBraveKey()}
                  disabled={removingBraveKey}
                  className="text-danger hover:bg-danger-soft"
                >
                  {removingBraveKey ? t("WebPanel.braveKeyRemovingButton") : t("WebPanel.braveKeyRemoveButton")}
                </Button>
              </div>
            ) : (
              <div className="flex items-center gap-2">
                <input
                  type="password"
                  value={braveKeyInput}
                  onChange={(event) => setBraveKeyInput(event.target.value)}
                  placeholder={t("WebPanel.braveKeyPlaceholder")}
                  autoComplete="off"
                  className="h-8 min-w-0 flex-1 rounded-md border border-border bg-surface px-2.5 font-mono text-sm text-foreground placeholder:font-sans placeholder:text-faint focus:outline-none focus:ring-2 focus:ring-accent"
                />
                <Button
                  variant="primary"
                  size="sm"
                  onClick={() => void handleSaveBraveKey()}
                  disabled={!braveKeyInput.trim() || savingBraveKey}
                >
                  {savingBraveKey ? t("WebPanel.braveKeySavingButton") : t("WebPanel.braveKeySaveButton")}
                </Button>
              </div>
            )}
            {braveKeyError && <p className="text-xs text-danger">{braveKeyError}</p>}
          </div>

          <div className="flex flex-col gap-1.5 border-t border-border pt-2.5">
            <span className="text-sm text-foreground">{t("WebPanel.searxngUrlLabel")}</span>
            <div className="flex items-center gap-2">
              <input
                type="text"
                value={searxngUrlInput}
                onChange={(event) => setSearxngUrlInput(event.target.value)}
                placeholder={t("WebPanel.searxngUrlPlaceholder")}
                className="h-8 min-w-0 flex-1 rounded-md border border-border bg-surface px-2.5 font-mono text-sm text-foreground placeholder:font-sans placeholder:text-faint focus:outline-none focus:ring-2 focus:ring-accent"
              />
              <Button variant="secondary" size="sm" onClick={() => void handleSaveSearxngUrl()} disabled={savingSearxngUrl}>
                {savingSearxngUrl ? t("WebPanel.searxngUrlSavingButton") : t("WebPanel.searxngUrlSaveButton")}
              </Button>
            </div>
            <p className="text-xs text-faint">{t("WebPanel.searxngFormatsHint")}</p>
          </div>
        </div>
      </section>

      <section className={webToolsEnabled ? "" : "pointer-events-none opacity-50"}>
        <h3 className="mb-1 text-xs font-semibold uppercase tracking-wide text-faint">{t("WebPanel.advancedHeading")}</h3>
        <div className="rounded-lg border border-border bg-background px-3">
          <Toggle
            checked={settings.allow_local_network}
            onChange={(value) => void handleAllowLocalNetworkChange(value)}
            label={t("WebPanel.allowLocalNetworkLabel")}
            description={t("WebPanel.allowLocalNetworkDescription")}
          />
        </div>
        <p className="mt-1.5 rounded-md bg-warning-soft px-2 py-1.5 text-xs text-warning">
          {t("WebPanel.allowLocalNetworkWarning")}
        </p>
      </section>
    </div>
  );
}

export default WebPanel;
