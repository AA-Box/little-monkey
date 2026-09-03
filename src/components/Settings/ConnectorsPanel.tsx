import { useEffect, useRef, useState } from "react";
import {
  Archive,
  Box as BoxIcon,
  Check,
  ChevronDown,
  CircleDot,
  Cloud,
  CreditCard,
  Database,
  DollarSign,
  FileSignature,
  FolderOpen,
  GitBranch,
  GitPullRequest,
  HardDrive,
  LifeBuoy,
  LineChart,
  ListChecks,
  Mail,
  MessageCircle,
  MessageSquare,
  NotebookText,
  Palette,
  Puzzle,
  Receipt,
  RefreshCw,
  Search,
  SlidersHorizontal,
  Table,
  Ticket,
  Trash2,
  Users,
  Wallet,
  type LucideIcon,
} from "lucide-react";
import { Button, StatusPill, type PillTone } from "../ui";
import {
  CONNECTOR_OAUTH_TERMINAL,
  useConnectorsStore,
  type ConnectorAccount,
  type ConnectorAuditEntry,
  type ConnectorOAuthPhase,
  type ConnectorOAuthProvider,
  type ConnectorProvider,
} from "../../store/connectorsStore";
import { useMcpStore, type McpOAuthPhase } from "../../store/mcpStore";
import { useT } from "../../lib/i18n";
import { McpPanel, shouldShowManualOAuthClientFields } from "./McpPanel";
import { errorMessage } from "../../lib/errors";

const PROVIDER_ICONS: Record<ConnectorProvider, LucideIcon> = {
  github: GitPullRequest,
  slack: MessageCircle,
  notion: NotebookText,
  jira: Ticket,
  s3: Database,
  extension: Puzzle,
  google_drive: HardDrive,
  microsoft_graph: Cloud,
  linear: ListChecks,
  asana: CircleDot,
  dropbox: FolderOpen,
  box: BoxIcon,
  airtable: Table,
  zendesk: LifeBuoy,
  hubspot: Users,
  discord: MessageSquare,
  gitlab: GitBranch,
};

const PROVIDER_LABEL_KEYS: Record<ConnectorProvider, string> = {
  github: "ConnectorsPanel.providerGithub",
  slack: "ConnectorsPanel.providerSlack",
  notion: "ConnectorsPanel.providerNotion",
  jira: "ConnectorsPanel.providerJira",
  s3: "ConnectorsPanel.providerS3",
  extension: "ConnectorsPanel.providerExtension",
  google_drive: "ConnectorsPanel.providerGoogleDrive",
  microsoft_graph: "ConnectorsPanel.providerMicrosoftGraph",
  linear: "ConnectorsPanel.providerLinear",
  asana: "ConnectorsPanel.providerAsana",
  dropbox: "ConnectorsPanel.providerDropbox",
  box: "ConnectorsPanel.providerBox",
  airtable: "ConnectorsPanel.providerAirtable",
  zendesk: "ConnectorsPanel.providerZendesk",
  hubspot: "ConnectorsPanel.providerHubspot",
  discord: "ConnectorsPanel.providerDiscord",
  gitlab: "ConnectorsPanel.providerGitlab",
};

interface TokenProviderInfo {
  provider: "slack" | "notion" | "jira";
  scopes: string[];
  copyKey: string;
  tokenPlaceholderKey: string;
}

const TOKEN_PROVIDERS: TokenProviderInfo[] = [
  {
    provider: "slack",
    scopes: ["channels:read", "channels:history", "chat:write"],
    copyKey: "ConnectorsPanel.slackCopy",
    tokenPlaceholderKey: "ConnectorsPanel.slackTokenPlaceholder",
  },
  {
    provider: "notion",
    scopes: ["read_content", "read_comments"],
    copyKey: "ConnectorsPanel.notionCopy",
    tokenPlaceholderKey: "ConnectorsPanel.notionTokenPlaceholder",
  },
  {
    provider: "jira",
    scopes: ["read:jira-work", "read:confluence-content"],
    copyKey: "ConnectorsPanel.jiraCopy",
    tokenPlaceholderKey: "ConnectorsPanel.jiraTokenPlaceholder",
  },
];

/** The user-facing half of `connector_oauth.rs`'s `OAUTH_PROVIDERS` table.
 * Deliberately NOT a second source of truth for endpoints, scopes or PKCE —
 * only what the card has to render. */
interface OAuthProviderInfo {
  provider: ConnectorOAuthProvider;
  copyKey: string;
  /** Mirrors `connector_oauth::SecretPolicy`: `required` — the token endpoint
   * refuses a request without a client secret; `optional` — accepted either
   * way, depending on how the app was registered; `never` — the provider
   * registers public clients only and its token endpoint rejects a secret. */
  secret: "required" | "optional" | "never";
  /** Placeholder key for the instance-host / tenant field, when the provider
   * needs one. */
  hostPlaceholderKey?: string;
  /** True when `connector_oauth.rs` has no default for that field (Zendesk's
   * `ApiHost { default: None }`), so a blank one is refused by the backend.
   * Catching it here keeps the failure in the form instead of a red pill. */
  hostRequired?: boolean;
}

export const OAUTH_PROVIDERS: OAuthProviderInfo[] = [
  { provider: "google_drive", copyKey: "ConnectorsPanel.googleDriveCopy", secret: "required" },
  {
    provider: "microsoft_graph",
    copyKey: "ConnectorsPanel.microsoftGraphCopy",
    secret: "never",
    hostPlaceholderKey: "ConnectorsPanel.oauthHostPlaceholderMicrosoft",
  },
  { provider: "linear", copyKey: "ConnectorsPanel.linearCopy", secret: "required" },
  { provider: "asana", copyKey: "ConnectorsPanel.asanaCopy", secret: "required" },
  { provider: "dropbox", copyKey: "ConnectorsPanel.dropboxCopy", secret: "optional" },
  { provider: "box", copyKey: "ConnectorsPanel.boxCopy", secret: "required" },
  { provider: "airtable", copyKey: "ConnectorsPanel.airtableCopy", secret: "optional" },
  {
    provider: "zendesk",
    copyKey: "ConnectorsPanel.zendeskCopy",
    secret: "optional",
    hostPlaceholderKey: "ConnectorsPanel.oauthHostPlaceholderZendesk",
    hostRequired: true,
  },
  { provider: "hubspot", copyKey: "ConnectorsPanel.hubspotCopy", secret: "required" },
  { provider: "discord", copyKey: "ConnectorsPanel.discordCopy", secret: "required" },
  {
    provider: "gitlab",
    copyKey: "ConnectorsPanel.gitlabCopy",
    secret: "optional",
    hostPlaceholderKey: "ConnectorsPanel.oauthHostPlaceholderGitlab",
  },
];

const CONNECTOR_CATEGORIES = [
  { id: "devTools", labelKey: "ConnectorsPanel.categoryDevTools" },
  { id: "communication", labelKey: "ConnectorsPanel.categoryCommunication" },
  { id: "productivity", labelKey: "ConnectorsPanel.categoryProductivity" },
  { id: "cloudStorage", labelKey: "ConnectorsPanel.categoryCloudStorage" },
  { id: "creative", labelKey: "ConnectorsPanel.categoryCreative" },
  { id: "commerce", labelKey: "ConnectorsPanel.categoryCommerce" },
  { id: "financial", labelKey: "ConnectorsPanel.categoryFinancial" },
  { id: "dataAnalytics", labelKey: "ConnectorsPanel.categoryDataAnalytics" },
  { id: "crm", labelKey: "ConnectorsPanel.categoryCrm" },
  { id: "legalDocs", labelKey: "ConnectorsPanel.categoryLegalDocs" },
] as const;

type AppConnectorCategory = (typeof CONNECTOR_CATEGORIES)[number]["id"];

