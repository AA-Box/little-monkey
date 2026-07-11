import { useState } from "react";
import { ChevronDown, Plug, RefreshCw, Trash2 } from "lucide-react";
import { Button, StatusPill, type PillTone } from "../ui";
import { useMcpStore, type McpServerEntry, type McpServerInfo, type McpStatus } from "../../store/mcpStore";
import { useT } from "../../lib/i18n";
import { AddMcpServerForm } from "./AddMcpServerForm";

/** No shared toggle-switch component exists in `ui/` yet — cloned from
 * `AutomationPanel.tsx`'s local `Toggle` rather than promoted prematurely. */
function Toggle({
  checked,
  onChange,
  label,
}: {
  checked: boolean;
  onChange: (value: boolean) => void;
  label: string;
}) {
  return (
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
  );
}

const STATUS_TONE: Record<McpStatus, PillTone> = {
  connecting: "warning",
  connected: "success",
  error: "danger",
  disconnected: "neutral",
};

/** Total cached tools across every connected+enabled server past which
 * `McpPanel` shows a soft "that's a lot of tools" warning — design doc's
 * "context bloat" risk note: no hard cap, just a nudge toward allowlisting. */
const TOOL_COUNT_WARNING_THRESHOLD = 40;

function toEntry(server: McpServerInfo, overrides: Partial<McpServerEntry> = {}): McpServerEntry {
  return {
    id: server.id,
    label: server.label,
    transport: server.transport,
    enabled: server.enabled,
    tool_allowlist: server.toolAllowlist,
    timeout_secs: server.timeoutSecs,
    ...overrides,
  };
}

/** Last non-empty line of a (usually single-line) error message — mirrors
 * `OllamaPullForm.tsx`'s pull-error surfacing: show the actual failure
 * verbatim, not a paraphrase. */
function lastErrorLine(message: string): string {
  const lines = message.trim().split("\n").filter((line) => line.trim().length > 0);
  return lines.length > 0 ? lines[lines.length - 1] : message;
}

function transportSummary(server: McpServerInfo): string {
  return server.transport.type === "stdio"
    ? [server.transport.command, ...server.transport.args].join(" ")
    : server.transport.url;
}

/** One configured server: status pill, enable toggle, reconnect/remove
 * actions, and a disclosure of its cached tools with per-tool allowlist
 * checkboxes (UX modeled on `OpenRouterModelsPanel`'s curation list). */
