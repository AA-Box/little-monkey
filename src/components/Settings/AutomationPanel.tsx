import { useEffect, useMemo, useState } from "react";
import { Plus, Trash2 } from "lucide-react";
import { Button, StatusPill } from "../ui";
import {
  MAX_CHECKPOINT_RETENTION,
  MAX_MAX_CONCURRENT_SUBAGENTS,
  MAX_VERIFY_MAX_ROUNDS,
  MIN_CHECKPOINT_RETENTION,
  MIN_MAX_CONCURRENT_SUBAGENTS,
  MIN_VERIFY_MAX_ROUNDS,
  useSettingsStore,
  type ContextTrimStrategy,
} from "../../store/settingsStore";
import { useModelStore } from "../../store/modelStore";
import { primaryRoot, useWorkspaceStore } from "../../store/workspaceStore";
import { useVerifyStore, type VerifyCommand, type VerifyCommandKind } from "../../store/verifyStore";
import { useWebStore, type SearchProvider } from "../../store/webStore";
import { useCliInstallStore } from "../../store/cliInstallStore";
import { providerModelKey } from "../../lib/visionModels";
import { useT } from "../../lib/i18n";
import { errorMessage } from "../../lib/errors";

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

const WEB_PROVIDER_OPTIONS: { value: SearchProvider; labelKey: string; descriptionKey: string }[] = [
  { value: "duckduckgo", labelKey: "WebPanel.providerDuckduckgoLabel", descriptionKey: "WebPanel.providerDuckduckgoDescription" },
  { value: "brave", labelKey: "WebPanel.providerBraveLabel", descriptionKey: "WebPanel.providerBraveDescription" },
  { value: "searxng", labelKey: "WebPanel.providerSearxngLabel", descriptionKey: "WebPanel.providerSearxngDescription" },
];

const STRATEGY_OPTIONS: { value: ContextTrimStrategy; labelKey: string; descriptionKey: string }[] = [
  { value: "summarize", labelKey: "AutomationPanel.strategySummarizeLabel", descriptionKey: "AutomationPanel.strategySummarizeDescription" },
  { value: "trim", labelKey: "AutomationPanel.strategyTrimLabel", descriptionKey: "AutomationPanel.strategyTrimDescription" },
];

/** Mirrors `DEFAULT_VERIFY_TIMEOUT_SECS` in `src-tauri/src/verify.rs` — shown
 * only as the timeout input's placeholder (an empty field means "use the
 * backend default", not literally 0). */
const DEFAULT_VERIFY_TIMEOUT_SECS = 300;

/** Parses the timeout input into `VerifyCommand.timeoutSecs` — mirrors
 * `AddMcpServerForm.tsx`'s `parseTimeoutSecs`: `undefined` (falls back to
 * `verify.rs`'s `DEFAULT_VERIFY_TIMEOUT_SECS`) for empty/invalid/non-positive
 * input, otherwise a rounded positive integer. Needed because `timeoutSecs`
 * round-trips through `verify_set_config` into a Rust `Option<u64>` — an
 * unvalidated negative or fractional value would fail Tauri's argument
 * deserialization before `verify_set_config` even runs, silently dropping the
 * whole config write (the `number` input's `min={1}` only affects `:invalid`
 * styling, it doesn't stop the user from typing "-5" or "1.5"). */
function parseVerifyTimeoutSecs(raw: string): number | undefined {
  const trimmed = raw.trim();
  if (trimmed.length === 0) return undefined;
  const parsed = Number(trimmed);
  return Number.isFinite(parsed) && parsed > 0 ? Math.round(parsed) : undefined;
}

const VERIFY_KIND_OPTIONS: { value: VerifyCommandKind; labelKey: string }[] = [
  { value: "lint", labelKey: "AutomationPanel.verifyKindLint" },
  { value: "test", labelKey: "AutomationPanel.verifyKindTest" },
  { value: "build", labelKey: "AutomationPanel.verifyKindBuild" },
  { value: "custom", labelKey: "AutomationPanel.verifyKindCustom" },
];

/**
 * One editable verification command: label / shell command / kind-select /
 * enabled toggle / delete. Every field change round-trips through
 * `verifyStore` (`verify_set_config` then a refresh) immediately — cheap for
 * a local app, and it means the on-disk config never drifts from what's
 * shown here even if the panel closes mid-edit.
 */