interface AppConnectorTemplate {
  id: string;
  labelKey: string;
  descriptionKey: string;
  icon: LucideIcon;
  category: AppConnectorCategory;
  /** Verified hosted MCP endpoint — empty for providers we don't have a
   * confirmed URL for yet (see `needsUrlInput` in `AppConnectorCard`): those
   * prompt for it once on first connect instead of guessing a URL that
   * might not exist. */
  url: string;
  /** False only for Figma's local Dev Mode server, which isn't an OAuth
   * server at all (auth is implicit in being signed into the Figma desktop
   * app) — running the generic MCP OAuth flow against it just surfaces a
   * misleading "needs client id" error from `rmcp`'s legacy-fallback DCR
   * guess. Every other catalog entry is a real remote OAuth server. */
  requiresOAuth?: boolean;
  /** Set for Slack/Google Drive/Gmail — these providers don't support RFC 7591
   * dynamic client registration (confirmed against their own docs), so
   * connecting them can't be as one-click as Notion/Stripe/PostHog/Atlassian:
   * the generic `mcp_oauth.rs` flow asks for the credentials of an OAuth app
   * the user registers themselves, once, and keeps them in their keychain for
   * later reconnects. Confidential clients use an id and secret; a public
   * client may omit the secret only when its provider explicitly supports and
   * enables PKCE. `docs/byo-oauth-clients.md` walks through it, and
   * `appByoClientHint` says so on the card.
   *
   * This app can't do better than that without holding OAuth client secrets,
   * which a public open-source binary can't keep secret from anyone who
   * downloads it. (`hosted_oauth.rs` brokers exactly that through a server for
   * builds that run one — it's deliberately not wired up here; see its module
   * doc.) */
  authMode?: "byoClient";
}

/** The "browse and one-click connect" catalog (mirrors the desktop app's
 * Connectors directory) — distinct from the "Connect a new account" section
 * above, which manages `connectorsStore`'s gh-CLI/manual-token accounts.
 * Every card here is backed by a real MCP server entry in `mcpStore` and
 * connects via the generic MCP-spec OAuth flow (`mcpStore.oauthConnect`),
 * same mechanism `McpPanel`'s per-server "Connect via OAuth" uses. */
const APP_CONNECTORS: AppConnectorTemplate[] = [
  { id: "slack", labelKey: "ConnectorsPanel.appSlackLabel", descriptionKey: "ConnectorsPanel.appSlackDescription", icon: MessageCircle, category: "communication", url: "https://mcp.slack.com/mcp", authMode: "byoClient" },
  { id: "atlassian", labelKey: "ConnectorsPanel.appAtlassianLabel", descriptionKey: "ConnectorsPanel.appAtlassianDescription", icon: Ticket, category: "devTools", url: "https://mcp.atlassian.com/v1/mcp/authv2" },
  { id: "google-drive", labelKey: "ConnectorsPanel.appGoogleDriveLabel", descriptionKey: "ConnectorsPanel.appGoogleDriveDescription", icon: HardDrive, category: "cloudStorage", url: "https://drivemcp.googleapis.com/mcp/v1", authMode: "byoClient" },
  { id: "gmail", labelKey: "ConnectorsPanel.appGmailLabel", descriptionKey: "ConnectorsPanel.appGmailDescription", icon: Mail, category: "communication", url: "https://gmailmcp.googleapis.com/mcp/v1", authMode: "byoClient" },
  { id: "figma", labelKey: "ConnectorsPanel.appFigmaLabel", descriptionKey: "ConnectorsPanel.appFigmaDescription", icon: Palette, category: "creative", url: "http://127.0.0.1:3845/mcp", requiresOAuth: false },
  { id: "notion", labelKey: "ConnectorsPanel.appNotionLabel", descriptionKey: "ConnectorsPanel.appNotionDescription", icon: NotebookText, category: "productivity", url: "https://mcp.notion.com/mcp" },
  { id: "linear", labelKey: "ConnectorsPanel.appLinearLabel", descriptionKey: "ConnectorsPanel.appLinearDescription", icon: ListChecks, category: "devTools", url: "" },
  { id: "asana", labelKey: "ConnectorsPanel.appAsanaLabel", descriptionKey: "ConnectorsPanel.appAsanaDescription", icon: ListChecks, category: "productivity", url: "" },
  { id: "box", labelKey: "ConnectorsPanel.appBoxLabel", descriptionKey: "ConnectorsPanel.appBoxDescription", icon: BoxIcon, category: "cloudStorage", url: "" },
  { id: "dropbox", labelKey: "ConnectorsPanel.appDropboxLabel", descriptionKey: "ConnectorsPanel.appDropboxDescription", icon: FolderOpen, category: "cloudStorage", url: "" },
  { id: "square", labelKey: "ConnectorsPanel.appSquareLabel", descriptionKey: "ConnectorsPanel.appSquareDescription", icon: CreditCard, category: "commerce", url: "" },
  { id: "stripe", labelKey: "ConnectorsPanel.appStripeLabel", descriptionKey: "ConnectorsPanel.appStripeDescription", icon: DollarSign, category: "financial", url: "https://mcp.stripe.com" },
  { id: "paypal", labelKey: "ConnectorsPanel.appPaypalLabel", descriptionKey: "ConnectorsPanel.appPaypalDescription", icon: Wallet, category: "financial", url: "" },
  { id: "quickbooks", labelKey: "ConnectorsPanel.appQuickbooksLabel", descriptionKey: "ConnectorsPanel.appQuickbooksDescription", icon: Receipt, category: "financial", url: "" },
  { id: "posthog", labelKey: "ConnectorsPanel.appPosthogLabel", descriptionKey: "ConnectorsPanel.appPosthogDescription", icon: LineChart, category: "dataAnalytics", url: "https://mcp.posthog.com/mcp" },
  { id: "hubspot", labelKey: "ConnectorsPanel.appHubspotLabel", descriptionKey: "ConnectorsPanel.appHubspotDescription", icon: Users, category: "crm", url: "" },
  { id: "docusign", labelKey: "ConnectorsPanel.appDocusignLabel", descriptionKey: "ConnectorsPanel.appDocusignDescription", icon: FileSignature, category: "legalDocs", url: "" },
  { id: "egnyte", labelKey: "ConnectorsPanel.appEgnyteLabel", descriptionKey: "ConnectorsPanel.appEgnyteDescription", icon: Archive, category: "legalDocs", url: "" },
];

const APP_OAUTH_PHASE_TONE: Partial<Record<McpOAuthPhase, PillTone>> = {
  discovering: "warning",
  needs_client_id: "warning",
  opening_browser: "warning",
  waiting_for_browser: "warning",
  exchanging_token: "warning",
  connected: "success",
  error: "danger",
  cancelled: "neutral",
};

/** Separate from `APP_OAUTH_PHASE_TONE` because `verifying` is not an
 * `McpOAuthPhase` — the catalog flow proves the account with a live identity
 * call the MCP flow has no equivalent of. */
const CONNECTOR_OAUTH_PHASE_TONE: Partial<Record<ConnectorOAuthPhase, PillTone>> = {
  needs_client_id: "warning",
  opening_browser: "warning",
  waiting_for_browser: "warning",
  exchanging_token: "warning",
  verifying: "warning",
  connected: "success",
  error: "danger",
  cancelled: "neutral",
};

/** Terminal phases: the Connect button is usable again and Cancel is not.
 * `idle` plus the store's own terminal set, so the two cannot drift. */
export const CONNECTOR_OAUTH_DONE: ConnectorOAuthPhase[] = ["idle", ...CONNECTOR_OAUTH_TERMINAL];

function lastAppErrorLine(message: string): string {
  const lines = message.trim().split("\n").filter((line) => line.trim().length > 0);
  return lines.length > 0 ? lines[lines.length - 1] : message;
}