function ServerRow({ server }: { server: McpServerInfo }) {
  const { t } = useT();
  const setEnabled = useMcpStore((s) => s.setEnabled);
  const updateServer = useMcpStore((s) => s.updateServer);
  const connect = useMcpStore((s) => s.connect);
  const removeServer = useMcpStore((s) => s.removeServer);
  const setHttpToken = useMcpStore((s) => s.setHttpToken);
  const removeHttpToken = useMcpStore((s) => s.removeHttpToken);

  const [reconnecting, setReconnecting] = useState(false);
  const [confirmingRemove, setConfirmingRemove] = useState(false);
  const [removing, setRemoving] = useState(false);
  const [actionError, setActionError] = useState<string | null>(null);
  const [timeoutText, setTimeoutText] = useState(String(server.timeoutSecs ?? ""));
  const [savingTimeout, setSavingTimeout] = useState(false);
  const [tokenInput, setTokenInput] = useState("");
  const [savingToken, setSavingToken] = useState(false);
  const [removingToken, setRemovingToken] = useState(false);

  async function handleReconnect() {
    setReconnecting(true);
    setActionError(null);
    try {
      await connect(server.id);
    } catch (err) {
      setActionError(err instanceof Error ? err.message : String(err));
    } finally {
      setReconnecting(false);
    }
  }

  async function handleRemove() {
    setRemoving(true);
    setActionError(null);
    try {
      await removeServer(server.id);
    } catch (err) {
      setActionError(err instanceof Error ? err.message : String(err));
      setRemoving(false);
    }
  }

  async function handleSaveTimeout() {
    const trimmed = timeoutText.trim();
    const parsed = trimmed.length === 0 ? null : Number(trimmed);
    const nextTimeout = parsed !== null && Number.isFinite(parsed) && parsed > 0 ? Math.round(parsed) : null;
    setSavingTimeout(true);
    setActionError(null);
    try {
      await updateServer(toEntry(server, { timeout_secs: nextTimeout }));
    } catch (err) {
      setActionError(err instanceof Error ? err.message : String(err));
    } finally {
      setSavingTimeout(false);
    }
  }

  async function handleSaveToken() {
    if (!tokenInput.trim()) return;
    setSavingToken(true);
    setActionError(null);
    try {
      await setHttpToken(server.id, tokenInput.trim());
      setTokenInput("");
    } catch (err) {
      setActionError(err instanceof Error ? err.message : String(err));
    } finally {
      setSavingToken(false);
    }
  }

  async function handleRemoveToken() {
    setRemovingToken(true);
    setActionError(null);
    try {
      await removeHttpToken(server.id);
    } catch (err) {
      setActionError(err instanceof Error ? err.message : String(err));
    } finally {
      setRemovingToken(false);
    }
  }

  function toggleAllowedTool(toolName: string) {
    const allNames = server.tools.map((tool) => tool.name);
    const current = server.toolAllowlist ?? allNames;
    const next = current.includes(toolName)
      ? current.filter((name) => name !== toolName)
      : [...current, toolName];
    const nextAllowlist = next.length === allNames.length ? null : next;
    void updateServer(toEntry(server, { tool_allowlist: nextAllowlist }));
  }

  const allowedSet = new Set(server.toolAllowlist ?? server.tools.map((tool) => tool.name));

  return (
    <div className="rounded-lg border border-border bg-background p-3">
      <div className="flex items-center gap-2">
        <span className="truncate text-sm font-medium text-foreground">{server.label}</span>
        <StatusPill tone={STATUS_TONE[server.status]}>{t(`McpPanel.status_${server.status}`)}</StatusPill>
        <div className="ml-auto flex shrink-0 items-center gap-2">
          <Toggle
            checked={server.enabled}
            onChange={(value) => void setEnabled(server.id, value)}
            label={t("McpPanel.enableToggleAriaLabel", { label: server.label })}
          />
          <Button variant="ghost" size="sm" onClick={() => void handleReconnect()} disabled={reconnecting || !server.enabled}>
            <RefreshCw size={12} className={reconnecting ? "animate-spin" : ""} />
            {t("McpPanel.reconnectButton")}
          </Button>
          {confirmingRemove ? (
            <span className="flex items-center gap-1">
              <Button variant="ghost" size="sm" onClick={() => setConfirmingRemove(false)} disabled={removing}>
                {t("McpPanel.removeCancelButton")}
              </Button>
              <Button variant="danger" size="sm" onClick={() => void handleRemove()} disabled={removing}>
                {removing ? t("McpPanel.removingButton") : t("McpPanel.removeConfirmButton")}
              </Button>
            </span>
          ) : (
            <Button variant="ghost" size="sm" onClick={() => setConfirmingRemove(true)}>
              <Trash2 size={12} />
              {t("McpPanel.removeButton")}
            </Button>
          )}
        </div>
      </div>

      <p className="mt-1 truncate font-mono text-xs text-faint">{transportSummary(server)}</p>

      {server.status === "error" && server.error && (
        <p className="mt-1.5 text-xs text-danger">{lastErrorLine(server.error)}</p>
      )}
      {actionError && <p className="mt-1.5 text-xs text-danger">{actionError}</p>}

      <details className="group mt-2">
        <summary className="flex cursor-pointer list-none items-center gap-1.5 text-xs text-muted [&::-webkit-details-marker]:hidden">
          <ChevronDown size={13} className="transition-transform group-open:rotate-180" />
          {t("McpPanel.connectionSettingsDisclosure")}
        </summary>
        <div className="mt-1.5 flex flex-col gap-2 border-t border-border pt-1.5">
          <div className="flex items-center gap-1.5">
            <span className="shrink-0 text-xs text-muted">{t("McpPanel.timeoutLabel")}</span>
            <input
              type="number"
              min={1}
              value={timeoutText}
              onChange={(event) => setTimeoutText(event.target.value)}
              placeholder={t("McpPanel.timeoutPlaceholder")}
              className="h-7 w-20 rounded-md border border-border bg-surface px-2 font-mono text-xs text-foreground placeholder:font-sans placeholder:text-faint focus:outline-none focus:ring-2 focus:ring-accent"
            />
            <Button variant="ghost" size="sm" onClick={() => void handleSaveTimeout()} disabled={savingTimeout}>
              {savingTimeout ? t("McpPanel.timeoutSavingButton") : t("McpPanel.timeoutSaveButton")}
            </Button>
          </div>

          {server.transport.type === "http" && (
            <div className="flex flex-col gap-1">
              {server.hasHttpToken ? (
                <div className="flex flex-wrap items-center gap-1.5">
                  <span className="font-mono text-xs text-muted">{t("McpPanel.tokenSaved")}</span>
                  <Button
                    variant="ghost"
                    size="sm"
                    onClick={() => void handleRemoveToken()}
                    disabled={removingToken}
                    className="text-danger hover:bg-danger-soft"
                  >
                    {removingToken ? t("McpPanel.tokenRemovingButton") : t("McpPanel.tokenRemoveButton")}
                  </Button>
                </div>
              ) : (
                <div className="flex items-center gap-1.5">
                  <input
                    type="password"
                    value={tokenInput}
                    onChange={(event) => setTokenInput(event.target.value)}
                    placeholder={t("McpPanel.tokenPlaceholder")}
                    autoComplete="off"
                    className="h-7 min-w-0 flex-1 rounded-md border border-border bg-surface px-2 font-mono text-xs text-foreground placeholder:font-sans placeholder:text-faint focus:outline-none focus:ring-2 focus:ring-accent"
                  />
                  <Button variant="ghost" size="sm" onClick={() => void handleSaveToken()} disabled={savingToken || !tokenInput.trim()}>
                    {savingToken ? t("McpPanel.tokenSavingButton") : t("McpPanel.tokenSaveButton")}
                  </Button>
                </div>
              )}
            </div>
          )}
        </div>
      </details>

      <details className="group mt-2">
        <summary className="flex cursor-pointer list-none items-center gap-1.5 text-xs text-muted [&::-webkit-details-marker]:hidden">
          <ChevronDown size={13} className="transition-transform group-open:rotate-180" />
          {t("McpPanel.toolsDisclosure", { count: server.tools.length })}
        </summary>
        <div className="mt-1.5 flex flex-col gap-0.5 border-t border-border pt-1.5">
          {server.tools.length === 0 ? (
            <p className="px-1 text-xs text-faint">{t("McpPanel.noToolsCached")}</p>
          ) : (
            server.tools.map((tool) => (
              <label key={tool.name} className="flex items-start gap-2 rounded-md px-1 py-1 text-xs hover:bg-surface-2">
                <input
                  type="checkbox"
                  checked={allowedSet.has(tool.name)}
                  onChange={() => toggleAllowedTool(tool.name)}
                  className="mt-0.5 accent-accent"
                />
                <span className="min-w-0 flex-1">
                  <span className="font-mono text-foreground">{tool.name}</span>
                  {tool.description && <span className="ml-1.5 text-faint">{tool.description}</span>}
                </span>
              </label>
            ))
          )}
        </div>
      </details>
    </div>
  );
}