function VerifyCommandRow({ command }: { command: VerifyCommand }) {
  const { t } = useT();
  const updateCommand = useVerifyStore((s) => s.updateCommand);
  const removeCommand = useVerifyStore((s) => s.removeCommand);
  const toggleCommand = useVerifyStore((s) => s.toggleCommand);
  const [removing, setRemoving] = useState(false);

  return (
    <div className="flex flex-wrap items-center gap-2 border-t border-border py-2.5 first:border-t-0">
      <input
        type="text"
        value={command.label}
        placeholder={t("AutomationPanel.verifyLabelPlaceholder")}
        onChange={(event) => void updateCommand(command.id, { label: event.target.value })}
        className="h-8 w-28 min-w-0 shrink-0 rounded-md border border-border bg-surface px-2 text-sm text-foreground focus:outline-none focus:ring-2 focus:ring-accent"
      />
      <input
        type="text"
        value={command.command}
        placeholder={t("AutomationPanel.verifyCommandPlaceholder")}
        onChange={(event) => void updateCommand(command.id, { command: event.target.value })}
        className="h-8 min-w-0 flex-1 rounded-md border border-border bg-surface px-2 font-mono text-xs text-foreground focus:outline-none focus:ring-2 focus:ring-accent"
      />
      <select
        value={command.kind}
        onChange={(event) => void updateCommand(command.id, { kind: event.target.value as VerifyCommandKind })}
        className="h-8 shrink-0 rounded-md border border-border bg-surface px-2 text-sm text-foreground focus:outline-none focus:ring-2 focus:ring-accent"
      >
        {VERIFY_KIND_OPTIONS.map((option) => (
          <option key={option.value} value={option.value}>
            {t(option.labelKey)}
          </option>
        ))}
      </select>
      <input
        type="number"
        min={1}
        value={command.timeoutSecs ?? ""}
        placeholder={String(DEFAULT_VERIFY_TIMEOUT_SECS)}
        title={t("AutomationPanel.verifyTimeoutLabel")}
        aria-label={t("AutomationPanel.verifyTimeoutAriaLabel", { label: command.label || command.command })}
        onChange={(event) =>
          void updateCommand(command.id, { timeoutSecs: parseVerifyTimeoutSecs(event.target.value) }).catch(() => {})
        }
        className="h-8 w-16 shrink-0 rounded-md border border-border bg-surface px-2 text-right text-sm text-foreground focus:outline-none focus:ring-2 focus:ring-accent"
      />
      <button
        type="button"
        role="switch"
        aria-checked={command.enabled}
        aria-label={t("AutomationPanel.verifyEnabledAriaLabel", { label: command.label || command.command })}
        onClick={() => void toggleCommand(command.id)}
        className={`relative h-5 w-9 shrink-0 cursor-pointer rounded-full transition-colors ${
          command.enabled ? "bg-accent" : "border border-border bg-surface-2"
        }`}
      >
        <span
          className={`absolute top-0.5 h-4 w-4 rounded-full bg-white shadow transition-[left] ${
            command.enabled ? "left-[18px]" : "left-0.5"
          }`}
        />
      </button>
      <button
        type="button"
        disabled={removing}
        aria-label={t("AutomationPanel.verifyDeleteAriaLabel", { label: command.label || command.command })}
        onClick={async () => {
          setRemoving(true);
          await removeCommand(command.id).catch(() => {});
          setRemoving(false);
        }}
        className="shrink-0 cursor-pointer text-faint transition-colors hover:text-danger disabled:cursor-not-allowed disabled:opacity-50"
      >
        <Trash2 size={14} />
      </button>
    </div>
  );
}

/**
 * One profile's optional model override row (slice 4): a provider+model
 * picker that, once both are chosen, calls `onSet` immediately — no separate
 * "apply" button, since (unlike the vision-override list below, which
 * accumulates many entries) there is only ever one value per profile, so
 * committing on the second selection is the least surprising behavior. Shows
 * a "Same as parent" badge when nothing is configured for this profile —
 * the actual default, since an absent entry in `subagentProfileModels` means
 * exactly that (see that setting's own doc comment).
 */