/** One catalog card: adds the backing MCP server on first connect (using
 * the template's verified URL, or a once-pasted one — see `urlInput`), then
 * runs the same generic MCP-spec OAuth flow as `McpPanel`'s
 * `OAuthConnectSection`, entirely within this one button. */
function AppConnectorCard({ template }: { template: AppConnectorTemplate }) {
  const { t } = useT();
  const servers = useMcpStore((s) => s.servers);
  const addServer = useMcpStore((s) => s.addServer);
  const oauthConnect = useMcpStore((s) => s.oauthConnect);
  const oauthCancel = useMcpStore((s) => s.oauthCancel);
  const oauthDisconnect = useMcpStore((s) => s.oauthDisconnect);
  const oauthRedirectUri = useMcpStore((s) => s.oauthRedirectUri);
  const connect = useMcpStore((s) => s.connect);
  const disconnect = useMcpStore((s) => s.disconnect);
  const phaseInfo = useMcpStore((s) => s.oauthStatus[template.id]);

  const [urlInput, setUrlInput] = useState("");
  const [clientIdInput, setClientIdInput] = useState("");
  const [clientSecretInput, setClientSecretInput] = useState("");
  const [redirectUri, setRedirectUri] = useState<string | null>(null);
  const [connecting, setConnecting] = useState(false);
  const [disconnecting, setDisconnecting] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [manualClientWasPrompted, setManualClientWasPrompted] = useState(false);

  const Icon = template.icon;
  const needsOwnClient = template.authMode === "byoClient";
  const usesOAuth = template.requiresOAuth !== false;
  const server = servers.find((s) => s.id === template.id);
  const phase: McpOAuthPhase = phaseInfo?.phase ?? "idle";
  const needsUrlInput = !server && template.url.length === 0;
  const hasOauth = Boolean(server?.hasOauth);
  const showManualClientFields = shouldShowManualOAuthClientFields(phase, manualClientWasPrompted);
  // Saved credentials and a live transport are separate states. A credential
  // surviving a failed keychain removal must not leave this card claiming the
  // server is still connected.
  const isConnected = server?.status === "connected";

  useEffect(() => {
    if (phase === "needs_client_id") setManualClientWasPrompted(true);
  }, [phase]);

  // Same reason as `McpPanel`'s `OAuthConnectSection`: providers that want a
  // client id usually want its redirect URI registered too, and getting that
  // wrong fails on the provider's own error page, not in this app.
  useEffect(() => {
    if (phase !== "needs_client_id" || redirectUri !== null) return;
    void oauthRedirectUri(template.id)
      .then(setRedirectUri)
      .catch(() => {});
  }, [phase, redirectUri, oauthRedirectUri, template.id]);

  async function handleConnect(clientId?: string, clientSecret?: string) {
    setError(null);
    setConnecting(true);
    try {
      if (!server) {
        const url = template.url || urlInput.trim();
        if (!url) return;
        await addServer({
          id: template.id,
          label: t(template.labelKey),
          transport: { type: "http", url },
          enabled: true,
          tool_allowlist: null,
          timeout_secs: 90,
        });
      }
      if (usesOAuth) {
        await oauthConnect(template.id, clientId, clientSecret);
        // Same reasoning as `OAuthConnectSection`: `mcp_oauth_connect` only
        // saves credentials, it doesn't itself (re)connect the server.
        await connect(template.id).catch(() => {});
      } else {
        // Figma's local Dev Mode server isn't an OAuth server — just connect
        // directly (see `requiresOAuth`'s doc comment).
        await connect(template.id);
      }
    } catch (err) {
      setError(errorMessage(err));
    } finally {
      setConnecting(false);
    }
  }

  async function handleDisconnect() {
    setDisconnecting(true);
    setError(null);
    try {
      if (usesOAuth) {
        await oauthDisconnect(template.id);
      } else {
        await disconnect(template.id);
      }
    } catch (err) {
      setError(errorMessage(err));
    } finally {
      setDisconnecting(false);
    }
  }

  return (
    <article className="rounded-lg border border-border bg-background p-3">
      <div className="flex items-start gap-3">
        <span className="flex h-9 w-9 shrink-0 items-center justify-center rounded-md bg-surface-2 text-muted">
          <Icon size={17} />
        </span>
        <div className="min-w-0 flex-1">
          <div className="flex flex-wrap items-center gap-2">
            <h4 className="text-sm font-semibold text-foreground">{t(template.labelKey)}</h4>
            <span className="rounded-md bg-surface-2 px-1.5 py-0.5 text-[11px] font-medium text-faint">
              {t(CONNECTOR_CATEGORIES.find((c) => c.id === template.category)?.labelKey ?? "ConnectorsPanel.categoryOther")}
            </span>
          </div>
          <p className="mt-1 text-xs leading-5 text-muted">{t(template.descriptionKey)}</p>
        </div>
      </div>

      {isConnected ? (
        <div className="mt-3 flex flex-col gap-1.5">
          <div className="flex flex-wrap items-center justify-between gap-2">
            <StatusPill tone="success">{t("ConnectorsPanel.appConnectedLabel")}</StatusPill>
            <div className="flex flex-wrap items-center justify-end gap-1.5">
              {usesOAuth && hasOauth && (
                <Button
                  variant="ghost"
                  size="sm"
                  onClick={() => void handleConnect()}
                  disabled={connecting || disconnecting}
                >
                  <RefreshCw size={12} className={connecting ? "animate-spin" : ""} />
                  {connecting ? t("ConnectorsPanel.appReauthorizingButton") : t("ConnectorsPanel.appReauthorizeButton")}
                </Button>
              )}
              {usesOAuth && connecting && (
                <Button variant="ghost" size="sm" onClick={() => void oauthCancel(template.id)}>
                  {t("ConnectorsPanel.appCancelButton")}
                </Button>
              )}
              <Button
                variant="ghost"
                size="sm"
                onClick={() => void handleDisconnect()}
                disabled={connecting || disconnecting}
                className="text-danger hover:bg-danger-soft"
              >
                {disconnecting ? t("ConnectorsPanel.appDisconnectingButton") : t("ConnectorsPanel.appDisconnectButton")}
              </Button>
            </div>
          </div>
          {usesOAuth && phase !== "idle" && phase !== "connected" && (
            <div>
              <StatusPill tone={APP_OAUTH_PHASE_TONE[phase] ?? "neutral"}>{t(`ConnectorsPanel.appOauthPhase_${phase}`)}</StatusPill>
            </div>
          )}
          {usesOAuth && phase === "error" && phaseInfo?.error && <p className="text-xs text-danger">{lastAppErrorLine(phaseInfo.error)}</p>}
          {error && <p className="text-xs text-danger">{error}</p>}
        </div>
      ) : (
        <div className="mt-3 flex flex-col gap-1.5">
          {needsUrlInput && (
            <input
              type="text"
              value={urlInput}
              onChange={(event) => setUrlInput(event.target.value)}
              placeholder={t("ConnectorsPanel.appUrlPlaceholder")}
              className="h-8 rounded-md border border-border bg-surface px-2.5 font-mono text-xs text-foreground placeholder:font-sans placeholder:text-faint focus:outline-none focus:ring-2 focus:ring-accent"
            />
          )}
          <div className="flex flex-wrap items-center justify-end gap-1.5">
            {usesOAuth && phase !== "idle" && (
              <StatusPill tone={APP_OAUTH_PHASE_TONE[phase] ?? "neutral"}>{t(`ConnectorsPanel.appOauthPhase_${phase}`)}</StatusPill>
            )}
            {usesOAuth && connecting && (
              <Button variant="ghost" size="sm" onClick={() => void oauthCancel(template.id)}>
                {t("ConnectorsPanel.appCancelButton")}
              </Button>
            )}
            {!showManualClientFields && (
              <Button
                size="sm"
                onClick={() => void handleConnect()}
                disabled={connecting || disconnecting || (needsUrlInput && !urlInput.trim())}
              >
                {disconnecting
                  ? t("ConnectorsPanel.appDisconnectingButton")
                  : connecting
                    ? hasOauth
                      ? t("ConnectorsPanel.appReauthorizingButton")
                      : t("ConnectorsPanel.appConnectingButton")
                    : needsUrlInput
                      ? t("ConnectorsPanel.appUrlSaveConnectButton")
                      : hasOauth
                        ? t("ConnectorsPanel.appReauthorizeButton")
                        : t("ConnectorsPanel.appConnectButton")}
              </Button>
            )}
          </div>
          {/* Said before the first click, not only after the flow bounces off a
              missing client id: these providers have no dynamic client
              registration, so "Connect" is a two-step process and the user
              needs to know a browser trip to their provider's console is part
              of it. */}
          {usesOAuth && needsOwnClient && !showManualClientFields && (
            <p className="text-[11px] text-faint">{t("ConnectorsPanel.appByoClientHint")}</p>
          )}
          {usesOAuth && showManualClientFields && (
            <div className="flex flex-col gap-1">
              <div className="flex flex-wrap items-center gap-1.5">
                <input
                  type="text"
                  value={clientIdInput}
                  onChange={(event) => setClientIdInput(event.target.value)}
                  placeholder={t("ConnectorsPanel.appClientIdPlaceholder")}
                  className="h-7 min-w-0 flex-1 rounded-md border border-border bg-surface px-2 font-mono text-xs text-foreground placeholder:font-sans placeholder:text-faint focus:outline-none focus:ring-2 focus:ring-accent"
                />
                {/* Confidential clients require a secret. A public client sends
                    none, but its provider must explicitly support and enable
                    PKCE for that app (including Slack's public-client mode). */}
                <input
                  type="password"
                  value={clientSecretInput}
                  onChange={(event) => setClientSecretInput(event.target.value)}
                  placeholder={t("ConnectorsPanel.appClientSecretPlaceholder")}
                  autoComplete="off"
                  className="h-7 min-w-0 flex-1 rounded-md border border-border bg-surface px-2 font-mono text-xs text-foreground placeholder:font-sans placeholder:text-faint focus:outline-none focus:ring-2 focus:ring-accent"
                />
                <Button
                  variant="ghost"
                  size="sm"
                  onClick={() => void handleConnect(clientIdInput.trim(), clientSecretInput.trim() || undefined)}
                  disabled={!clientIdInput.trim() || connecting}
                >
                  {phase === "error" ? t("ConnectorsPanel.appRetryButton") : t("ConnectorsPanel.appContinueButton")}
                </Button>
              </div>
              {redirectUri && (
                <div className="flex items-center gap-1.5">
                  <span className="shrink-0 text-[11px] text-faint">{t("ConnectorsPanel.appRedirectUriLabel")}</span>
                  <input
                    type="text"
                    value={redirectUri}
                    readOnly
                    onFocus={(event) => event.currentTarget.select()}
                    className="h-7 min-w-0 flex-1 rounded-md border border-border bg-surface-2 px-2 font-mono text-xs text-foreground focus:outline-none focus:ring-2 focus:ring-accent"
                  />
                </div>
              )}
              <p className="text-[11px] text-faint">{t("ConnectorsPanel.appClientIdHint")}</p>
            </div>
          )}
          {usesOAuth && phase === "error" && phaseInfo?.error && <p className="text-xs text-danger">{lastAppErrorLine(phaseInfo.error)}</p>}
          {error && <p className="text-xs text-danger">{error}</p>}
        </div>
      )}
    </article>
  );
}

