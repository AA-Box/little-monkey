import { useEffect, useMemo, useState } from "react";
import { Button, StatusPill } from "../ui";
import type { PillTone } from "../ui/StatusPill";
import { useApiServerStore, type Backend, type Scope, type TokenAuditEntry } from "../../store/apiServerStore";
import { useT } from "../../lib/i18n";
import { buildWidgetEmbedSnippet, resolveExpiryPreset, type TokenExpiryPreset } from "../../lib/chatWidgetEmbed";

/** No shared toggle-switch component exists in `ui/` yet — cloned from
 * `AutomationPanel.tsx`'s local `Toggle` (the description-supporting
 * variant) rather than promoted prematurely. */
function Toggle({
  checked,
  onChange,
  label,
  description,
  disabled,
}: {
  checked: boolean;
  onChange: (value: boolean) => void;
  label: string;
  description?: string;
  disabled?: boolean;
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
          disabled={disabled}
          onClick={() => onChange(!checked)}
          className={`relative h-5 w-9 shrink-0 cursor-pointer rounded-full transition-colors disabled:cursor-not-allowed disabled:opacity-50 ${
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

const STATUS_TONES: Record<string, PillTone> = {
  stopped: "neutral",
  starting: "warning",
  running: "success",
  error: "danger",
};

const SCOPE_OPTIONS: Scope[] = ["chat", "models", "embeddings", "knowledge", "workflow_run", "artifact_read"];
const BACKEND_OPTIONS: Backend[] = ["local", "ollama", "providers"];
const EXPIRY_PRESETS: TokenExpiryPreset[] = ["never", "1h", "1d", "7d", "30d", "90d"];

/**
 * Settings "API Server" tab (phase 2): on/off toggle, port field (applies
 * immediately, restarting the server if it's running), autostart/
 * require-token/expose-* toggles (`expose_providers` — the "money-spending
 * switch" per the design doc — gets an explicit confirm), and a full token
 * table with a create-token flow that shows the plaintext exactly once.
 */
export function ApiServerPanel() {
  const { t } = useT();
  const status = useApiServerStore((s) => s.status);
  const config = useApiServerStore((s) => s.config);
  const tokens = useApiServerStore((s) => s.tokens);
  const loaded = useApiServerStore((s) => s.loaded);
  const mintedToken = useApiServerStore((s) => s.mintedToken);
  const refresh = useApiServerStore((s) => s.refresh);
  const start = useApiServerStore((s) => s.start);
  const stop = useApiServerStore((s) => s.stop);
  const setConfig = useApiServerStore((s) => s.setConfig);
  const createToken = useApiServerStore((s) => s.createToken);
  const revokeToken = useApiServerStore((s) => s.revokeToken);
  const dismissMintedToken = useApiServerStore((s) => s.dismissMintedToken);
  const exportAudit = useApiServerStore((s) => s.exportAudit);

  useEffect(() => {
    void refresh();
  }, [refresh]);

  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [copiedUrl, setCopiedUrl] = useState(false);
  const [copiedToken, setCopiedToken] = useState(false);

  const [portInput, setPortInput] = useState(config.port);
  const [savingPort, setSavingPort] = useState(false);
  useEffect(() => {
    setPortInput(config.port);
  }, [config.port]);

  const [confirmingExposeProviders, setConfirmingExposeProviders] = useState(false);

  const [createLabel, setCreateLabel] = useState("");
  const [createScopes, setCreateScopes] = useState<Scope[]>(["chat", "models"]);
  const [createBackends, setCreateBackends] = useState<Backend[]>(["local", "ollama"]);
  const [createExpiry, setCreateExpiry] = useState<TokenExpiryPreset>("never");
  const [creatingToken, setCreatingToken] = useState(false);
  const [createError, setCreateError] = useState<string | null>(null);
  const [confirmingRevokeId, setConfirmingRevokeId] = useState<string | null>(null);

  const [auditRows, setAuditRows] = useState<TokenAuditEntry[] | null>(null);
  const [auditLoading, setAuditLoading] = useState(false);
  const [auditError, setAuditError] = useState<string | null>(null);
  const [copiedAudit, setCopiedAudit] = useState(false);

  const [widgetTokenId, setWidgetTokenId] = useState<string>("");
  const [widgetTokenValue, setWidgetTokenValue] = useState("");
  const [widgetTitle, setWidgetTitle] = useState("");
  const [widgetModel, setWidgetModel] = useState("");
  const [copiedWidgetSnippet, setCopiedWidgetSnippet] = useState(false);

  const running = status.status === "running" || status.status === "starting";
  const baseUrl = `http://127.0.0.1:${status.port || config.port}/v1`;

  // Defaults the widget token picker to whichever token was most recently
  // minted (its plaintext is still in hand via `mintedToken`), falling back
  // to the first existing token otherwise — but never auto-fills a
  // plaintext for a token this session didn't just create, since Little
  // Monkey never persists or re-reveals one (see `chatWidgetEmbed.ts`'s doc
  // comment).
  useEffect(() => {
    if (widgetTokenId && tokens.some((t) => t.id === widgetTokenId)) return;
    const defaultId = mintedToken?.entry.id ?? tokens[0]?.id ?? "";
    setWidgetTokenId(defaultId);
  }, [tokens, mintedToken, widgetTokenId]);

  useEffect(() => {
    if (mintedToken && mintedToken.entry.id === widgetTokenId) {
      setWidgetTokenValue(mintedToken.token);
    }
  }, [mintedToken, widgetTokenId]);

  const widgetSelectedToken = tokens.find((t) => t.id === widgetTokenId) ?? null;
  // Scopes beyond `chat` on the embedded token — the SDK README's own
  // guidance is "do not reuse a token that also carries models/embeddings/
  // knowledge/artifact_read/workflow_run" for the widget, since the plaintext
  // ends up readable by anyone who views the embedding page's source.
  const widgetExtraScopeLabels = widgetSelectedToken
    ? widgetSelectedToken.scopes.filter((scope) => scope !== "chat").map((scope) => t(`ApiServerPanel.scope.${scope}`))
    : [];

  // Manually switching the reference-token dropdown must not leave a stale
  // plaintext (from a previously selected token) paired with a scope warning
  // that now describes a *different* token — only re-populate the plaintext
  // automatically when switching back to the token just minted this session.
  function handleWidgetTokenIdChange(nextId: string) {
    setWidgetTokenId(nextId);
    setWidgetTokenValue(mintedToken && mintedToken.entry.id === nextId ? mintedToken.token : "");
  }

  const widgetSnippet = useMemo(
    () =>
      buildWidgetEmbedSnippet({
        baseUrl,
        token: widgetTokenValue,
        model: widgetModel.trim() || undefined,
        title: widgetTitle.trim() || undefined,
      }),
    [baseUrl, widgetTokenValue, widgetModel, widgetTitle],
  );

  async function handleToggle(value: boolean) {
    setError(null);
    setBusy(true);
    try {
      if (value) {
        await start();
      } else {
        await stop();
      }
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    } finally {
      setBusy(false);
    }
  }

  async function copy(text: string, mark: (value: boolean) => void) {
    await navigator.clipboard.writeText(text);
    mark(true);
    setTimeout(() => mark(false), 1500);
  }

  async function updateConfig(patch: Partial<typeof config>) {
    setError(null);
    try {
      await setConfig({ ...config, ...patch });
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    }
  }

  async function handleSavePort() {
    setSavingPort(true);
    setError(null);
    try {
      await setConfig({ ...config, port: portInput });
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    } finally {
      setSavingPort(false);
    }
  }

  function handleExposeProvidersToggle(value: boolean) {
    if (value) {
      setConfirmingExposeProviders(true);
    } else {
      void updateConfig({ expose_providers: false });
    }
  }

  function confirmExposeProviders() {
    setConfirmingExposeProviders(false);
    void updateConfig({ expose_providers: true });
  }

  function toggleScope(scope: Scope) {
    setCreateScopes((prev) => (prev.includes(scope) ? prev.filter((s) => s !== scope) : [...prev, scope]));
  }

  function toggleBackend(backend: Backend) {
    setCreateBackends((prev) => (prev.includes(backend) ? prev.filter((b) => b !== backend) : [...prev, backend]));
  }

  async function handleCreateToken() {
    setCreateError(null);
    setCreatingToken(true);
    try {
      await createToken(createLabel.trim(), createScopes, createBackends, resolveExpiryPreset(createExpiry));
      setCreateLabel("");
      setCreateExpiry("never");
    } catch (err) {
      setCreateError(err instanceof Error ? err.message : String(err));
    } finally {
      setCreatingToken(false);
    }
  }

  async function handleRefreshAudit() {
    setAuditError(null);
    setAuditLoading(true);
    try {
      setAuditRows(await exportAudit());
    } catch (err) {
      setAuditError(err instanceof Error ? err.message : String(err));
    } finally {
      setAuditLoading(false);
    }
  }

  async function handleRevokeToken(id: string) {
    try {
      await revokeToken(id);
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err));
    } finally {
      setConfirmingRevokeId(null);
    }
  }

  function formatDate(ms: number): string {
    return new Date(ms).toLocaleString();
  }

  // A token past its `expires_at` already gets a 401 from every request
  // (see `authenticate`'s expiry check) whether or not it's been explicitly
  // revoked — so the panel must never call it "Active" just because it's
  // still sitting in `config.tokens` rather than `config.revoked`.
  function isExpired(expiresAt: number | null): boolean {
    return expiresAt !== null && expiresAt <= Date.now();
  }

  return (
    <div className="flex flex-col gap-4 py-2">
      <p className="text-xs text-muted">{t("ApiServerPanel.description")}</p>

      <div className="rounded-lg border border-border bg-background px-3">
        <Toggle
          checked={running}
          onChange={(value) => void handleToggle(value)}
          label={t("ApiServerPanel.enableToggleLabel")}
          description={t("ApiServerPanel.enableToggleDescription")}
          disabled={busy}
        />
      </div>

      {loaded && (
        <div className="flex flex-wrap items-center gap-2">
          <StatusPill tone={STATUS_TONES[status.status] ?? "neutral"}>
            {t(`ApiServerPanel.status.${status.status}`)}
          </StatusPill>
          {status.status === "running" && (
            <span className="text-xs text-muted">{t("ApiServerPanel.requestsLabel", { count: status.request_count })}</span>
          )}
          {status.status === "running" && status.last_request_at && (
            <span className="text-xs text-faint">
              {t("ApiServerPanel.lastRequestLabel", { time: formatDate(status.last_request_at) })}
            </span>
          )}
        </div>
      )}

      {(error || status.last_error) && <p className="text-xs text-danger">{error ?? status.last_error}</p>}

      <section>
        <h3 className="mb-1 text-xs font-semibold uppercase tracking-wide text-faint">{t("ApiServerPanel.connectionHeading")}</h3>
        <div className="flex flex-col gap-2.5 rounded-lg border border-border bg-background p-3">
          <label className="flex items-center justify-between gap-3 text-sm">
            <span className="text-foreground">{t("ApiServerPanel.portLabel")}</span>
            <div className="flex items-center gap-2">
              <input
                type="number"
                min={1}
                max={65535}
                value={portInput}
                onChange={(event) => setPortInput(Number(event.target.value))}
                className="h-8 w-24 rounded-md border border-border bg-surface px-2 text-right text-sm text-foreground focus:outline-none focus:ring-2 focus:ring-accent"
              />
              <Button variant="secondary" size="sm" onClick={() => void handleSavePort()} disabled={savingPort || portInput === config.port}>
                {savingPort ? t("ApiServerPanel.portSavingButton") : t("ApiServerPanel.portSaveButton")}
              </Button>
            </div>
          </label>
          <p className="text-xs text-faint">{t("ApiServerPanel.portHint")}</p>

          <div className="flex flex-col gap-1.5 border-t border-border pt-2.5">
            <span className="text-sm text-foreground">{t("ApiServerPanel.baseUrlLabel")}</span>
            <div className="flex items-center gap-2">
              <code className="min-w-0 flex-1 truncate rounded-md border border-border bg-surface px-2.5 py-1.5 font-mono text-sm text-foreground">
                {baseUrl}
              </code>
              <Button variant="secondary" size="sm" onClick={() => void copy(baseUrl, setCopiedUrl)}>
                {copiedUrl ? t("ApiServerPanel.copiedButton") : t("ApiServerPanel.copyButton")}
              </Button>
            </div>
          </div>
        </div>
      </section>

      <section>
        <h3 className="mb-1 text-xs font-semibold uppercase tracking-wide text-faint">{t("ApiServerPanel.settingsHeading")}</h3>
        <div className="rounded-lg border border-border bg-background px-3">
          <Toggle
            checked={config.autostart}
            onChange={(value) => void updateConfig({ autostart: value })}
            label={t("ApiServerPanel.autostartLabel")}
            description={t("ApiServerPanel.autostartDescription")}
          />
          <div className="border-t border-border">
            <Toggle
              checked={config.require_token}
              onChange={(value) => void updateConfig({ require_token: value })}
              label={t("ApiServerPanel.requireTokenLabel")}
              description={t("ApiServerPanel.requireTokenDescription")}
            />
          </div>
          <div className="border-t border-border">
            <Toggle
              checked={config.expose_ollama}
              onChange={(value) => void updateConfig({ expose_ollama: value })}
              label={t("ApiServerPanel.exposeOllamaLabel")}
              description={t("ApiServerPanel.exposeOllamaDescription")}
            />
          </div>
          <div className="border-t border-border">
            <Toggle
              checked={config.expose_providers}
              onChange={handleExposeProvidersToggle}
              label={t("ApiServerPanel.exposeProvidersLabel")}
              description={t("ApiServerPanel.exposeProvidersDescription")}
            />
          </div>
        </div>

        {!config.require_token && (
          <p className="mt-1.5 rounded-md bg-warning-soft px-2 py-1.5 text-xs text-warning">
            {t("ApiServerPanel.requireTokenOffWarning")}
          </p>
        )}
        {config.expose_providers && (
          <p className="mt-1.5 rounded-md bg-warning-soft px-2 py-1.5 text-xs text-warning">
            {t("ApiServerPanel.exposeProvidersWarning")}
          </p>
        )}

        {confirmingExposeProviders && (
          <div className="mt-1.5 flex flex-col gap-2 rounded-md border border-danger bg-danger-soft p-2.5">
            <p className="text-xs text-danger">{t("ApiServerPanel.exposeProvidersConfirmMessage")}</p>
            <div className="flex justify-end gap-2">
              <Button variant="ghost" size="sm" onClick={() => setConfirmingExposeProviders(false)}>
                {t("ApiServerPanel.exposeProvidersCancelButton")}
              </Button>
              <Button variant="danger" size="sm" onClick={confirmExposeProviders}>
                {t("ApiServerPanel.exposeProvidersConfirmButton")}
              </Button>
            </div>
          </div>
        )}
      </section>

      <section>
        <h3 className="mb-1 text-xs font-semibold uppercase tracking-wide text-faint">{t("ApiServerPanel.tokensHeading")}</h3>

        {mintedToken && (
          <div className="mb-2 flex flex-col gap-1.5 rounded-lg border border-accent bg-accent-soft p-3">
            <p className="text-xs font-medium text-accent">{t("ApiServerPanel.mintedTokenWarning")}</p>
            <div className="flex items-center gap-2">
              <code className="min-w-0 flex-1 truncate rounded-md border border-border bg-surface px-2.5 py-1.5 font-mono text-sm text-foreground">
                {mintedToken.token}
              </code>
              <Button variant="secondary" size="sm" onClick={() => void copy(mintedToken.token, setCopiedToken)}>
                {copiedToken ? t("ApiServerPanel.copiedButton") : t("ApiServerPanel.copyButton")}
              </Button>
              <Button variant="ghost" size="sm" onClick={dismissMintedToken}>
                {t("ApiServerPanel.mintedTokenDismissButton")}
              </Button>
            </div>
          </div>
        )}

        {tokens.length === 0 ? (
          <p className="px-1 text-xs text-faint">{t("ApiServerPanel.tokensEmptyState")}</p>
        ) : (
          <div className="flex flex-col gap-2">
            {tokens.map((token) => (
              <div key={token.id} className="rounded-lg border border-border bg-background p-3">
                <div className="flex flex-wrap items-center gap-2">
                  <span className="truncate text-sm font-medium text-foreground">{token.label}</span>
                  {token.scopes.map((scope) => (
                    <span key={scope} className="rounded bg-surface-2 px-1.5 py-0.5 text-[10px] font-medium uppercase text-muted">
                      {t(`ApiServerPanel.scope.${scope}`)}
                    </span>
                  ))}
                  {token.backends.map((backend) => (
                    <span key={backend} className="rounded bg-accent-soft px-1.5 py-0.5 text-[10px] font-medium uppercase text-accent">
                      {t(`ApiServerPanel.backend.${backend}`)}
                    </span>
                  ))}
                  <div className="ml-auto shrink-0">
                    {confirmingRevokeId === token.id ? (
                      <span className="flex items-center gap-1">
                        <Button variant="ghost" size="sm" onClick={() => setConfirmingRevokeId(null)}>
                          {t("ApiServerPanel.revokeCancelButton")}
                        </Button>
                        <Button variant="danger" size="sm" onClick={() => void handleRevokeToken(token.id)}>
                          {t("ApiServerPanel.revokeConfirmButton")}
                        </Button>
                      </span>
                    ) : (
                      <Button variant="ghost" size="sm" onClick={() => setConfirmingRevokeId(token.id)}>
                        {t("ApiServerPanel.revokeButton")}
                      </Button>
                    )}
                  </div>
                </div>
                <p className="mt-1 text-xs text-faint">
                  {t("ApiServerPanel.createdLabel", { date: formatDate(token.created_at) })}
                  {" · "}
                  {token.last_used_at ? t("ApiServerPanel.lastUsedLabel", { date: formatDate(token.last_used_at) }) : t("ApiServerPanel.neverUsedLabel")}
                  {" · "}
                  {token.expires_at ? (
                    <span className={isExpired(token.expires_at) ? "font-medium text-danger" : undefined}>
                      {isExpired(token.expires_at)
                        ? t("ApiServerPanel.expiredLabel", { date: formatDate(token.expires_at) })
                        : t("ApiServerPanel.expiresLabel", { date: formatDate(token.expires_at) })}
                    </span>
                  ) : (
                    t("ApiServerPanel.neverExpiresLabel")
                  )}
                </p>
              </div>
            ))}
          </div>
        )}

        <div className="mt-2 flex flex-col gap-2 rounded-lg border border-dashed border-border p-3">
          <p className="text-xs font-semibold uppercase tracking-wider text-faint">{t("ApiServerPanel.createTokenHeading")}</p>
          <input
            type="text"
            value={createLabel}
            onChange={(event) => setCreateLabel(event.target.value)}
            placeholder={t("ApiServerPanel.createTokenLabelPlaceholder")}
            className="h-8 rounded-md border border-border bg-surface px-2.5 text-sm text-foreground placeholder:text-faint focus:outline-none focus:ring-2 focus:ring-accent"
          />

          <div className="flex flex-col gap-1">
            <span className="text-xs text-muted">{t("ApiServerPanel.createTokenScopesLabel")}</span>
            <div className="flex flex-wrap gap-3">
              {SCOPE_OPTIONS.map((scope) => (
                <label key={scope} className="flex cursor-pointer items-center gap-1.5 text-xs text-foreground">
                  <input type="checkbox" checked={createScopes.includes(scope)} onChange={() => toggleScope(scope)} className="accent-accent" />
                  {t(`ApiServerPanel.scope.${scope}`)}
                </label>
              ))}
            </div>
          </div>

          <div className="flex flex-col gap-1">
            <span className="text-xs text-muted">{t("ApiServerPanel.createTokenBackendsLabel")}</span>
            <div className="flex flex-wrap gap-3">
              {BACKEND_OPTIONS.map((backend) => (
                <label key={backend} className="flex cursor-pointer items-center gap-1.5 text-xs text-foreground">
                  <input type="checkbox" checked={createBackends.includes(backend)} onChange={() => toggleBackend(backend)} className="accent-accent" />
                  {t(`ApiServerPanel.backend.${backend}`)}
                </label>
              ))}
            </div>
          </div>

          <label className="flex items-center justify-between gap-2 text-xs text-muted">
            <span>{t("ApiServerPanel.createTokenExpiryLabel")}</span>
            <select
              value={createExpiry}
              onChange={(event) => setCreateExpiry(event.target.value as TokenExpiryPreset)}
              className="h-7 rounded-md border border-border bg-surface px-2 text-xs text-foreground focus:outline-none focus:ring-2 focus:ring-accent"
            >
              {EXPIRY_PRESETS.map((preset) => (
                <option key={preset} value={preset}>
                  {t(`ApiServerPanel.expiryPreset.${preset}`)}
                </option>
              ))}
            </select>
          </label>

          {createError && <p className="text-xs text-danger">{createError}</p>}

          <Button
            variant="primary"
            size="sm"
            onClick={() => void handleCreateToken()}
            disabled={!createLabel.trim() || createScopes.length === 0 || createBackends.length === 0 || creatingToken}
          >
            {creatingToken ? t("ApiServerPanel.createTokenCreatingButton") : t("ApiServerPanel.createTokenButton")}
          </Button>
        </div>
      </section>

      <section>
        <div className="mb-1 flex items-center justify-between">
          <h3 className="text-xs font-semibold uppercase tracking-wide text-faint">{t("ApiServerPanel.auditHeading")}</h3>
          <div className="flex items-center gap-2">
            {auditRows && (
              <Button
                variant="secondary"
                size="sm"
                onClick={() => void copy(JSON.stringify(auditRows, null, 2), setCopiedAudit)}
              >
                {copiedAudit ? t("ApiServerPanel.copiedButton") : t("ApiServerPanel.auditCopyButton")}
              </Button>
            )}
            <Button variant="secondary" size="sm" onClick={() => void handleRefreshAudit()} disabled={auditLoading}>
              {auditLoading ? t("ApiServerPanel.auditRefreshingButton") : t("ApiServerPanel.auditRefreshButton")}
            </Button>
          </div>
        </div>
        <p className="mb-1.5 text-xs text-faint">{t("ApiServerPanel.auditDescription")}</p>

        {auditError && <p className="mb-1.5 text-xs text-danger">{auditError}</p>}

        {auditRows === null ? (
          <p className="px-1 text-xs text-faint">{t("ApiServerPanel.auditNotLoadedState")}</p>
        ) : auditRows.length === 0 ? (
          <p className="px-1 text-xs text-faint">{t("ApiServerPanel.auditEmptyState")}</p>
        ) : (
          <div className="flex flex-col gap-2">
            {auditRows.map((row) => {
              const rowExpired = !row.revoked_at && isExpired(row.expires_at);
              return (
                <div key={row.id} className="rounded-lg border border-border bg-background p-3">
                  <div className="flex flex-wrap items-center gap-2">
                    <span className="truncate text-sm font-medium text-foreground">{row.label}</span>
                    {row.scopes.map((scope) => (
                      <span key={scope} className="rounded bg-surface-2 px-1.5 py-0.5 text-[10px] font-medium uppercase text-muted">
                        {t(`ApiServerPanel.scope.${scope}`)}
                      </span>
                    ))}
                    <span
                      className={`ml-auto shrink-0 rounded px-1.5 py-0.5 text-[10px] font-medium uppercase ${
                        row.revoked_at
                          ? "bg-danger-soft text-danger"
                          : rowExpired
                            ? "bg-warning-soft text-warning"
                            : "bg-accent-soft text-accent"
                      }`}
                    >
                      {row.revoked_at
                        ? t("ApiServerPanel.auditRevokedBadge")
                        : rowExpired
                          ? t("ApiServerPanel.auditExpiredBadge")
                          : t("ApiServerPanel.auditActiveBadge")}
                    </span>
                  </div>
                  <p className="mt-1 text-xs text-faint">
                    {t("ApiServerPanel.createdLabel", { date: formatDate(row.created_at) })}
                    {row.revoked_at ? (
                      <>
                        {" · "}
                        {t("ApiServerPanel.auditRevokedLabel", { date: formatDate(row.revoked_at) })}
                      </>
                    ) : row.expires_at ? (
                      <>
                        {" · "}
                        {rowExpired
                          ? t("ApiServerPanel.expiredLabel", { date: formatDate(row.expires_at) })
                          : t("ApiServerPanel.expiresLabel", { date: formatDate(row.expires_at) })}
                      </>
                    ) : (
                      <>
                        {" · "}
                        {t("ApiServerPanel.neverExpiresLabel")}
                      </>
                    )}
                  </p>
                </div>
              );
            })}
          </div>
        )}
      </section>

      <section>
        <h3 className="mb-1 text-xs font-semibold uppercase tracking-wide text-faint">{t("ApiServerPanel.widgetHeading")}</h3>
        <p className="mb-1.5 text-xs text-faint">{t("ApiServerPanel.widgetDescription")}</p>

        <div className="flex flex-col gap-2.5 rounded-lg border border-border bg-background p-3">
          {tokens.length === 0 ? (
            <p className="text-xs text-faint">{t("ApiServerPanel.widgetNoTokensState")}</p>
          ) : (
            <label className="flex flex-col gap-1 text-xs text-muted">
              {t("ApiServerPanel.widgetTokenLabel")}
              <select
                value={widgetTokenId}
                onChange={(event) => handleWidgetTokenIdChange(event.target.value)}
                className="h-8 rounded-md border border-border bg-surface px-2 text-sm text-foreground focus:outline-none focus:ring-2 focus:ring-accent"
              >
                {tokens.map((token) => (
                  <option key={token.id} value={token.id}>
                    {token.label}
                  </option>
                ))}
              </select>
            </label>
          )}

          {widgetSelectedToken && !widgetSelectedToken.scopes.includes("chat") && (
            <p className="rounded-md bg-warning-soft px-2 py-1.5 text-xs text-warning">
              {t("ApiServerPanel.widgetScopeWarning")}
            </p>
          )}

          {widgetSelectedToken && widgetExtraScopeLabels.length > 0 && (
            <p className="rounded-md bg-warning-soft px-2 py-1.5 text-xs text-warning">
              {t("ApiServerPanel.widgetBroadScopeWarning", { scopes: widgetExtraScopeLabels.join(", ") })}
            </p>
          )}

          <label className="flex flex-col gap-1 text-xs text-muted">
            {t("ApiServerPanel.widgetPasteTokenLabel")}
            <input
              type="text"
              value={widgetTokenValue}
              onChange={(event) => setWidgetTokenValue(event.target.value)}
              placeholder={t("ApiServerPanel.widgetPasteTokenPlaceholder")}
              className="h-8 rounded-md border border-border bg-surface px-2.5 font-mono text-xs text-foreground placeholder:text-faint focus:outline-none focus:ring-2 focus:ring-accent"
            />
          </label>

          <div className="flex flex-col gap-2 sm:flex-row">
            <label className="flex flex-1 flex-col gap-1 text-xs text-muted">
              {t("ApiServerPanel.widgetTitleLabel")}
              <input
                type="text"
                value={widgetTitle}
                onChange={(event) => setWidgetTitle(event.target.value)}
                placeholder={t("ApiServerPanel.widgetTitlePlaceholder")}
                className="h-8 rounded-md border border-border bg-surface px-2.5 text-sm text-foreground placeholder:text-faint focus:outline-none focus:ring-2 focus:ring-accent"
              />
            </label>
            <label className="flex flex-1 flex-col gap-1 text-xs text-muted">
              {t("ApiServerPanel.widgetModelLabel")}
              <input
                type="text"
                value={widgetModel}
                onChange={(event) => setWidgetModel(event.target.value)}
                placeholder={t("ApiServerPanel.widgetModelPlaceholder")}
                className="h-8 rounded-md border border-border bg-surface px-2.5 text-sm text-foreground placeholder:text-faint focus:outline-none focus:ring-2 focus:ring-accent"
              />
            </label>
          </div>

          <div className="flex flex-col gap-1.5">
            <div className="flex items-center justify-between">
              <span className="text-xs text-muted">{t("ApiServerPanel.widgetSnippetLabel")}</span>
              <Button variant="secondary" size="sm" onClick={() => void copy(widgetSnippet, setCopiedWidgetSnippet)}>
                {copiedWidgetSnippet ? t("ApiServerPanel.copiedButton") : t("ApiServerPanel.widgetCopySnippetButton")}
              </Button>
            </div>
            <pre className="max-h-48 overflow-auto rounded-md border border-border bg-surface p-2.5 font-mono text-xs text-foreground">
              {widgetSnippet}
            </pre>
            <p className="text-xs text-faint">{t("ApiServerPanel.widgetSnippetHint")}</p>
          </div>
        </div>
      </section>
    </div>
  );
}

export default ApiServerPanel;