function SubagentModelOverrideRow({
  labelKey,
  override,
  connectedProviders,
  providerModels,
  onSet,
  onClear,
}: {
  labelKey: string;
  override: { providerId: string; model: string } | undefined;
  connectedProviders: { id: string; label: string }[];
  providerModels: Record<string, { id: string }[]>;
  onSet: (providerId: string, model: string) => void;
  onClear: () => void;
}) {
  const { t } = useT();
  const [providerId, setProviderId] = useState(override?.providerId ?? "");
  const models = providerId ? providerModels[providerId] ?? [] : [];

  return (
    <div className="flex flex-wrap items-center gap-2 border-t border-border py-2.5 first:border-t-0">
      <span className="w-28 shrink-0 text-sm text-foreground">{t(labelKey)}</span>
      <select
        value={providerId}
        onChange={(event) => setProviderId(event.target.value)}
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
        value={override?.providerId === providerId ? override.model : ""}
        onChange={(event) => {
          if (event.target.value) onSet(providerId, event.target.value);
        }}
        disabled={!providerId}
        className="h-8 min-w-0 flex-1 rounded-md border border-border bg-surface px-2 text-sm text-foreground focus:outline-none focus:ring-2 focus:ring-accent disabled:cursor-not-allowed"
      >
        <option value="">{t("AutomationPanel.modelPlaceholderOption")}</option>
        {models.map((model) => (
          <option key={model.id} value={model.id}>
            {model.id}
          </option>
        ))}
      </select>
      {override ? (
        <button type="button" onClick={onClear} className="shrink-0 cursor-pointer text-xs text-faint hover:text-danger">
          {t("AutomationPanel.clearButton")}
        </button>
      ) : (
        <StatusPill tone="neutral">{t("AutomationPanel.subagentModelOverrideDefaultBadge")}</StatusPill>
      )}
    </div>
  );
}

/**
 * Web tools settings — folded into this panel from a standalone "Web" tab
 * (ROADMAP.md §3.9's own decision, previously undone; see git history) so
 * Settings stays at the roadmap's stated 9-ish-tab budget instead of growing
 * a tab per feature. The master `webToolsEnabled` toggle (mirrors
 * `memoryEnabled`'s "disabled = not offered to the model" treatment — the
 * permission prompt shown on every call is the real per-call gate either
 * way), a `search_provider` picker with each provider's own connection
 * fields (Brave key via the OS keychain, SearXNG base URL), and the
 * `allow_local_network` escape hatch with an explicit warning — this toggle
 * re-opens the exact loopback targets (llama-server, Ollama) the SSRF guard
 * in `web.rs` exists to close off. i18n keys stay under the `WebPanel.*`
 * namespace (unchanged) since renaming them would only add translation
 * churn for a purely internal relocation.
 */