function formatDate(ms: number): string {
  return new Date(ms).toLocaleString();
}

/** GitHub's "Connect via gh CLI" card: one button, no form — identity comes
 * entirely from the already-authenticated `gh` process, never a pasted
 * token. */
function GithubConnectCard() {
  const { t } = useT();
  const addGithub = useConnectorsStore((s) => s.addGithub);
  const [connecting, setConnecting] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [connected, setConnected] = useState<ConnectorAccount | null>(null);

  async function handleConnect() {
    setConnecting(true);
    setError(null);
    setConnected(null);
    try {
      const account = await addGithub();
      setConnected(account);
    } catch (err) {
      setError(errorMessage(err));
    } finally {
      setConnecting(false);
    }
  }

  return (
    <article className="rounded-lg border border-border bg-background p-3">
      <div className="flex items-start gap-3">
        <span className="flex h-9 w-9 shrink-0 items-center justify-center rounded-md bg-surface-2 text-muted">
          <GitPullRequest size={17} />
        </span>
        <div className="min-w-0 flex-1">
          <h4 className="text-sm font-semibold text-foreground">{t("ConnectorsPanel.providerGithub")}</h4>
          <p className="mt-1 text-xs leading-5 text-muted">{t("ConnectorsPanel.githubCopy")}</p>
        </div>
      </div>
      <div className="mt-3 flex items-center justify-between gap-2">
        <div className="min-w-0 flex-1 text-xs">
          {connected && (
            <p className="text-success">{t("ConnectorsPanel.githubConnectedAs", { login: connected.identity ?? "" })}</p>
          )}
          {error && <p className="text-danger">{error}</p>}
        </div>
        <Button size="sm" onClick={() => void handleConnect()} disabled={connecting} className="shrink-0">
          {connecting ? t("ConnectorsPanel.connectingButton") : t("ConnectorsPanel.githubConnectButton")}
        </Button>
      </div>
    </article>
  );
}

/** Slack/Notion/Jira's shared token form: capability/storage-location copy
 * shown up front, verify-then-save on submit (never saved on a failed
 * verification — see `connectors_add_token`). */
function TokenConnectForm({ info, onDone }: { info: TokenProviderInfo; onDone: () => void }) {
  const { t } = useT();
  const addToken = useConnectorsStore((s) => s.addToken);

  const [label, setLabel] = useState("");
  const [token, setToken] = useState("");
  const [email, setEmail] = useState("");
  const [siteUrl, setSiteUrl] = useState("");
  const [submitting, setSubmitting] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const needsJiraFields = info.provider === "jira";
  const canSubmit =
    label.trim().length > 0 &&
    token.trim().length > 0 &&
    (!needsJiraFields || (email.trim().length > 0 && siteUrl.trim().length > 0)) &&
    !submitting;

  async function handleSubmit() {
    if (!canSubmit) return;
    setSubmitting(true);
    setError(null);
    try {
      await addToken({
        provider: info.provider,
        label: label.trim(),
        token: token.trim(),
        scopes: info.scopes,
        email: needsJiraFields ? email.trim() : undefined,
        siteUrl: needsJiraFields ? siteUrl.trim() : undefined,
      });
      onDone();
    } catch (err) {
      setError(errorMessage(err));
    } finally {
      setSubmitting(false);
    }
  }

  return (
    <div className="rounded-lg border border-border bg-background p-3">
      <p className="rounded-md bg-surface-2 px-2.5 py-2 text-xs leading-5 text-muted">{t(info.copyKey)}</p>
      <div className="mt-3 flex flex-col gap-2">
        <input
          type="text"
          value={label}
          onChange={(event) => setLabel(event.target.value)}
          placeholder={t("ConnectorsPanel.labelPlaceholder")}
          className="h-8 rounded-md border border-border bg-surface px-2.5 text-sm text-foreground placeholder:text-faint focus:outline-none focus:ring-2 focus:ring-accent"
        />
        {needsJiraFields && (
          <>
            <input
              type="email"
              value={email}
              onChange={(event) => setEmail(event.target.value)}
              placeholder={t("ConnectorsPanel.jiraEmailPlaceholder")}
              className="h-8 rounded-md border border-border bg-surface px-2.5 text-sm text-foreground placeholder:text-faint focus:outline-none focus:ring-2 focus:ring-accent"
            />
            <input
              type="text"
              value={siteUrl}
              onChange={(event) => setSiteUrl(event.target.value)}
              placeholder={t("ConnectorsPanel.jiraSiteUrlPlaceholder")}
              className="h-8 rounded-md border border-border bg-surface px-2.5 font-mono text-sm text-foreground placeholder:font-sans placeholder:text-faint focus:outline-none focus:ring-2 focus:ring-accent"
            />
          </>
        )}
        <input
          type="password"
          value={token}
          onChange={(event) => setToken(event.target.value)}
          placeholder={t(info.tokenPlaceholderKey)}
          autoComplete="off"
          className="h-8 rounded-md border border-border bg-surface px-2.5 font-mono text-sm text-foreground placeholder:font-sans placeholder:text-faint focus:outline-none focus:ring-2 focus:ring-accent"
        />
      </div>
      {error && <p className="mt-2 text-xs text-danger">{error}</p>}
      <div className="mt-3 flex justify-end gap-2">
        <Button variant="ghost" size="sm" onClick={onDone} disabled={submitting}>
          {t("ConnectorsPanel.cancelButton")}
        </Button>
        <Button size="sm" onClick={() => void handleSubmit()} disabled={!canSubmit}>
          {submitting ? t("ConnectorsPanel.verifyingButton") : t("ConnectorsPanel.verifyAndSaveButton")}
        </Button>
      </div>
    </div>
  );
}

