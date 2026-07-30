import { useEffect, useState } from "react";
import {
  ChevronDown,
  Cloud,
  GitPullRequest,
  Plug,
  RefreshCw,
  Terminal,
  Trash2,
  type LucideIcon,
} from "lucide-react";
import { Button, StatusPill, type PillTone } from "../ui";
import {
  mcpServerNeedsAuthentication,
  useMcpStore,
  type McpOAuthPhase,
  type McpServerEntry,
  type McpServerInfo,
  type McpStatus,
} from "../../store/mcpStore";
import { useT } from "../../lib/i18n";
import { AddMcpServerForm, type McpServerDraft } from "./AddMcpServerForm";

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

interface AppConnectorTemplate {
  id: string;
  labelKey: string;
  descriptionKey: string;
  badgeKey: string;
  detailKey: string;
  icon: LucideIcon;
  draft: McpServerDraft;
  /** Only offered on macOS — hidden from the template grid everywhere else
   * (see `isMacPlatform`). */
  macOnly?: boolean;
  /** When set, clicking "Use template" first stages this bundled MCP
   * server's embedded source under the app data directory (see
   * `mcpStore.stageBundledServer` / `src-tauri/src/bundled_mcp_servers.rs`)
   * and fills the resulting absolute path in as the draft's sole arg,
   * rather than using `draft.argsText` verbatim. */
  stageBundledServerId?: string;
}

/** Crude but standard best-effort client-side OS check (no IPC round trip
 * needed, unlike `desktop_control.rs`'s Rust-side platform gating) — used
 * only to hide macOS-only templates from the grid on other platforms; the
 * bundled AppleScript server itself also refuses at call time on non-macOS,
 * so this is a UX nicety, not the actual safety boundary. */
function isMacPlatform(): boolean {
  if (typeof navigator === "undefined") return false;
  return /mac/i.test(navigator.platform || navigator.userAgent || "");
}

export const APP_CONNECTOR_TEMPLATES: AppConnectorTemplate[] = [
  {
    id: "github",
    labelKey: "McpPanel.templateGitHubLabel",
    descriptionKey: "McpPanel.templateGitHubDescription",
    badgeKey: "McpPanel.templateLocalBadge",
    detailKey: "McpPanel.templateGitHubDetail",
    icon: GitPullRequest,
    draft: {
      transportKind: "stdio",
      label: "GitHub",
      command: "docker",
      argsText: [
        "run",
        "-i",
        "--rm",
        "-e",
        "GITHUB_PERSONAL_ACCESS_TOKEN",
        "-e",
        "GITHUB_TOOLSETS",
        "-e",
        "GITHUB_READ_ONLY",
        "ghcr.io/github/github-mcp-server",
      ].join("\n"),
      env: {
        GITHUB_TOOLSETS: "repos,issues,pull_requests,actions",
        GITHUB_READ_ONLY: "1",
      },
      timeoutText: "90",
    },
  },
  {
    id: "custom-http",
    labelKey: "McpPanel.templateCustomHttpLabel",
    descriptionKey: "McpPanel.templateCustomHttpDescription",
    badgeKey: "McpPanel.templateRemoteBadge",
    detailKey: "McpPanel.templateCustomHttpDetail",
    icon: Cloud,
    draft: {
      transportKind: "http",
      label: "Custom app",
      url: "",
      timeoutText: "60",
    },
  },
  {
    id: "osascript-control",
    labelKey: "McpPanel.templateAppleScriptLabel",
    descriptionKey: "McpPanel.templateAppleScriptDescription",
    badgeKey: "McpPanel.templateLocalBadge",
    detailKey: "McpPanel.templateAppleScriptDetail",
    icon: Terminal,
    macOnly: true,
    stageBundledServerId: "osascript-control",
    draft: {
      transportKind: "stdio",
      label: "macOS Control (AppleScript)",
      command: "node",
      // Filled in with the staged server's absolute path right before the
      // draft is applied — see `stageBundledServerId` above.
      argsText: "",
      timeoutText: "30",
    },
  },
];

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

/** Status-pill tone per live `mcp-oauth://status` phase — mirrors
 * `STATUS_TONE`'s role for `McpStatus`. `"idle"` never reaches this (the
 * pill is only rendered once `oauthStatus[server.id]` exists). */
const OAUTH_PHASE_TONE: Partial<Record<McpOAuthPhase, PillTone>> = {
  discovering: "warning",
  needs_client_id: "warning",
  opening_browser: "warning",
  waiting_for_browser: "warning",
  exchanging_token: "warning",
  connected: "success",
  error: "danger",
  cancelled: "neutral",
};