function WebSettingsSection() {
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

  useEffect(() => {
    setSearxngUrlInput(settings.searxng_base_url ?? "");
  }, [settings.searxng_base_url]);

  async function handleProviderChange(provider: SearchProvider) {
    setSettingsError(null);
    try {
      await setSettings({ ...settings, search_provider: provider });
    } catch (err) {
      setSettingsError(errorMessage(err));
    }
  }

  async function handleSaveSearxngUrl() {
    setSavingSearxngUrl(true);
    setSettingsError(null);
    try {
      await setSettings({ ...settings, searxng_base_url: searxngUrlInput.trim() || null });
    } catch (err) {
      setSettingsError(errorMessage(err));
    } finally {
      setSavingSearxngUrl(false);
    }
  }

  async function handleAllowLocalNetworkChange(value: boolean) {
    setSettingsError(null);
    try {
      await setSettings({ ...settings, allow_local_network: value });
    } catch (err) {
      setSettingsError(errorMessage(err));
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
      setBraveKeyError(errorMessage(err));
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
      setBraveKeyError(errorMessage(err));
    } finally {
      setRemovingBraveKey(false);
    }
  }

  return (
    <section>
      <h3 className="mb-1 text-xs font-semibold uppercase tracking-wide text-faint">{t("WebPanel.providerHeading")}</h3>
      <p className="mb-2 text-xs text-muted">{t("WebPanel.description")}</p>
      <div className="rounded-lg border border-border bg-background px-3">
        <Toggle
          checked={webToolsEnabled}
          onChange={setWebToolsEnabled}
          label={t("WebPanel.enableToggleLabel")}
          description={t("WebPanel.enableToggleDescription")}
        />
      </div>

      <div className={`mt-3 flex flex-col gap-2.5 rounded-lg border border-border bg-background p-3 ${webToolsEnabled ? "" : "pointer-events-none opacity-50"}`}>
        <div className="flex flex-col gap-2">
          {WEB_PROVIDER_OPTIONS.map((option) => (
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

      <div className={`mt-3 ${webToolsEnabled ? "" : "pointer-events-none opacity-50"}`}>
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
      </div>
    </section>
  );
}

/**
 * Controls the `monkey` terminal CLI's auto-install-onto-`PATH` behavior
 * (cli_install.rs) — on by default. Turning it off doesn't just stop future
 * auto-installs, it immediately uninstalls (removes the symlink/registry
 * entry); turning it back on immediately reinstalls — see
 * `cli_install_set_enabled`'s doc comment for why the toggle and reality are
 * kept from ever visibly disagreeing. `status.error` surfaces a failed
 * install/uninstall attempt (e.g. no writable PATH directory found) inline
 * rather than only in a console log, since this runs silently at every
 * launch otherwise and a stuck-off/stuck-on state would be invisible.
 */
function CliInstallSection() {
  const { t } = useT();
  const status = useCliInstallStore((s) => s.status);
  const loaded = useCliInstallStore((s) => s.loaded);
  const updating = useCliInstallStore((s) => s.updating);
  const refresh = useCliInstallStore((s) => s.refresh);
  const setEnabled = useCliInstallStore((s) => s.setEnabled);

  useEffect(() => {
    void refresh();
  }, [refresh]);

  const [toggleError, setToggleError] = useState<string | null>(null);

  async function handleToggle(value: boolean) {
    setToggleError(null);
    try {
      await setEnabled(value);
    } catch (err) {
      setToggleError(errorMessage(err));
    }
  }

  return (
    <section className="mt-5">
      <h3 className="mb-1 text-xs font-semibold uppercase tracking-wide text-faint">{t("CliInstallSection.heading")}</h3>
      <p className="mb-2 text-xs text-muted">{t("CliInstallSection.description")}</p>
      <div className="rounded-lg border border-border bg-background px-3">
        <Toggle
          checked={status.enabled}
          onChange={(value) => void handleToggle(value)}
          label={t("CliInstallSection.toggleLabel")}
          description={t("CliInstallSection.toggleDescription")}
        />
      </div>

      {loaded && (
        <div className="mt-2 flex items-center gap-2 text-xs">
          {status.installed && status.install_path ? (
            <>
              <StatusPill tone={status.on_path ? "success" : "warning"}>
                {status.on_path ? t("CliInstallSection.installedOnPath") : t("CliInstallSection.installedNotOnPath")}
              </StatusPill>
              <span className="truncate font-mono text-faint">{status.install_path}</span>
            </>
          ) : (
            <StatusPill tone="neutral">
              {status.enabled ? t("CliInstallSection.notInstalled") : t("CliInstallSection.disabled")}
            </StatusPill>
          )}
          {updating && <span className="text-faint">{t("CliInstallSection.updating")}</span>}
        </div>
      )}
      {(status.error || toggleError) && <p className="mt-1.5 text-xs text-danger">{status.error ?? toggleError}</p>}
    </section>
  );
}

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
  const verifyEnabled = useSettingsStore((s) => s.verifyEnabled);
  const setVerifyEnabled = useSettingsStore((s) => s.setVerifyEnabled);
  const verifyMaxRounds = useSettingsStore((s) => s.verifyMaxRounds);
  const setVerifyMaxRounds = useSettingsStore((s) => s.setVerifyMaxRounds);
  const artifactScriptsEnabled = useSettingsStore((s) => s.artifactScriptsEnabled);
  const setArtifactScriptsEnabled = useSettingsStore((s) => s.setArtifactScriptsEnabled);
  const artifactAutoPreview = useSettingsStore((s) => s.artifactAutoPreview);
  const setArtifactAutoPreview = useSettingsStore((s) => s.setArtifactAutoPreview);
  const riskAnnotationsEnabled = useSettingsStore((s) => s.riskAnnotationsEnabled);
  const setRiskAnnotationsEnabled = useSettingsStore((s) => s.setRiskAnnotationsEnabled);
  const subagentsEnabled = useSettingsStore((s) => s.subagentsEnabled);
  const setSubagentsEnabled = useSettingsStore((s) => s.setSubagentsEnabled);
  const skillAutoInvokeEnabled = useSettingsStore((s) => s.skillAutoInvokeEnabled);
  const setSkillAutoInvokeEnabled = useSettingsStore((s) => s.setSkillAutoInvokeEnabled);
  const maxConcurrentSubagents = useSettingsStore((s) => s.maxConcurrentSubagents);
  const setMaxConcurrentSubagents = useSettingsStore((s) => s.setMaxConcurrentSubagents);
  const subagentProfileModels = useSettingsStore((s) => s.subagentProfileModels);
  const setSubagentProfileModel = useSettingsStore((s) => s.setSubagentProfileModel);
  const clearSubagentProfileModel = useSettingsStore((s) => s.clearSubagentProfileModel);

  const providers = useModelStore((s) => s.providers);
  const providerModels = useModelStore((s) => s.providerModels);
  const connectedProviders = useMemo(() => providers.filter((p) => p.has_key), [providers]);

  const roots = useWorkspaceStore((s) => s.roots);
  const rootsVersion = useWorkspaceStore((s) => s.rootsVersion);
  const hasWorkspace = primaryRoot(roots) !== null;
  const verifyCommands = useVerifyStore((s) => s.config.commands);
  const addVerifyCommand = useVerifyStore((s) => s.addCommand);
  const refreshVerifyConfig = useVerifyStore((s) => s.refresh);

  // Reload the verification config whenever the primary workspace changes —
  // it's keyed by workspace root on the backend, so a different folder has a
  // different (possibly empty) command list. Same `rootsVersion`-keyed
  // reload trigger `App.tsx` uses for `FileTree`/`WorkspaceBar`.
  useEffect(() => {
    void refreshVerifyConfig();
  }, [rootsVersion, refreshVerifyConfig]);

  const [overrideProviderId, setOverrideProviderId] = useState("");
  const [overrideModelId, setOverrideModelId] = useState("");
  const overrideModels = overrideProviderId ? providerModels[overrideProviderId] ?? [] : [];
  const overrideEntries = Object.entries(visionOverrides);

  return (
    <div className="flex flex-col gap-6 py-2">
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
        <h3 className="mb-1 text-xs font-semibold uppercase tracking-wide text-faint">{t("AutomationPanel.verifyHeading")}</h3>
        <div className="rounded-lg border border-border bg-background px-3">
          <Toggle
            checked={verifyEnabled}
            onChange={setVerifyEnabled}
            label={t("AutomationPanel.verifyEnabledLabel")}
            description={t("AutomationPanel.verifyEnabledDescription")}
          />
          <label className="flex items-center justify-between gap-3 border-t border-border py-2.5 text-sm">
            <span className="flex flex-col">
              <span className="text-foreground">{t("AutomationPanel.verifyMaxRoundsLabel")}</span>
              <span className="text-xs text-muted">{t("AutomationPanel.verifyMaxRoundsDescription")}</span>
            </span>
            <input
              type="number"
              min={MIN_VERIFY_MAX_ROUNDS}
              max={MAX_VERIFY_MAX_ROUNDS}
              value={verifyMaxRounds}
              onChange={(event) => setVerifyMaxRounds(Number(event.target.value))}
              className="h-8 w-16 shrink-0 rounded-md border border-border bg-surface px-2 text-right text-sm text-foreground focus:outline-none focus:ring-2 focus:ring-accent"
            />
          </label>
          <div className="border-t border-border py-2.5">
            {!hasWorkspace ? (
              <p className="text-xs text-faint">{t("AutomationPanel.verifyNoWorkspaceOpen")}</p>
            ) : (
              <>
                {verifyCommands.length === 0 ? (
                  <p className="pb-2 text-xs text-faint">{t("AutomationPanel.verifyEmptyState")}</p>
                ) : (
                  verifyCommands.map((command) => <VerifyCommandRow key={command.id} command={command} />)
                )}
                <div className={verifyCommands.length > 0 ? "border-t border-border pt-2.5" : ""}>
                  <Button size="sm" variant="secondary" onClick={() => void addVerifyCommand()}>
                    <Plus size={12} />
                    {t("AutomationPanel.verifyAddCommandButton")}
                  </Button>
                </div>
              </>
            )}
          </div>
        </div>
      </section>

      <section>
        <h3 className="mb-1 text-xs font-semibold uppercase tracking-wide text-faint">{t("AutomationPanel.riskAnnotationsHeading")}</h3>
        <div className="rounded-lg border border-border bg-background px-3">
          <Toggle
            checked={riskAnnotationsEnabled}
            onChange={setRiskAnnotationsEnabled}
            label={t("AutomationPanel.riskAnnotationsLabel")}
            description={t("AutomationPanel.riskAnnotationsDescription")}
          />
        </div>
      </section>

      <section>
        <h3 className="mb-1 text-xs font-semibold uppercase tracking-wide text-faint">{t("AutomationPanel.artifactsHeading")}</h3>
        <div className="rounded-lg border border-border bg-background px-3">
          <Toggle
            checked={artifactScriptsEnabled}
            onChange={setArtifactScriptsEnabled}
            label={t("AutomationPanel.artifactScriptsEnabledLabel")}
            description={t("AutomationPanel.artifactScriptsEnabledDescription")}
          />
          <Toggle
            checked={artifactAutoPreview}
            onChange={setArtifactAutoPreview}
            label={t("AutomationPanel.artifactAutoPreviewLabel")}
            description={t("AutomationPanel.artifactAutoPreviewDescription")}
          />
        </div>
      </section>

      <section>
        <h3 className="mb-1 text-xs font-semibold uppercase tracking-wide text-faint">{t("AutomationPanel.subagentsHeading")}</h3>
        <div className="rounded-lg border border-border bg-background px-3">
          <Toggle
            checked={subagentsEnabled}
            onChange={setSubagentsEnabled}
            label={t("AutomationPanel.subagentsEnabledLabel")}
            description={t("AutomationPanel.subagentsEnabledDescription")}
          />
          <label className="flex items-center justify-between gap-3 border-t border-border py-2.5 text-sm">
            <span className="flex flex-col">
              <span className="text-foreground">{t("AutomationPanel.maxConcurrentSubagentsLabel")}</span>
              <span className="text-xs text-muted">{t("AutomationPanel.maxConcurrentSubagentsDescription")}</span>
            </span>
            <input
              type="number"
              min={MIN_MAX_CONCURRENT_SUBAGENTS}
              max={MAX_MAX_CONCURRENT_SUBAGENTS}
              value={maxConcurrentSubagents}
              onChange={(event) => setMaxConcurrentSubagents(Number(event.target.value))}
              className="h-8 w-16 shrink-0 rounded-md border border-border bg-surface px-2 text-right text-sm text-foreground focus:outline-none focus:ring-2 focus:ring-accent"
            />
          </label>
        </div>
        <p className="mb-1 mt-3 text-xs text-muted">{t("AutomationPanel.subagentModelOverrideIntro")}</p>
        <div className="rounded-lg border border-border bg-background px-3">
          <SubagentModelOverrideRow
            labelKey="AutomationPanel.subagentModelOverrideExploreLabel"
            override={subagentProfileModels.explore}
            connectedProviders={connectedProviders}
            providerModels={providerModels}
            onSet={(providerId, model) => setSubagentProfileModel("explore", { providerId, model })}
            onClear={() => clearSubagentProfileModel("explore")}
          />
          <SubagentModelOverrideRow
            labelKey="AutomationPanel.subagentModelOverrideCodeLabel"
            override={subagentProfileModels.code}
            connectedProviders={connectedProviders}
            providerModels={providerModels}
            onSet={(providerId, model) => setSubagentProfileModel("code", { providerId, model })}
            onClear={() => clearSubagentProfileModel("code")}
          />
        </div>
      </section>

      <section>
        <h3 className="mb-1 text-xs font-semibold uppercase tracking-wide text-faint">{t("AutomationPanel.skillsHeading")}</h3>
        <div className="rounded-lg border border-border bg-background px-3">
          <Toggle
            checked={skillAutoInvokeEnabled}
            onChange={setSkillAutoInvokeEnabled}
            label={t("AutomationPanel.skillAutoInvokeEnabledLabel")}
            description={t("AutomationPanel.skillAutoInvokeEnabledDescription")}
          />
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

      <WebSettingsSection />
      <CliInstallSection />
    </div>
  );
}

export default AutomationPanel;