/** One OAuth provider's connect form. The redirect URI comes first, before
 * the client fields, because it is what the user has to register with the
 * provider *before* the client id exists — and it never changes for that
 * provider (see `connector_oauth::preferred_redirect_uri`). Nothing is saved
 * until the backend's live read-only identity call has succeeded. */
export function OAuthConnectForm({ info, onDone }: { info: OAuthProviderInfo; onDone: () => void }) {
  const { t } = useT();
  const oauthConnect = useConnectorsStore((s) => s.oauthConnect);
  const oauthCancel = useConnectorsStore((s) => s.oauthCancel);
  const oauthRedirectUri = useConnectorsStore((s) => s.oauthRedirectUri);
  const status = useConnectorsStore((s) => s.oauthStatus[info.provider]);

  const [label, setLabel] = useState("");
  const [host, setHost] = useState("");
  const [clientId, setClientId] = useState("");
  const [clientSecret, setClientSecret] = useState("");
  const [redirectUri, setRedirectUri] = useState<string | null>(null);
  const [copied, setCopied] = useState(false);
  const [connecting, setConnecting] = useState(false);
  const [attempted, setAttempted] = useState(false);
  const [error, setError] = useState<string | null>(null);

  // `oauthStatus` is keyed by provider and outlives this form, so an earlier
  // attempt's terminal phase is still there when the card is reopened — for a
  // *second* account of the same provider that would be a green "Connected"
  // pill over an untouched form. Only this mount's own attempt is rendered.
  const phase: ConnectorOAuthPhase = status?.phase ?? "idle";
  const inFlight = connecting && !CONNECTOR_OAUTH_DONE.includes(phase);
  const showSecretField = info.secret !== "never";
  // The client ID is deliberately not required: left blank, the backend reuses
  // the registration already saved in the keychain for this provider, which is
  // what makes a second account of the same provider one click. Blank with
  // nothing stored comes back as the `needs_client_id` phase.
  const canSubmit =
    label.trim().length > 0 && (!info.hostRequired || host.trim().length > 0) && !connecting;

  useEffect(() => {
    if (redirectUri !== null) return;
    void oauthRedirectUri(info.provider)
      .then(setRedirectUri)
      .catch(() => {});
  }, [redirectUri, oauthRedirectUri, info.provider]);

  async function handleConnect() {
    if (!canSubmit) return;
    setConnecting(true);
    setAttempted(true);
    setError(null);
    try {
      await oauthConnect({
        provider: info.provider,
        label: label.trim(),
        host: info.hostPlaceholderKey ? host.trim() || undefined : undefined,
        clientId: clientId.trim() || undefined,
        clientSecret: clientSecret.trim() || undefined,
      });
      onDone();
    } catch (err) {
      setError(errorMessage(err));
    } finally {
      setConnecting(false);
    }
  }

  return (
    <div className="rounded-lg border border-border bg-background p-3">
      <p className="rounded-md bg-surface-2 px-2.5 py-2 text-xs leading-5 text-muted">{t(info.copyKey)}</p>
      {redirectUri && (
        <div className="mt-3 rounded-md border border-border bg-surface-2 px-2.5 py-2">
          <p className="text-[11px] font-semibold uppercase text-faint">{t("ConnectorsPanel.oauthRedirectUriLabel")}</p>
          <div className="mt-1 flex items-center gap-2">
            <code className="min-w-0 flex-1 truncate font-mono text-xs text-foreground">{redirectUri}</code>
            <Button
              variant="ghost"
              size="sm"
              onClick={() => {
                void navigator.clipboard.writeText(redirectUri).then(() => {
                  setCopied(true);
                  setTimeout(() => setCopied(false), 2000);
                });
              }}
            >
              {copied ? t("ConnectorsPanel.oauthRedirectUriCopied") : t("ConnectorsPanel.oauthRedirectUriCopy")}
            </Button>
          </div>
          <p className="mt-1 text-[11px] leading-4 text-muted">{t("ConnectorsPanel.oauthRedirectUriHint")}</p>
        </div>
      )}
      <div className="mt-3 flex flex-col gap-2">
        <input
          type="text"
          value={label}
          onChange={(event) => setLabel(event.target.value)}
          placeholder={t("ConnectorsPanel.labelPlaceholder")}
          aria-label={t("ConnectorsPanel.labelPlaceholder")}
          className="h-8 rounded-md border border-border bg-surface px-2.5 text-sm text-foreground placeholder:text-faint focus:outline-none focus:ring-2 focus:ring-accent"
        />
        {info.hostPlaceholderKey && (
          <input
            type="text"
            value={host}
            onChange={(event) => setHost(event.target.value)}
            placeholder={t(info.hostPlaceholderKey)}
            aria-label={t(info.hostPlaceholderKey)}
            className="h-8 rounded-md border border-border bg-surface px-2.5 font-mono text-sm text-foreground placeholder:font-sans placeholder:text-faint focus:outline-none focus:ring-2 focus:ring-accent"
          />
        )}
        <input
          type="text"
          value={clientId}
          onChange={(event) => setClientId(event.target.value)}
          placeholder={t("ConnectorsPanel.oauthClientIdPlaceholder")}
          aria-label={t("ConnectorsPanel.oauthClientIdPlaceholder")}
          autoComplete="off"
          className="h-8 rounded-md border border-border bg-surface px-2.5 font-mono text-sm text-foreground placeholder:font-sans placeholder:text-faint focus:outline-none focus:ring-2 focus:ring-accent"
        />
        {showSecretField && (
          <input
            type="password"
            value={clientSecret}
            onChange={(event) => setClientSecret(event.target.value)}
            placeholder={t("ConnectorsPanel.oauthClientSecretPlaceholder")}
            aria-label={t("ConnectorsPanel.oauthClientSecretPlaceholder")}
            autoComplete="off"
            className="h-8 rounded-md border border-border bg-surface px-2.5 font-mono text-sm text-foreground placeholder:font-sans placeholder:text-faint focus:outline-none focus:ring-2 focus:ring-accent"
          />
        )}
        <p className="text-[11px] leading-4 text-muted">
          {t(`ConnectorsPanel.oauthSecretHint_${info.secret}`)}
        </p>
      </div>
      {attempted && phase !== "idle" && (
        <div className="mt-2">
          <StatusPill tone={CONNECTOR_OAUTH_PHASE_TONE[phase] ?? "neutral"}>
            {t(`McpPanel.oauthPhase_${phase}`)}
          </StatusPill>
        </div>
      )}
      {(error || (attempted && status?.error)) && (
        <p className="mt-2 text-xs text-danger">{lastAppErrorLine(error ?? status?.error ?? "")}</p>
      )}
      <div className="mt-3 flex justify-end gap-2">
        {inFlight ? (
          <Button variant="ghost" size="sm" onClick={() => void oauthCancel(info.provider)}>
            {t("ConnectorsPanel.oauthCancelButton")}
          </Button>
        ) : (
          <Button variant="ghost" size="sm" onClick={onDone} disabled={connecting}>
            {t("ConnectorsPanel.cancelButton")}
          </Button>
        )}
        <Button size="sm" onClick={() => void handleConnect()} disabled={!canSubmit}>
          {connecting ? t("ConnectorsPanel.connectingButton") : t("ConnectorsPanel.oauthConnectButton")}
        </Button>
      </div>
    </div>
  );
}

