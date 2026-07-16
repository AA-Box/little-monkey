import { useEffect, useState } from "react";
import {
  Database,
  GitPullRequest,
  MessageCircle,
  NotebookText,
  RefreshCw,
  Ticket,
  Trash2,
  type LucideIcon,
} from "lucide-react";
import { Button, StatusPill, type PillTone } from "../ui";
import {
  useConnectorsStore,
  type ConnectorAccount,
  type ConnectorAuditEntry,
  type ConnectorProvider,
} from "../../store/connectorsStore";
import { useT } from "../../lib/i18n";

const PROVIDER_ICONS: Record<ConnectorProvider, LucideIcon> = {
  github: GitPullRequest,
  slack: MessageCircle,
  notion: NotebookText,
  jira: Ticket,
  s3: Database,
};

const PROVIDER_LABEL_KEYS: Record<ConnectorProvider, string> = {
  github: "ConnectorsPanel.providerGithub",
  slack: "ConnectorsPanel.providerSlack",
  notion: "ConnectorsPanel.providerNotion",
  jira: "ConnectorsPanel.providerJira",
  s3: "ConnectorsPanel.providerS3",
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
      setError(err instanceof Error ? err.message : String(err));
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
      setError(err instanceof Error ? err.message : String(err));
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
      setError(err instanceof Error ? err.message : String(err));
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
      setActionError(err instanceof Error ? err.message : String(err));
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
      setActionError(err instanceof Error ? err.message : String(err));
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
      setError(err instanceof Error ? err.message : String(err));
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
 * Settings "Connectors" tab: guided GitHub (via `gh` CLI)/Slack/Notion/Jira/
 * S3 connections — pick a provider, see its exact scopes/storage-location
 * copy, verify-then-save, then manage (reverify/revoke/export audit) what's
 * already connected. Google Drive, SharePoint/Graph, and anything else that
 * genuinely needs a registered OAuth app are explicit non-goals here (see
 * `connectors.rs`'s module doc) — not faked with a token workaround.
 */
export function ConnectorsPanel() {
  const { t } = useT();
  const accounts = useConnectorsStore((s) => s.accounts);
  const loading = useConnectorsStore((s) => s.loading);
  const error = useConnectorsStore((s) => s.error);
  const refresh = useConnectorsStore((s) => s.refresh);

  const [openForm, setOpenForm] = useState<"slack" | "notion" | "jira" | "s3" | null>(null);

  useEffect(() => {
    void refresh();
  }, [refresh]);

  return (
    <div className="flex flex-col gap-3 py-2">
      <p className="text-xs text-muted">{t("ConnectorsPanel.description")}</p>
      <p className="rounded-md bg-surface-2 px-2 py-1.5 text-xs text-muted">{t("ConnectorsPanel.nonGoalNotice")}</p>
      {error && <p className="text-xs text-danger">{error}</p>}

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
    </div>
  );
}

export default ConnectorsPanel;