/**
 * Settings "MCP" tab: the configured-server list (status, enable toggle,
 * reconnect/remove, per-tool allowlist) plus the add-server form. Servers
 * marked `enabled` are connected automatically at app startup (see
 * `App.tsx`'s boot effect) — this tab is for reviewing/curating what's
 * already running as much as it is for adding new ones.
 */
export function McpPanel() {
  const { t } = useT();
  const servers = useMcpStore((s) => s.servers);

  const totalCachedTools = servers
    .filter((s) => s.enabled && s.status === "connected")
    .reduce((sum, s) => sum + s.tools.length, 0);

  return (
    <div className="flex flex-col gap-3 p-2">
      <p className="text-xs text-muted">{t("McpPanel.description")}</p>
      <p className="rounded-md bg-warning-soft px-2 py-1.5 text-xs text-warning">{t("McpPanel.sideEffectsNotice")}</p>

      {totalCachedTools > TOOL_COUNT_WARNING_THRESHOLD && (
        <p className="rounded-md bg-warning-soft px-2 py-1.5 text-xs text-warning">
          {t("McpPanel.toolCountWarning", { count: totalCachedTools })}
        </p>
      )}

      {servers.length === 0 ? (
        <p className="px-1 text-xs text-faint">
          <Plug size={12} className="mr-1 inline-block align-text-bottom" />
          {t("McpPanel.emptyState")}
        </p>
      ) : (
        <div className="flex flex-col gap-2">
          {servers.map((server) => (
            <ServerRow key={server.id} server={server} />
          ))}
        </div>
      )}

      <AddMcpServerForm />
    </div>
  );
}

export default McpPanel;