function S3ConnectForm({ onDone }: { onDone: () => void }) {
  const { t } = useT();
  const addS3 = useConnectorsStore((s) => s.addS3);

  const [label, setLabel] = useState("");
  const [endpoint, setEndpoint] = useState("");
  const [bucket, setBucket] = useState("");
  const [region, setRegion] = useState("");
  const [accessKey, setAccessKey] = useState("");
  const [secretKey, setSecretKey] = useState("");
  const [submitting, setSubmitting] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const canSubmit =
    [label, endpoint, bucket, region, accessKey, secretKey].every((value) => value.trim().length > 0) &&
    !submitting;

  async function handleSubmit() {
    if (!canSubmit) return;
    setSubmitting(true);
    setError(null);
    try {
      await addS3({
        label: label.trim(),
        endpoint: endpoint.trim(),
        bucket: bucket.trim(),
        region: region.trim(),
        accessKey: accessKey.trim(),
        secretKey: secretKey.trim(),
      });
      onDone();
    } catch (err) {
      setError(errorMessage(err));
    } finally {
      setSubmitting(false);
    }
  }

  return (
    <div className="rounded-lg border border-border bg-background p-3">
      <p className="rounded-md bg-surface-2 px-2.5 py-2 text-xs leading-5 text-muted">{t("ConnectorsPanel.s3Copy")}</p>
      <div className="mt-3 flex flex-col gap-2">
        <input
          type="text"
          value={label}
          onChange={(event) => setLabel(event.target.value)}
          placeholder={t("ConnectorsPanel.labelPlaceholder")}
          className="h-8 rounded-md border border-border bg-surface px-2.5 text-sm text-foreground placeholder:text-faint focus:outline-none focus:ring-2 focus:ring-accent"
        />
        <input
          type="text"
          value={endpoint}
          onChange={(event) => setEndpoint(event.target.value)}
          placeholder={t("ConnectorsPanel.s3EndpointPlaceholder")}
          className="h-8 rounded-md border border-border bg-surface px-2.5 font-mono text-sm text-foreground placeholder:font-sans placeholder:text-faint focus:outline-none focus:ring-2 focus:ring-accent"
        />
        <div className="flex gap-2">
          <input
            type="text"
            value={bucket}
            onChange={(event) => setBucket(event.target.value)}
            placeholder={t("ConnectorsPanel.s3BucketPlaceholder")}
            className="h-8 min-w-0 flex-1 rounded-md border border-border bg-surface px-2.5 font-mono text-sm text-foreground placeholder:font-sans placeholder:text-faint focus:outline-none focus:ring-2 focus:ring-accent"
          />
          <input
            type="text"
            value={region}
            onChange={(event) => setRegion(event.target.value)}
            placeholder={t("ConnectorsPanel.s3RegionPlaceholder")}
            className="h-8 w-32 shrink-0 rounded-md border border-border bg-surface px-2.5 font-mono text-sm text-foreground placeholder:font-sans placeholder:text-faint focus:outline-none focus:ring-2 focus:ring-accent"
          />
        </div>
        <input
          type="text"
          value={accessKey}
          onChange={(event) => setAccessKey(event.target.value)}
          placeholder={t("ConnectorsPanel.s3AccessKeyPlaceholder")}
          autoComplete="off"
          className="h-8 rounded-md border border-border bg-surface px-2.5 font-mono text-sm text-foreground placeholder:font-sans placeholder:text-faint focus:outline-none focus:ring-2 focus:ring-accent"
        />
        <input
          type="password"
          value={secretKey}
          onChange={(event) => setSecretKey(event.target.value)}
          placeholder={t("ConnectorsPanel.s3SecretKeyPlaceholder")}
          autoComplete="off"
          className="h-8 rounded-md border border-border bg-surface px-2.5 font-mono text-sm text-foreground placeholder:font-sans placeholder:text-faint focus:outline-none focus:ring-2 focus:ring-accent"
        />
      </div>
      {error && <p className="mt-2 text-xs text-danger">{error}</p>}
      <div className="mt-3 flex justify-end gap-2">
        <Button variant="ghost" size="sm" onClick={onDone} disabled={submitting}>
          {t("ConnectorsPanel.cancelButton")}
        </Button>
        <Button size="sm" onClick={() => void handleSubmit()} disabled={!canSubmit}>
          {submitting ? t("ConnectorsPanel.verifyingButton") : t("ConnectorsPanel.verifyAndSaveButton")}
        </Button>
      </div>
    </div>
  );
}

const HEALTH_TONE: Record<"ok" | "error" | "unverified", PillTone> = {
  ok: "success",
  error: "danger",
  unverified: "neutral",
};

function AccountRow({ account }: { account: ConnectorAccount }) {
  const { t } = useT();
  const remove = useConnectorsStore((s) => s.remove);
  const reverify = useConnectorsStore((s) => s.reverify);
  const Icon = PROVIDER_ICONS[account.provider];

  const [reverifying, setReverifying] = useState(false);
  const [confirmingRemove, setConfirmingRemove] = useState(false);
  const [removing, setRemoving] = useState(false);
  const [actionError, setActionError] = useState<string | null>(null);

  const health: "ok" | "error" | "unverified" = account.last_error
    ? "error"
    : account.last_verified_at
      ? "ok"
      : "unverified";

  async function handleReverify() {
    setReverifying(true);
    setActionError(null);
    try {
      await reverify(account.id);
    } catch (err) {
      setActionError(errorMessage(err));
    } finally {
      setReverifying(false);
    }
  }

  async function handleRemove() {
    setRemoving(true);
    setActionError(null);
    try {
      await remove(account.id);
    } catch (err) {
      setActionError(errorMessage(err));
      setRemoving(false);
    }
  }

  return (
    <div className="rounded-lg border border-border bg-background p-3">
      <div className="flex items-center gap-2">
        <Icon size={15} className="shrink-0 text-muted" />
        <span className="truncate text-sm font-medium text-foreground">{account.label}</span>
        <StatusPill tone={HEALTH_TONE[health]}>{t(`ConnectorsPanel.health_${health}`)}</StatusPill>
        <div className="ml-auto flex shrink-0 items-center gap-2">
          <Button variant="ghost" size="sm" onClick={() => void handleReverify()} disabled={reverifying}>
            <RefreshCw size={12} className={reverifying ? "animate-spin" : ""} />
            {t("ConnectorsPanel.reverifyButton")}
          </Button>
          {confirmingRemove ? (
            <span className="flex items-center gap-1">
              <Button variant="ghost" size="sm" onClick={() => setConfirmingRemove(false)} disabled={removing}>
                {t("ConnectorsPanel.removeCancelButton")}
              </Button>
              <Button variant="danger" size="sm" onClick={() => void handleRemove()} disabled={removing}>
                {removing ? t("ConnectorsPanel.removingButton") : t("ConnectorsPanel.removeConfirmButton")}
              </Button>
            </span>
          ) : (
            <Button variant="ghost" size="sm" onClick={() => setConfirmingRemove(true)}>
              <Trash2 size={12} />
              {t("ConnectorsPanel.removeButton")}
            </Button>
          )}
        </div>
      </div>
      <p className="mt-1 truncate text-xs text-faint">
        {t(PROVIDER_LABEL_KEYS[account.provider])}
        {account.identity ? ` · ${account.identity}` : ""}
      </p>
      {account.last_verified_at && (
        <p className="mt-1 text-xs text-faint">
          {t("ConnectorsPanel.lastVerifiedLabel", { date: formatDate(account.last_verified_at) })}
        </p>
      )}
      {account.last_error && <p className="mt-1.5 text-xs text-danger">{account.last_error}</p>}
      {actionError && <p className="mt-1.5 text-xs text-danger">{actionError}</p>}
    </div>
  );
}