/** Keep a user-supplied OAuth client editable after its authorization attempt
 * fails, without opening the same form for an unrelated DCR/CIMD failure. */
export function shouldShowManualOAuthClientFields(
  phase: McpOAuthPhase,
  manualClientWasPrompted: boolean,
): boolean {
  return phase === "needs_client_id" || (phase === "error" && manualClientWasPrompted);
}

/**
 * Generic MCP-spec OAuth 2.0 "Connect via OAuth" action for one HTTP
 * server's connection-settings disclosure (see `ServerRow`) — an
 * alternative to (not a replacement for) the manual bearer-token field right
 * above it. Streams live phase transitions from `mcpStore.oauthStatus` (fed
 * by `src-tauri/src/mcp_oauth.rs`'s `mcp-oauth://status` events): opening the
 * browser, waiting for it, exchanging the code, or — for a server whose
 * authorization server doesn't support dynamic client registration (e.g.
 * Google's) — prompting for a client id from the user's own OAuth app
 * registration and retrying.
 */
function OAuthConnectSection({ server }: { server: McpServerInfo }) {
  const { t } = useT();
  const oauthConnect = useMcpStore((s) => s.oauthConnect);
  const oauthCancel = useMcpStore((s) => s.oauthCancel);
  const oauthDisconnect = useMcpStore((s) => s.oauthDisconnect);
  const oauthRedirectUri = useMcpStore((s) => s.oauthRedirectUri);
  const connect = useMcpStore((s) => s.connect);
  const phaseInfo = useMcpStore((s) => s.oauthStatus[server.id]);

  const [clientIdInput, setClientIdInput] = useState("");
  const [clientSecretInput, setClientSecretInput] = useState("");
  const [redirectUri, setRedirectUri] = useState<string | null>(null);
  const [connecting, setConnecting] = useState(false);
  const [disconnecting, setDisconnecting] = useState(false);
  const [disconnectError, setDisconnectError] = useState<string | null>(null);
  const [manualClientWasPrompted, setManualClientWasPrompted] = useState(false);

  const phase: McpOAuthPhase = phaseInfo?.phase ?? "idle";
  const showManualClientFields = shouldShowManualOAuthClientFields(phase, manualClientWasPrompted);

  useEffect(() => {
    if (phase === "needs_client_id") setManualClientWasPrompted(true);
  }, [phase]);

  // Providers without dynamic client registration usually require the redirect
  // URI to be registered alongside the client (Google Web-application clients
  // and every Slack app do), and a mismatch fails in the browser with an opaque
  // provider error page rather than anywhere in this app — so show the exact
  // URI to register at the moment the user is being asked for a client id.
  useEffect(() => {
    if (phase !== "needs_client_id" || redirectUri !== null) return;
    void oauthRedirectUri(server.id)
      .then(setRedirectUri)
      .catch(() => {});
  }, [phase, redirectUri, oauthRedirectUri, server.id]);

  async function handleConnect(clientId?: string, clientSecret?: string) {
    setDisconnectError(null);
    setConnecting(true);
    try {
      await oauthConnect(server.id, clientId, clientSecret);
      // `mcp_oauth_connect` only saves credentials to the keychain — it
      // never itself connects/reconnects the MCP server (see that command's
      // own doc comment), so the server's top-level status pill would
      // otherwise stay "disconnected" until the user separately notices and
      // hits the unrelated Reconnect button. Follow success with the normal
      // connect, same as `AddMcpServerForm`'s add-then-connect flow; a
      // failure here surfaces via the server row's own status pill
      // (`mcp://status` -> "error"), nothing further to do.
      await connect(server.id).catch(() => {});
    } catch {
      // Surfaced via `phaseInfo.error`/the status pill already — the
      // `mcp-oauth://status` event that caused this rejection landed first.
    } finally {
      setConnecting(false);
    }
  }

  async function handleDisconnect() {
    setDisconnecting(true);
    setDisconnectError(null);
    try {
      await oauthDisconnect(server.id);
    } catch (error) {
      setDisconnectError(error instanceof Error ? error.message : String(error));
    } finally {
      setDisconnecting(false);
    }
  }

  if (server.hasOauth) {
    return (
      <div className="flex flex-col gap-1.5">
        <div className="flex flex-wrap items-center gap-1.5">
          <span className="font-mono text-xs text-muted">{t("McpPanel.oauthConnected")}</span>
          <Button
            variant="ghost"
            size="sm"
            onClick={() => void handleConnect()}
            disabled={connecting || disconnecting}
          >
            <RefreshCw size={12} className={connecting ? "animate-spin" : ""} />
            {connecting ? t("McpPanel.oauthReauthorizingButton") : t("McpPanel.oauthReauthorizeButton")}
          </Button>
          {connecting && (
            <Button variant="ghost" size="sm" onClick={() => void oauthCancel(server.id)}>
              {t("McpPanel.oauthCancelButton")}
            </Button>
          )}
          <Button
            variant="ghost"
            size="sm"
            onClick={() => void handleDisconnect()}
            disabled={connecting || disconnecting}
            className="text-danger hover:bg-danger-soft"
          >
            {disconnecting ? t("McpPanel.oauthDisconnectingButton") : t("McpPanel.oauthDisconnectButton")}
          </Button>
          {phase !== "idle" && phase !== "connected" && (
            <StatusPill tone={OAUTH_PHASE_TONE[phase] ?? "neutral"}>{t(`McpPanel.oauthPhase_${phase}`)}</StatusPill>
          )}
        </div>
        {phase === "error" && phaseInfo?.error && <p className="text-xs text-danger">{lastErrorLine(phaseInfo.error)}</p>}
        {disconnectError && <p className="text-xs text-danger">{lastErrorLine(disconnectError)}</p>}
      </div>
    );
  }

  return (
    <div className="flex flex-col gap-1.5">
      <div className="flex flex-wrap items-center gap-1.5">
        {!showManualClientFields && (
          <Button
            variant="ghost"
            size="sm"
            onClick={() => void handleConnect()}
            disabled={connecting}
          >
            {connecting ? t("McpPanel.oauthConnectingButton") : t("McpPanel.oauthConnectButton")}
          </Button>
        )}
        {connecting && (
          <Button variant="ghost" size="sm" onClick={() => void oauthCancel(server.id)}>
            {t("McpPanel.oauthCancelButton")}
          </Button>
        )}
        {phase !== "idle" && (
          <StatusPill tone={OAUTH_PHASE_TONE[phase] ?? "neutral"}>{t(`McpPanel.oauthPhase_${phase}`)}</StatusPill>
        )}
      </div>
      {showManualClientFields && (
        <div className="flex flex-col gap-1">
          <div className="flex items-center gap-1.5">
            <input
              type="text"
              value={clientIdInput}
              onChange={(event) => setClientIdInput(event.target.value)}
              placeholder={t("McpPanel.oauthClientIdPlaceholder")}
              className="h-7 min-w-0 flex-1 rounded-md border border-border bg-surface px-2 font-mono text-xs text-foreground placeholder:font-sans placeholder:text-faint focus:outline-none focus:ring-2 focus:ring-accent"
            />
            {/* Confidential clients authenticate at the token endpoint and
                require this secret. Public clients send no secret, but the
                provider must explicitly support and enable PKCE for that app. */}
            <input
              type="password"
              value={clientSecretInput}
              onChange={(event) => setClientSecretInput(event.target.value)}
              placeholder={t("McpPanel.oauthClientSecretPlaceholder")}
              autoComplete="off"
              className="h-7 min-w-0 flex-1 rounded-md border border-border bg-surface px-2 font-mono text-xs text-foreground placeholder:font-sans placeholder:text-faint focus:outline-none focus:ring-2 focus:ring-accent"
            />
            <Button
              variant="ghost"
              size="sm"
              onClick={() => void handleConnect(clientIdInput.trim(), clientSecretInput.trim() || undefined)}
              disabled={!clientIdInput.trim() || connecting}
            >
              {phase === "error" ? t("McpPanel.oauthRetryButton") : t("McpPanel.oauthContinueButton")}
            </Button>
          </div>
          {redirectUri && (
            <div className="flex items-center gap-1.5">
              <span className="shrink-0 text-[11px] text-faint">{t("McpPanel.oauthRedirectUriLabel")}</span>
              <input
                type="text"
                value={redirectUri}
                readOnly
                onFocus={(event) => event.currentTarget.select()}
                className="h-7 min-w-0 flex-1 rounded-md border border-border bg-surface-2 px-2 font-mono text-xs text-foreground focus:outline-none focus:ring-2 focus:ring-accent"
              />
            </div>
          )}
          <p className="text-[11px] text-faint">{t("McpPanel.oauthClientIdHint")}</p>
        </div>
      )}
      {phase === "error" && phaseInfo?.error && <p className="text-xs text-danger">{lastErrorLine(phaseInfo.error)}</p>}
    </div>
  );
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

  // Authorization is optional for HTTP MCP servers. Only show this warning
  // after the backend has observed an auth-shaped connection error; transport
  // plus missing credentials alone would incorrectly flag public endpoints.
  const needsAuth = mcpServerNeedsAuthentication(server);

  return (
    <div className="rounded-lg border border-border bg-background p-3">
      <div className="flex items-center gap-2">
        <span className="truncate text-sm font-medium text-foreground">{server.label}</span>
        <StatusPill tone={STATUS_TONE[server.status]}>{t(`McpPanel.status_${server.status}`)}</StatusPill>
        {needsAuth && (
          <span title={t("McpPanel.needsAuthPillTitle")}>
            <StatusPill tone="warning">{t("McpPanel.needsAuthPill")}</StatusPill>
          </span>
        )}
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
            <div className="flex flex-col gap-2">
              <div className="flex flex-col gap-1">
                <span className="text-xs font-medium text-faint">{t("McpPanel.oauthSectionHeading")}</span>
                <OAuthConnectSection server={server} />
              </div>

              <div className="flex flex-col gap-1 border-t border-border pt-2">
                <span className="text-xs font-medium text-faint">{t("McpPanel.tokenSectionHeading")}</span>
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
  const stageBundledServer = useMcpStore((s) => s.stageBundledServer);
  const [draft, setDraft] = useState<McpServerDraft | null>(null);
  const [draftVersion, setDraftVersion] = useState(0);
  const [templateError, setTemplateError] = useState<string | null>(null);
  const [stagingTemplateId, setStagingTemplateId] = useState<string | null>(null);

  const totalCachedTools = servers
    .filter((s) => s.enabled && s.status === "connected")
    .reduce((sum, s) => sum + s.tools.length, 0);

  const visibleTemplates = APP_CONNECTOR_TEMPLATES.filter(
    (template) => !template.macOnly || isMacPlatform()
  );

  async function useTemplate(template: AppConnectorTemplate) {
    setTemplateError(null);
    let draftPatch: Partial<McpServerDraft> = {};
    if (template.stageBundledServerId) {
      setStagingTemplateId(template.id);
      try {
        const path = await stageBundledServer(template.stageBundledServerId);
        draftPatch = { argsText: path };
      } catch (err) {
        setTemplateError(
          t("McpPanel.templateAppleScriptStagingError", {
            error: err instanceof Error ? err.message : String(err),
          })
        );
        return;
      } finally {
        setStagingTemplateId(null);
      }
    }
    setDraft({
      ...template.draft,
      ...draftPatch,
      env: template.draft.env ? { ...template.draft.env } : undefined,
    });
    setDraftVersion((version) => version + 1);
  }

  return (
    <div className="flex flex-col gap-3 py-2">
      <p className="text-xs text-muted">{t("McpPanel.description")}</p>
      <p className="rounded-md bg-warning-soft px-2 py-1.5 text-xs text-warning">{t("McpPanel.sideEffectsNotice")}</p>

      <section className="rounded-lg border border-border bg-surface p-3">
        <div className="flex flex-wrap items-start justify-between gap-3">
          <div className="min-w-0">
            <h3 className="text-sm font-semibold text-foreground">{t("McpPanel.appConnectorsHeading")}</h3>
            <p className="mt-1 text-xs leading-5 text-muted">{t("McpPanel.appConnectorsDescription")}</p>
          </div>
        </div>
        <div className="mt-3 grid gap-2 lg:grid-cols-2">
          {visibleTemplates.map((template) => {
            const Icon = template.icon;
            const staging = stagingTemplateId === template.id;
            return (
              <article key={template.id} className="rounded-lg border border-border bg-background p-3">
                <div className="flex items-start gap-3">
                  <span className="flex h-9 w-9 shrink-0 items-center justify-center rounded-md bg-surface-2 text-muted">
                    <Icon size={17} />
                  </span>
                  <div className="min-w-0 flex-1">
                    <div className="flex flex-wrap items-center gap-2">
                      <h4 className="text-sm font-semibold text-foreground">{t(template.labelKey)}</h4>
                      <span className="rounded-md bg-surface-2 px-1.5 py-0.5 text-[11px] font-medium text-faint">
                        {t(template.badgeKey)}
                      </span>
                    </div>
                    <p className="mt-1 text-xs leading-5 text-muted">{t(template.descriptionKey)}</p>
                    <p className="mt-2 text-[11px] leading-4 text-faint">{t(template.detailKey)}</p>
                  </div>
                </div>
                <div className="mt-3 flex justify-end">
                  <Button size="sm" onClick={() => void useTemplate(template)} disabled={staging}>
                    {staging ? t("McpPanel.useTemplatePreparingButton") : t("McpPanel.useTemplateButton")}
                  </Button>
                </div>
              </article>
            );
          })}
        </div>
        {templateError && <p className="mt-2 text-xs text-danger">{templateError}</p>}
      </section>

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

      <AddMcpServerForm draft={draft} draftVersion={draftVersion} />
    </div>
  );
}

export default McpPanel;
