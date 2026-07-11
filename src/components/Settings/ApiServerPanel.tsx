import { useEffect, useState } from "react";
import { Button, StatusPill } from "../ui";
import type { PillTone } from "../ui/StatusPill";
import { useApiServerStore } from "../../store/apiServerStore";
import { useT } from "../../lib/i18n";

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

/**
 * Settings "API Server" tab (phase 1 — minimal): on/off toggle, port field,
 * a copyable OpenAI-compatible base-URL chip, and the single auto-generated
 * bearer token shown while the server is running. Full per-token
 * scopes/backends management is phase 2 (see the design doc) — this panel
 * intentionally has no token table yet.
 */
export function ApiServerPanel() {
  const { t } = useT();
  const status = useApiServerStore((s) => s.status);
  const loaded = useApiServerStore((s) => s.loaded);
  const portInput = useApiServerStore((s) => s.portInput);
  const setPortInput = useApiServerStore((s) => s.setPortInput);
  const refresh = useApiServerStore((s) => s.refresh);
  const start = useApiServerStore((s) => s.start);
  const stop = useApiServerStore((s) => s.stop);

  useEffect(() => {
    void refresh();
  }, [refresh]);

  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [copiedUrl, setCopiedUrl] = useState(false);
  const [copiedToken, setCopiedToken] = useState(false);

  const running = status.status === "running" || status.status === "starting";
  const baseUrl = `http://127.0.0.1:${status.port || portInput}/v1`;

  async function handleToggle(value: boolean) {
    setError(null);
    setBusy(true);
    try {
      if (value) {
        await start(portInput);
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

  return (
    <div className="flex flex-col gap-4 p-2">
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
        <div className="flex items-center gap-2">
          <StatusPill tone={STATUS_TONES[status.status] ?? "neutral"}>
            {t(`ApiServerPanel.status.${status.status}`)}
          </StatusPill>
          {status.status === "running" && (
            <span className="text-xs text-muted">{t("ApiServerPanel.requestsLabel", { count: status.request_count })}</span>
          )}
        </div>
      )}

      {(error || status.last_error) && <p className="text-xs text-danger">{error ?? status.last_error}</p>}

      <section>
        <h3 className="mb-1 text-xs font-semibold uppercase tracking-wide text-faint">{t("ApiServerPanel.connectionHeading")}</h3>
        <div className="flex flex-col gap-2.5 rounded-lg border border-border bg-background p-3">
          <label className="flex items-center justify-between gap-3 text-sm">
            <span className="text-foreground">{t("ApiServerPanel.portLabel")}</span>
            <input
              type="number"
              min={1}
              max={65535}
              value={portInput}
              disabled={running}
              onChange={(event) => setPortInput(Number(event.target.value))}
              className="h-8 w-24 rounded-md border border-border bg-surface px-2 text-right text-sm text-foreground focus:outline-none focus:ring-2 focus:ring-accent disabled:cursor-not-allowed disabled:opacity-50"
            />
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

          {status.token && (
            <div className="flex flex-col gap-1.5 border-t border-border pt-2.5">
              <span className="text-sm text-foreground">{t("ApiServerPanel.tokenLabel")}</span>
              <div className="flex items-center gap-2">
                <code className="min-w-0 flex-1 truncate rounded-md border border-border bg-surface px-2.5 py-1.5 font-mono text-sm text-foreground">
                  {status.token}
                </code>
                <Button variant="secondary" size="sm" onClick={() => void copy(status.token as string, setCopiedToken)}>
                  {copiedToken ? t("ApiServerPanel.copiedButton") : t("ApiServerPanel.copyButton")}
                </Button>
              </div>
              <p className="text-xs text-faint">{t("ApiServerPanel.tokenHint")}</p>
            </div>
          )}
        </div>
        <p className="mt-1.5 rounded-md bg-warning-soft px-2 py-1.5 text-xs text-warning">{t("ApiServerPanel.authWarning")}</p>
      </section>
    </div>
  );
}

export default ApiServerPanel;