function AuditExport() {
  const { t } = useT();
  const exportAudit = useConnectorsStore((s) => s.exportAudit);
  const [audit, setAudit] = useState<ConnectorAuditEntry[] | null>(null);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);

  async function handleExport() {
    setLoading(true);
    setError(null);
    try {
      setAudit(await exportAudit());
    } catch (err) {
      setError(errorMessage(err));
    } finally {
      setLoading(false);
    }
  }

  return (
    <div className="flex flex-col gap-2">
      <div>
        <Button variant="secondary" size="sm" onClick={() => void handleExport()} disabled={loading}>
          {loading ? t("ConnectorsPanel.exportingButton") : t("ConnectorsPanel.exportAuditButton")}
        </Button>
      </div>
      {error && <p className="text-xs text-danger">{error}</p>}
      {audit && (
        <pre className="max-h-64 overflow-auto rounded-md border border-border bg-surface-2 p-2 text-[11px] leading-4 text-muted">
          {JSON.stringify(audit, null, 2)}
        </pre>
      )}
    </div>
  );
}

/**
 * Settings "Connectors" tab. Two distinct kinds of connection live here and
 * they share no credential:
 *
 * - **Accounts** ("Connect a new account"): the `connectorsStore` catalog —
 *   GitHub via the `gh` CLI, Slack/Notion/Jira with a pasted token, S3/R2
 *   with access keys, and eleven providers over authorization-code OAuth
 *   against an OAuth app the user registers themselves (`connector_oauth.rs`
 *   — no client credentials ship in this binary). Each one is proven with a
 *   live read-only identity call before it is saved and stored in the OS
 *   keychain only.
 * - **App connectors** (the grid above): each card adds an *MCP server* and
 *   connects it through `mcpStore.oauthConnect`. Eight providers appear in both
 *   places under the same brand name (Slack, Notion, Google Drive, Linear,
 *   Asana, Box, Dropbox, HubSpot); they are separate connections with
 *   separate keychain entries, which is why their copy says so.
 */
export function ConnectorsPanel() {
  const { t } = useT();
  const accounts = useConnectorsStore((s) => s.accounts);
  const loading = useConnectorsStore((s) => s.loading);
  const error = useConnectorsStore((s) => s.error);
  const refresh = useConnectorsStore((s) => s.refresh);

  const [openForm, setOpenForm] = useState<ConnectorProvider | null>(null);

  const [appSearchQuery, setAppSearchQuery] = useState("");
  const [appSelectedCategories, setAppSelectedCategories] = useState<Set<AppConnectorCategory>>(new Set());
  const [appSortMode, setAppSortMode] = useState<"default" | "alphabetical">("default");
  const [appFilterMenuOpen, setAppFilterMenuOpen] = useState(false);
  const [appSortMenuOpen, setAppSortMenuOpen] = useState(false);
  const appFilterMenuRef = useRef<HTMLDivElement>(null);
  const appSortMenuRef = useRef<HTMLDivElement>(null);

  useEffect(() => {
    void refresh();
  }, [refresh]);

  useEffect(() => {
    if (!appFilterMenuOpen && !appSortMenuOpen) return;
    function handlePointerDown(event: PointerEvent) {
      const target = event.target as Node;
      if (appFilterMenuOpen && appFilterMenuRef.current && !appFilterMenuRef.current.contains(target)) {
        setAppFilterMenuOpen(false);
      }
      if (appSortMenuOpen && appSortMenuRef.current && !appSortMenuRef.current.contains(target)) {
        setAppSortMenuOpen(false);
      }
    }
    document.addEventListener("pointerdown", handlePointerDown);
    return () => document.removeEventListener("pointerdown", handlePointerDown);
  }, [appFilterMenuOpen, appSortMenuOpen]);

  function toggleAppCategory(category: AppConnectorCategory) {
    setAppSelectedCategories((prev) => {
      const next = new Set(prev);
      if (next.has(category)) next.delete(category);
      else next.add(category);
      return next;
    });
  }

  const searchedAppConnectors = APP_CONNECTORS.filter((template) => {
    if (appSelectedCategories.size > 0 && !appSelectedCategories.has(template.category)) return false;
    const query = appSearchQuery.trim().toLowerCase();
    if (!query) return true;
    const haystack = `${t(template.labelKey)} ${t(template.descriptionKey)}`.toLowerCase();
    return haystack.includes(query);
  });
  const visibleAppConnectors =
    appSortMode === "alphabetical"
      ? [...searchedAppConnectors].sort((a, b) => t(a.labelKey).localeCompare(t(b.labelKey)))
      : searchedAppConnectors;

  return (
    <div className="flex flex-col gap-3 py-2">
      <p className="text-xs text-muted">{t("ConnectorsPanel.description")}</p>
      <p className="rounded-md bg-surface-2 px-2 py-1.5 text-xs text-muted">{t("ConnectorsPanel.nonGoalNotice")}</p>
      {error && <p className="text-xs text-danger">{error}</p>}

      <section className="rounded-lg border border-border bg-surface p-3">
        <h3 className="text-sm font-semibold text-foreground">{t("ConnectorsPanel.appConnectorsHeading")}</h3>
        <p className="mt-1 text-xs leading-5 text-muted">{t("ConnectorsPanel.appConnectorsDescription")}</p>

        <div className="mt-3 flex flex-wrap items-center gap-2">
          <div className="relative min-w-[180px] flex-1">
            <Search size={13} className="pointer-events-none absolute left-2 top-1/2 -translate-y-1/2 text-faint" />
            <input
              type="text"
              value={appSearchQuery}
              onChange={(event) => setAppSearchQuery(event.target.value)}
              placeholder={t("ConnectorsPanel.appSearchPlaceholder")}
              className="h-8 w-full rounded-md border border-border bg-background pl-7 pr-2 text-xs text-foreground placeholder:text-faint focus:outline-none focus:ring-2 focus:ring-accent"
            />
          </div>

          <div className="relative" ref={appFilterMenuRef}>
            <Button
              variant="ghost"
              size="sm"
              onClick={() => {
                setAppSortMenuOpen(false);
                setAppFilterMenuOpen((prev) => !prev);
              }}
            >
              <SlidersHorizontal size={12} />
              {t("ConnectorsPanel.appFilterByButton")}
              {appSelectedCategories.size > 0 && (
                <span className="ml-0.5 rounded-full bg-accent px-1.5 text-[10px] font-medium text-accent-foreground">
                  {appSelectedCategories.size}
                </span>
              )}
              <ChevronDown size={12} />
            </Button>
            {appFilterMenuOpen && (
              <div className="absolute right-0 top-full z-30 mt-1 max-h-80 w-56 overflow-y-auto rounded-lg border border-border bg-background py-2 shadow-lg">
                <div className="flex items-center justify-between px-3 pb-1">
                  <span className="text-[11px] font-semibold uppercase text-faint">{t("ConnectorsPanel.appFilterCategoryHeading")}</span>
                  {appSelectedCategories.size > 0 && (
                    <button type="button" onClick={() => setAppSelectedCategories(new Set())} className="cursor-pointer text-[11px] text-accent hover:underline">
                      {t("ConnectorsPanel.appFilterClearButton")}
                    </button>
                  )}
                </div>
                {CONNECTOR_CATEGORIES.map((category) => (
                  <label key={category.id} className="flex cursor-pointer items-center gap-2 px-3 py-1 text-sm text-foreground hover:bg-surface-2">
                    <input
                      type="checkbox"
                      checked={appSelectedCategories.has(category.id)}
                      onChange={() => toggleAppCategory(category.id)}
                      className="accent-accent"
                    />
                    {t(category.labelKey)}
                  </label>
                ))}
              </div>
            )}
          </div>

          <div className="relative" ref={appSortMenuRef}>
            <Button
              variant="ghost"
              size="sm"
              onClick={() => {
                setAppFilterMenuOpen(false);
                setAppSortMenuOpen((prev) => !prev);
              }}
            >
              {t("ConnectorsPanel.appSortByButton")}
              <ChevronDown size={12} />
            </Button>
            {appSortMenuOpen && (
              <div className="absolute right-0 top-full z-30 mt-1 w-40 rounded-lg border border-border bg-background py-1 shadow-lg">
                {(["default", "alphabetical"] as const).map((mode) => (
                  <button
                    key={mode}
                    type="button"
                    onClick={() => {
                      setAppSortMode(mode);
                      setAppSortMenuOpen(false);
                    }}
                    className="flex w-full cursor-pointer items-center justify-between gap-2 px-3 py-1.5 text-left text-sm text-foreground hover:bg-surface-2"
                  >
                    {t(mode === "default" ? "ConnectorsPanel.appSortDefault" : "ConnectorsPanel.appSortAlphabetical")}
                    {appSortMode === mode && <Check size={13} className="text-accent" />}
                  </button>
                ))}
              </div>
            )}
          </div>
        </div>

        {visibleAppConnectors.length === 0 ? (
          <p className="mt-4 px-1 text-xs text-faint">{t("ConnectorsPanel.appNoMatch")}</p>
        ) : (
          <div className="mt-3 grid gap-2 lg:grid-cols-2">
            {visibleAppConnectors.map((template) => (
              <AppConnectorCard key={template.id} template={template} />
            ))}
          </div>
        )}
      </section>

      <section className="rounded-lg border border-border bg-surface p-3">
        <h3 className="text-sm font-semibold text-foreground">{t("ConnectorsPanel.connectHeading")}</h3>
        <div className="mt-3 grid gap-2 lg:grid-cols-2">
          <GithubConnectCard />
          {TOKEN_PROVIDERS.map((info) => {
            const Icon = PROVIDER_ICONS[info.provider];
            const isOpen = openForm === info.provider;
            return (
              <article key={info.provider} className="flex flex-col gap-2">
                {!isOpen && (
                  <div className="rounded-lg border border-border bg-background p-3">
                    <div className="flex items-start gap-3">
                      <span className="flex h-9 w-9 shrink-0 items-center justify-center rounded-md bg-surface-2 text-muted">
                        <Icon size={17} />
                      </span>
                      <div className="min-w-0 flex-1">
                        <h4 className="text-sm font-semibold text-foreground">{t(PROVIDER_LABEL_KEYS[info.provider])}</h4>
                        <p className="mt-1 text-xs leading-5 text-muted">{t(info.copyKey)}</p>
                      </div>
                    </div>
                    <div className="mt-3 flex justify-end">
                      <Button size="sm" onClick={() => setOpenForm(info.provider)}>
                        {t("ConnectorsPanel.connectButton")}
                      </Button>
                    </div>
                  </div>
                )}
                {isOpen && <TokenConnectForm info={info} onDone={() => setOpenForm(null)} />}
              </article>
            );
          })}
          <article className="flex flex-col gap-2">
            {openForm !== "s3" && (
              <div className="rounded-lg border border-border bg-background p-3">
                <div className="flex items-start gap-3">
                  <span className="flex h-9 w-9 shrink-0 items-center justify-center rounded-md bg-surface-2 text-muted">
                    <Database size={17} />
                  </span>
                  <div className="min-w-0 flex-1">
                    <h4 className="text-sm font-semibold text-foreground">{t("ConnectorsPanel.providerS3")}</h4>
                    <p className="mt-1 text-xs leading-5 text-muted">{t("ConnectorsPanel.s3Copy")}</p>
                  </div>
                </div>
                <div className="mt-3 flex justify-end">
                  <Button size="sm" onClick={() => setOpenForm("s3")}>
                    {t("ConnectorsPanel.connectButton")}
                  </Button>
                </div>
              </div>
            )}
            {openForm === "s3" && <S3ConnectForm onDone={() => setOpenForm(null)} />}
          </article>
        </div>

        <h4 className="mt-5 text-sm font-semibold text-foreground">{t("ConnectorsPanel.oauthHeading")}</h4>
        <p className="mt-1 text-xs leading-5 text-muted">{t("ConnectorsPanel.oauthDescription")}</p>
        <div className="mt-3 grid gap-2 lg:grid-cols-2">
          {OAUTH_PROVIDERS.map((info) => {
            const Icon = PROVIDER_ICONS[info.provider];
            const isOpen = openForm === info.provider;
            return (
              <article key={info.provider} className="flex flex-col gap-2">
                {!isOpen && (
                  <div className="rounded-lg border border-border bg-background p-3">
                    <div className="flex items-start gap-3">
                      <span className="flex h-9 w-9 shrink-0 items-center justify-center rounded-md bg-surface-2 text-muted">
                        <Icon size={17} />
                      </span>
                      <div className="min-w-0 flex-1">
                        <h4 className="text-sm font-semibold text-foreground">{t(PROVIDER_LABEL_KEYS[info.provider])}</h4>
                        <p className="mt-1 text-xs leading-5 text-muted">{t(info.copyKey)}</p>
                      </div>
                    </div>
                    <div className="mt-3 flex justify-end">
                      <Button size="sm" onClick={() => setOpenForm(info.provider)}>
                        {t("ConnectorsPanel.connectButton")}
                      </Button>
                    </div>
                  </div>
                )}
                {isOpen && <OAuthConnectForm info={info} onDone={() => setOpenForm(null)} />}
              </article>
            );
          })}
        </div>
      </section>

      <section className="rounded-lg border border-border bg-surface p-3">
        <div className="flex items-center justify-between gap-2">
          <h3 className="text-sm font-semibold text-foreground">{t("ConnectorsPanel.connectedHeading")}</h3>
          {loading && <span className="text-xs text-faint">{t("ConnectorsPanel.loadingLabel")}</span>}
        </div>
        <div className="mt-3 flex flex-col gap-2">
          {accounts.length === 0 ? (
            <p className="px-1 text-xs text-faint">{t("ConnectorsPanel.emptyState")}</p>
          ) : (
            accounts.map((account) => <AccountRow key={account.id} account={account} />)
          )}
        </div>
      </section>

      <section className="rounded-lg border border-border bg-surface p-3">
        <h3 className="text-sm font-semibold text-foreground">{t("ConnectorsPanel.auditHeading")}</h3>
        <p className="mt-1 text-xs leading-5 text-muted">{t("ConnectorsPanel.auditDescription")}</p>
        <div className="mt-3">
          <AuditExport />
        </div>
      </section>

      <section className="rounded-lg border border-border bg-surface p-3">
        <h3 className="text-sm font-semibold text-foreground">{t("ConnectorsPanel.mcpServersHeading")}</h3>
        <p className="mt-1 text-xs leading-5 text-muted">{t("ConnectorsPanel.mcpServersDescription")}</p>
      </section>
      <McpPanel />
    </div>
  );
}

export default ConnectorsPanel;
