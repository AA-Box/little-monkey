import { useEffect, useMemo, useState } from "react";
import { open } from "@tauri-apps/plugin-dialog";
import {
  AlertTriangle,
  Check,
  ChevronDown,
  ChevronRight,
  Clipboard,
  Clock3,
  DatabaseZap,
  FileSearch,
  Globe2,
  Loader2,
  Plus,
  RefreshCw,
  ShieldCheck,
  Square,
  Trash2,
} from "lucide-react";
import { useSessionStore } from "../../store/sessionStore";
import { useConnectorsStore, type ConnectorProvider } from "../../store/connectorsStore";
import {
  DEFAULT_HYBRID_CONFIG,
  connectorUsesAccountReference,
  useKnowledgeV2Store,
  type HybridSearchConfig,
  type KnowledgeConnector,
  type KnowledgeInspectorResponse,
  type KnowledgeOcrConfig,
  type KnowledgeSourceV2,
  type OcrInstallRequest,
  type PiiPreview,
} from "../../store/knowledgeV2Store";
import type { KnowledgeStack } from "../../store/stackStore";
import { useT } from "../../lib/i18n";
import { Button, IconButton, StatusPill } from "../ui";
import { errorMessage } from "../../lib/errors";

type ConnectorKind = KnowledgeConnector["kind"];

/** Which Connector Catalog provider (`connectorsStore.ts`) backs each
 * account-reference connector kind — used to filter the picker's option
 * list down to accounts of the right provider. */
const CONNECTOR_KIND_PROVIDER: Record<
  "git_hub_repo" | "s3_bucket" | "notion_pages" | "slack_channels" | "jira_project",
  ConnectorProvider
> = {
  git_hub_repo: "github",
  s3_bucket: "s3",
  notion_pages: "notion",
  slack_channels: "slack",
  jira_project: "jira",
};

// English fallback/legacy labels for the connector kinds this panel already
// supported before i18n coverage was added (see this file's top-level
// pre-existing-gap note) — left as-is rather than retrofitted. The six new
// External Knowledge Sync connector kinds are labeled through `t()` instead,
// inside the component (see `connectorLabels` below).
const LEGACY_CONNECTOR_LABELS = {
  local_file: "Local file",
  local_folder: "Local folder",
  project: "Project",
  url: "Website / URL",
  sitemap: "Sitemap",
  selected_chats: "Selected conversations",
  web_dav: "WebDAV file",
} as const satisfies Partial<Record<ConnectorKind, string>>;

function toOrigin(value: string): string {
  try {
    return new URL(value).origin;
  } catch {
    return "";
  }
}

function formatWhen(value: number | null): string {
  return value ? new Date(value).toLocaleString() : "Never";
}

function sourceDescription(source: KnowledgeSourceV2): string {
  switch (source.connector.kind) {
    case "local_file":
    case "local_folder":
    case "project":
      return source.connector.path;
    case "url":
    case "sitemap":
    case "web_dav":
      return source.connector.url;
    case "selected_chats":
      return `${source.connector.session_ids.length} conversation${source.connector.session_ids.length === 1 ? "" : "s"}`;
    case "git_hub_repo": {
      const { owner, repo, git_ref, path_prefix } = source.connector;
      const ref = git_ref ? `@${git_ref}` : "";
      const prefix = path_prefix ? `/${path_prefix}` : "";
      return `${owner}/${repo}${ref}${prefix}`;
    }
    case "s3_bucket": {
      const { bucket, prefix } = source.connector;
      return `s3://${bucket}${prefix ? `/${prefix}` : ""}`;
    }
    case "watched_folder":
      return source.connector.path;
    case "notion_pages":
      return source.connector.root_id;
    case "slack_channels":
      return source.connector.channel_ids.join(", ");
    case "jira_project":
      return source.connector.project_key;
  }
}

/**
 * `stackId` and `onStackChange` are owned by `KnowledgePanel`, which uses the
 * same value to decide which stack row is expanded. Before that, this panel kept
 * its own selection and the two could sit on different stacks — a stack's v1
 * sources and its v2 sources were then on screen together, describing different
 * stacks, with nothing saying so.
 */
export function KnowledgeV2Panel({
  stacks,
  stackId,
  onStackChange,
}: {
  stacks: KnowledgeStack[];
  stackId: string;
  onStackChange: (stackId: string) => void;
}) {
  const sources = useKnowledgeV2Store((state) => state.sources);
  const progress = useKnowledgeV2Store((state) => state.progress);
  const reports = useKnowledgeV2Store((state) => state.reports);
  const errors = useKnowledgeV2Store((state) => state.errors);
  const refreshSources = useKnowledgeV2Store((state) => state.refreshSources);
  const addSource = useKnowledgeV2Store((state) => state.addSource);
  const updateSource = useKnowledgeV2Store((state) => state.updateSource);
  const removeSource = useKnowledgeV2Store((state) => state.removeSource);
  const refreshStack = useKnowledgeV2Store((state) => state.refreshStack);
  const cancelRefresh = useKnowledgeV2Store((state) => state.cancelRefresh);
  const updateChunking = useKnowledgeV2Store((state) => state.updateChunking);
  const backgroundConfig = useKnowledgeV2Store((state) => state.backgroundConfig);
  const refreshBackgroundConfig = useKnowledgeV2Store((state) => state.refreshBackgroundConfig);
  const saveBackgroundConfig = useKnowledgeV2Store((state) => state.saveBackgroundConfig);
  const query = useKnowledgeV2Store((state) => state.query);
  const cancelQuery = useKnowledgeV2Store((state) => state.cancelQuery);
  const piiPreview = useKnowledgeV2Store((state) => state.piiPreview);
  const ocrStatus = useKnowledgeV2Store((state) => state.ocrStatus);
  const configureExternalOcr = useKnowledgeV2Store((state) => state.configureExternalOcr);
  const installOcr = useKnowledgeV2Store((state) => state.installOcr);
  const setOcrEnabled = useKnowledgeV2Store((state) => state.setOcrEnabled);
  const sessions = useSessionStore((state) => state.sessions);
  const connectorAccounts = useConnectorsStore((state) => state.accounts);
  const refreshConnectorAccounts = useConnectorsStore((state) => state.refresh);
  const { t } = useT();

  const [adding, setAdding] = useState(false);
  const [kind, setKind] = useState<ConnectorKind>("local_folder");
  const [label, setLabel] = useState("");
  const [path, setPath] = useState("");
  const [url, setUrl] = useState("");
  const [allowedOrigin, setAllowedOrigin] = useState("");
  const [maxDepth, setMaxDepth] = useState(1);
  const [maxPages, setMaxPages] = useState(25);
  const [obeyRobots, setObeyRobots] = useState(true);
  const [allowLoopback, setAllowLoopback] = useState(false);
  const [selectedChats, setSelectedChats] = useState<string[]>([]);
  const [webdavUsername, setWebdavUsername] = useState("");
  const [webdavPassword, setWebdavPassword] = useState("");
  const [busy, setBusy] = useState(false);
  const [formError, setFormError] = useState<string | null>(null);
  const [expandedSource, setExpandedSource] = useState<string | null>(null);
  const [backgroundInterval, setBackgroundInterval] = useState(60);
  const [backgroundAllStacks, setBackgroundAllStacks] = useState(true);

  // --- External Knowledge Sync connector form state ---------------------
  const [connectorAccountId, setConnectorAccountId] = useState("");
  const [githubOwner, setGithubOwner] = useState("");
  const [githubRepo, setGithubRepo] = useState("");
  const [githubRef, setGithubRef] = useState("");
  const [githubPathPrefix, setGithubPathPrefix] = useState("");
  const [s3Endpoint, setS3Endpoint] = useState("");
  const [s3Bucket, setS3Bucket] = useState("");
  const [s3Prefix, setS3Prefix] = useState("");
  const [s3Region, setS3Region] = useState("us-east-1");
  const [debounceMs, setDebounceMs] = useState(2_000);
  const [notionRootId, setNotionRootId] = useState("");
  const [slackChannelIds, setSlackChannelIds] = useState("");
  const [jiraProjectKey, setJiraProjectKey] = useState("");

  const connectorLabels: Record<ConnectorKind, string> = {
    ...LEGACY_CONNECTOR_LABELS,
    git_hub_repo: t("KnowledgeV2Panel.connectorGitHubRepo"),
    s3_bucket: t("KnowledgeV2Panel.connectorS3Bucket"),
    watched_folder: t("KnowledgeV2Panel.connectorWatchedFolder"),
    notion_pages: t("KnowledgeV2Panel.connectorNotionPages"),
    slack_channels: t("KnowledgeV2Panel.connectorSlackChannels"),
    jira_project: t("KnowledgeV2Panel.connectorJiraProject"),
  };

  const accountsForKind = connectorUsesAccountReference(kind)
    ? connectorAccounts.filter((account) => account.provider === CONNECTOR_KIND_PROVIDER[kind])
    : [];

  useEffect(() => {
    void refreshConnectorAccounts();
  }, [refreshConnectorAccounts]);

  useEffect(() => {
    if (connectorUsesAccountReference(kind) && !accountsForKind.some((account) => account.id === connectorAccountId)) {
      setConnectorAccountId(accountsForKind[0]?.id ?? "");
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [kind, connectorAccounts]);

  useEffect(() => {
    void refreshSources();
    void refreshBackgroundConfig();
  }, [refreshBackgroundConfig, refreshSources]);

  useEffect(() => {
    if (!backgroundConfig) return;
    setBackgroundInterval(backgroundConfig.intervalMinutes);
    setBackgroundAllStacks(backgroundConfig.stackIds.length === 0);
  }, [backgroundConfig]);

  useEffect(() => {
    if ((kind === "url" || kind === "sitemap" || kind === "web_dav") && url) {
      setAllowedOrigin(toOrigin(url));
    }
  }, [kind, url]);

  const stackSources = useMemo(
    () => sources.filter((source) => source.stack_id === stackId),
    [sources, stackId],
  );
  const selectedStack = stacks.find((stack) => stack.id === stackId) ?? null;
  const activeProgress = stackId ? progress[stackId] : undefined;
  const indexing = activeProgress != null && activeProgress.phase !== "done";
  const report = stackId ? reports[stackId] : undefined;
  const resetForm = () => {
    setAdding(false);
    setLabel("");
    setPath("");
    setUrl("");
    setAllowedOrigin("");
    setSelectedChats([]);
    setWebdavUsername("");
    setWebdavPassword("");
    setGithubOwner("");
    setGithubRepo("");
    setGithubRef("");
    setGithubPathPrefix("");
    setS3Endpoint("");
    setS3Bucket("");
    setS3Prefix("");
    setDebounceMs(2_000);
    setNotionRootId("");
    setSlackChannelIds("");
    setJiraProjectKey("");
    setFormError(null);
  };

  const choosePath = async () => {
    const directory = kind === "local_folder" || kind === "project" || kind === "watched_folder";
    const selected = await open({ directory, multiple: false });
    if (typeof selected === "string") {
      setPath(selected);
      if (!label) setLabel(selected.split(/[\\/]/).filter(Boolean).pop() ?? connectorLabels[kind]);
    }
  };

  const connectorFromForm = (): KnowledgeConnector => {
    if (kind === "local_file") return { kind, path: path.trim() };
    if (kind === "local_folder") return { kind, path: path.trim() };
    if (kind === "project") return { kind, path: path.trim() };
    if (kind === "watched_folder") return { kind, path: path.trim(), debounce_ms: debounceMs };
    if (kind === "selected_chats") return { kind, session_ids: selectedChats };
    if (kind === "web_dav") {
      return {
        kind,
        url: url.trim(),
        username: webdavUsername.trim(),
        credential_ref: `source-${crypto.randomUUID()}`,
        allow_loopback: allowLoopback,
      };
    }
    if (kind === "sitemap") {
      return {
        kind,
        url: url.trim(),
        allowed_origin: allowedOrigin,
        max_pages: maxPages,
        obey_robots: obeyRobots,
        allow_loopback: allowLoopback,
      };
    }
    if (kind === "git_hub_repo") {
      return {
        kind,
        owner: githubOwner.trim(),
        repo: githubRepo.trim(),
        git_ref: githubRef.trim() || null,
        path_prefix: githubPathPrefix.trim() || null,
        connector_account_id: connectorAccountId,
      };
    }
    if (kind === "s3_bucket") {
      return {
        kind,
        endpoint: s3Endpoint.trim(),
        bucket: s3Bucket.trim(),
        prefix: s3Prefix.trim() || null,
        region: s3Region.trim(),
        connector_account_id: connectorAccountId,
      };
    }
    if (kind === "notion_pages") {
      return { kind, connector_account_id: connectorAccountId, root_id: notionRootId.trim() };
    }
    if (kind === "slack_channels") {
      return {
        kind,
        connector_account_id: connectorAccountId,
        channel_ids: slackChannelIds
          .split(",")
          .map((value) => value.trim())
          .filter(Boolean),
      };
    }
    if (kind === "jira_project") {
      return { kind, connector_account_id: connectorAccountId, project_key: jiraProjectKey.trim() };
    }
    return {
      kind: "url",
      url: url.trim(),
      allowed_origin: allowedOrigin,
      max_depth: maxDepth,
      max_pages: maxPages,
      obey_robots: obeyRobots,
      allow_loopback: allowLoopback,
    };
  };

  const handleAdd = async () => {
    if (!stackId) return;
    setBusy(true);
    setFormError(null);
    try {
      const connector = connectorFromForm();
      await addSource(
        stackId,
        label.trim() || connectorLabels[kind],
        connector,
        kind === "web_dav" ? webdavPassword : undefined,
      );
      resetForm();
    } catch (error) {
      setFormError(errorMessage(error));
    } finally {
      setBusy(false);
    }
  };

  return (
    <section className="rounded-lg border border-border bg-background p-3">
      <div className="flex flex-wrap items-start justify-between gap-2">
        <div className="min-w-0">
          <div className="flex items-center gap-2">
            <DatabaseZap size={16} className="text-accent" />
            <h3 className="text-sm font-medium text-foreground">Knowledge Stacks 2.0</h3>
            <StatusPill tone="success">Hybrid</StatusPill>
          </div>
          <p className="mt-1 max-w-2xl text-xs text-faint">
            Incremental Office, PDF, website, project, conversation, and WebDAV ingestion with
            location-aware citations, FTS5 + vector fusion, optional local reranking, and a
            reproducible retrieval trace.
          </p>
        </div>
        <select
          value={stackId}
          onChange={(event) => onStackChange(event.target.value)}
          aria-label={t("KnowledgeV2Panel.stackSelectorAriaLabel")}
          className="h-8 min-w-44 rounded-md border border-border bg-surface px-2 text-xs text-foreground focus:outline-none focus:ring-2 focus:ring-accent"
        >
          {stacks.length === 0 && <option value="">Create a stack first</option>}
          {stacks.map((stack) => (
            <option key={stack.id} value={stack.id}>
              {stack.name}
            </option>
          ))}
        </select>
      </div>

      {stackId && (
        <>
          <OcrControl
            status={ocrStatus}
            configureExternal={configureExternalOcr}
            install={installOcr}
            setEnabled={setOcrEnabled}
          />
          <div className="mt-3 flex flex-wrap items-center gap-2 border-t border-border pt-3">
            {indexing ? (
              <Button variant="danger" size="sm" onClick={() => void cancelRefresh(stackId)}>
                <Square size={13} /> Stop refresh
              </Button>
            ) : (
              <Button
                variant="primary"
                size="sm"
                disabled={stackSources.filter((source) => source.enabled).length === 0}
                onClick={() => void refreshStack(stackId)}
              >
                <RefreshCw size={13} /> Refresh index
              </Button>
            )}
            <Button variant="secondary" size="sm" onClick={() => setAdding((value) => !value)}>
              <Plus size={13} /> Add source
            </Button>
            {activeProgress && (
              <span className="flex items-center gap-1.5 text-xs text-muted">
                {indexing && <Loader2 size={12} className="animate-spin" />}
                {activeProgress.phase} · {activeProgress.objects_done}/{activeProgress.objects_total || "?"}
                {" objects · "}
                {activeProgress.chunks} chunks ({activeProgress.reused_chunks} reused)
              </span>
            )}
          </div>

          <div className="mt-3 rounded-md border border-border bg-surface p-3">
            <div className="flex flex-wrap items-start justify-between gap-3">
              <div className="flex min-w-0 items-start gap-2">
                <Clock3 size={15} className="mt-0.5 shrink-0 text-accent" />
                <div>
                  <p className="text-xs font-medium text-foreground">Daemon background refresh</p>
                  <p className="mt-1 text-[11px] leading-4 text-muted">Opt in to refresh enabled connectors while the app window is closed. The installed local daemon uses the same bounded extraction and atomic index publication path.</p>
                </div>
              </div>
              <label className="flex items-center gap-2 text-xs text-muted">
                <input
                  type="checkbox"
                  checked={backgroundConfig?.enabled ?? false}
                  disabled={!backgroundConfig || busy}
                  onChange={(event) => {
                    setBusy(true);
                    void saveBackgroundConfig(event.target.checked, backgroundInterval, backgroundAllStacks || !stackId ? [] : [stackId])
                      .catch((error) => setFormError(errorMessage(error)))
                      .finally(() => setBusy(false));
                  }}
                />
                Enabled
              </label>
            </div>
            <div className="mt-3 grid gap-2 sm:grid-cols-2">
              <label className="text-xs text-muted">Interval (minutes)<input type="number" min={5} max={10080} value={backgroundInterval} onChange={(event) => setBackgroundInterval(Number(event.target.value))} className="mt-1 h-8 w-full rounded-md border border-border bg-background px-2 text-xs text-foreground" /></label>
              <label className="flex items-end gap-2 pb-2 text-xs text-muted"><input type="checkbox" checked={backgroundAllStacks} onChange={(event) => setBackgroundAllStacks(event.target.checked)} /> Refresh all stacks with enabled sources</label>
            </div>
            <div className="mt-2 flex flex-wrap items-center gap-2">
              <Button size="sm" disabled={!backgroundConfig || busy || backgroundInterval < 5 || backgroundInterval > 10080} onClick={() => {
                setBusy(true);
                void saveBackgroundConfig(backgroundConfig?.enabled ?? false, backgroundInterval, backgroundAllStacks || !stackId ? [] : [stackId])
                  .catch((error) => setFormError(errorMessage(error)))
                  .finally(() => setBusy(false));
              }}>Save background schedule</Button>
              <span className="text-[10px] text-faint">Next: {formatWhen(backgroundConfig?.nextDueMs ?? null)} · Last success: {formatWhen(backgroundConfig?.lastSuccessMs ?? null)}</span>
              {backgroundConfig?.lastError && (
                <p role="alert" className="text-[10px] text-danger">
                  Background refresh failed {backgroundConfig.consecutiveFailures} time{backgroundConfig.consecutiveFailures === 1 ? "" : "s"}: {backgroundConfig.lastError}
                </p>
              )}
            </div>
          </div>

          {errors[stackId] && (
            <p className="mt-2 rounded-md border border-danger/30 bg-danger/5 p-2 text-xs text-danger">
              {errors[stackId]}
            </p>
          )}
          {report && !indexing && (
            <div className="mt-2 grid grid-cols-2 gap-1 rounded-md bg-surface-2 p-2 text-[11px] text-muted sm:grid-cols-4">
              <span>{report.object_count} objects</span>
              <span>{report.embedded_chunks} embedded</span>
              <span>{report.reused_chunks} reused</span>
              <span>{report.deleted_objects} deleted</span>
              <span className="col-span-2 truncate font-mono sm:col-span-4">
                generation {report.generation_id}
              </span>
              {report.warnings.map((warning) => (
                <span key={warning} className="col-span-2 text-warning sm:col-span-4">
                  {warning}
                </span>
              ))}
            </div>
          )}

          {adding && (
            <div className="mt-3 rounded-lg border border-accent/30 bg-surface p-3">
              <p className="mb-2 text-[11px] text-faint">{t("KnowledgeV2Panel.connectorNonGoalsNote")}</p>
              <div className="grid gap-2 sm:grid-cols-2">
                <label className="text-xs text-muted">
                  Source type
                  <select
                    value={kind}
                    onChange={(event) => setKind(event.target.value as ConnectorKind)}
                    className="mt-1 h-8 w-full rounded-md border border-border bg-background px-2 text-xs text-foreground"
                  >
                    {Object.entries(connectorLabels).map(([value, text]) => (
                      <option key={value} value={value}>
                        {text}
                      </option>
                    ))}
                  </select>
                </label>
                <label className="text-xs text-muted">
                  Label
                  <input
                    value={label}
                    onChange={(event) => setLabel(event.target.value)}
                    className="mt-1 h-8 w-full rounded-md border border-border bg-background px-2 text-xs text-foreground"
                    placeholder={connectorLabels[kind]}
                  />
                </label>
              </div>

              {connectorUsesAccountReference(kind) && (
                <label className="mt-2 block text-xs text-muted">
                  {t("KnowledgeV2Panel.connectorAccountLabel")}
                  <select
                    value={connectorAccountId}
                    onChange={(event) => setConnectorAccountId(event.target.value)}
                    className="mt-1 h-8 w-full rounded-md border border-border bg-background px-2 text-xs text-foreground"
                  >
                    {accountsForKind.length === 0 && (
                      <option value="">{t("KnowledgeV2Panel.connectorAccountNoneOption")}</option>
                    )}
                    {accountsForKind.map((account) => (
                      <option key={account.id} value={account.id}>
                        {account.label}
                      </option>
                    ))}
                  </select>
                  {accountsForKind.length === 0 && (
                    <span className="mt-1 block text-[11px] text-warning">
                      {t("KnowledgeV2Panel.connectorAccountMissingHint")}
                    </span>
                  )}
                </label>
              )}

              {kind === "git_hub_repo" && (
                <div className="mt-2 grid gap-2 sm:grid-cols-2">
                  <label className="text-xs text-muted">
                    {t("KnowledgeV2Panel.githubOwnerLabel")}
                    <input
                      value={githubOwner}
                      onChange={(event) => setGithubOwner(event.target.value)}
                      className="mt-1 h-8 w-full rounded-md border border-border bg-background px-2 font-mono text-xs text-foreground"
                      placeholder="acme"
                    />
                  </label>
                  <label className="text-xs text-muted">
                    {t("KnowledgeV2Panel.githubRepoLabel")}
                    <input
                      value={githubRepo}
                      onChange={(event) => setGithubRepo(event.target.value)}
                      className="mt-1 h-8 w-full rounded-md border border-border bg-background px-2 font-mono text-xs text-foreground"
                      placeholder="widgets"
                    />
                  </label>
                  <label className="text-xs text-muted">
                    {t("KnowledgeV2Panel.githubRefLabel")}
                    <input
                      value={githubRef}
                      onChange={(event) => setGithubRef(event.target.value)}
                      className="mt-1 h-8 w-full rounded-md border border-border bg-background px-2 font-mono text-xs text-foreground"
                      placeholder="main"
                    />
                  </label>
                  <label className="text-xs text-muted">
                    {t("KnowledgeV2Panel.githubPathPrefixLabel")}
                    <input
                      value={githubPathPrefix}
                      onChange={(event) => setGithubPathPrefix(event.target.value)}
                      className="mt-1 h-8 w-full rounded-md border border-border bg-background px-2 font-mono text-xs text-foreground"
                      placeholder="docs/"
                    />
                  </label>
                </div>
              )}

              {kind === "s3_bucket" && (
                <div className="mt-2 grid gap-2 sm:grid-cols-2">
                  <label className="text-xs text-muted sm:col-span-2">
                    {t("KnowledgeV2Panel.s3EndpointLabel")}
                    <input
                      value={s3Endpoint}
                      onChange={(event) => setS3Endpoint(event.target.value)}
                      className="mt-1 h-8 w-full rounded-md border border-border bg-background px-2 font-mono text-xs text-foreground"
                      placeholder="https://s3.amazonaws.com"
                    />
                  </label>
                  <label className="text-xs text-muted">
                    {t("KnowledgeV2Panel.s3BucketLabel")}
                    <input
                      value={s3Bucket}
                      onChange={(event) => setS3Bucket(event.target.value)}
                      className="mt-1 h-8 w-full rounded-md border border-border bg-background px-2 font-mono text-xs text-foreground"
                    />
                  </label>
                  <label className="text-xs text-muted">
                    {t("KnowledgeV2Panel.s3RegionLabel")}
                    <input
                      value={s3Region}
                      onChange={(event) => setS3Region(event.target.value)}
                      className="mt-1 h-8 w-full rounded-md border border-border bg-background px-2 font-mono text-xs text-foreground"
                    />
                  </label>
                  <label className="text-xs text-muted sm:col-span-2">
                    {t("KnowledgeV2Panel.s3PrefixLabel")}
                    <input
                      value={s3Prefix}
                      onChange={(event) => setS3Prefix(event.target.value)}
                      className="mt-1 h-8 w-full rounded-md border border-border bg-background px-2 font-mono text-xs text-foreground"
                      placeholder="reports/2024/"
                    />
                  </label>
                </div>
              )}

              {kind === "watched_folder" && (
                <label className="mt-2 block text-xs text-muted">
                  {t("KnowledgeV2Panel.debounceLabel")}
                  <input
                    type="number"
                    min={200}
                    max={600_000}
                    value={debounceMs}
                    onChange={(event) => setDebounceMs(Number(event.target.value))}
                    className="mt-1 h-8 w-full rounded-md border border-border bg-background px-2 text-xs text-foreground"
                  />
                </label>
              )}

              {kind === "notion_pages" && (
                <label className="mt-2 block text-xs text-muted">
                  {t("KnowledgeV2Panel.notionRootIdLabel")}
                  <input
                    value={notionRootId}
                    onChange={(event) => setNotionRootId(event.target.value)}
                    className="mt-1 h-8 w-full rounded-md border border-border bg-background px-2 font-mono text-xs text-foreground"
                  />
                </label>
              )}

              {kind === "slack_channels" && (
                <label className="mt-2 block text-xs text-muted">
                  {t("KnowledgeV2Panel.slackChannelIdsLabel")}
                  <input
                    value={slackChannelIds}
                    onChange={(event) => setSlackChannelIds(event.target.value)}
                    className="mt-1 h-8 w-full rounded-md border border-border bg-background px-2 font-mono text-xs text-foreground"
                    placeholder="C0123456789, C9876543210"
                  />
                </label>
              )}

              {kind === "jira_project" && (
                <label className="mt-2 block text-xs text-muted">
                  {t("KnowledgeV2Panel.jiraProjectKeyLabel")}
                  <input
                    value={jiraProjectKey}
                    onChange={(event) => setJiraProjectKey(event.target.value)}
                    className="mt-1 h-8 w-full rounded-md border border-border bg-background px-2 font-mono text-xs text-foreground"
                    placeholder="PROJ"
                  />
                </label>
              )}

              {(kind === "local_file" || kind === "local_folder" || kind === "project" || kind === "watched_folder") && (
                <div className="mt-2 flex gap-2">
                  <input
                    value={path}
                    readOnly
                    className="h-8 min-w-0 flex-1 rounded-md border border-border bg-background px-2 font-mono text-xs text-foreground"
                    placeholder="Choose an app-approved path"
                  />
                  <Button variant="secondary" size="sm" onClick={() => void choosePath()}>
                    Choose
                  </Button>
                </div>
              )}

              {(kind === "url" || kind === "sitemap" || kind === "web_dav") && (
                <div className="mt-2 grid gap-2 sm:grid-cols-2">
                  <label className="text-xs text-muted sm:col-span-2">
                    URL
                    <input
                      value={url}
                      onChange={(event) => setUrl(event.target.value)}
                      className="mt-1 h-8 w-full rounded-md border border-border bg-background px-2 font-mono text-xs text-foreground"
                      placeholder={kind === "sitemap" ? "https://example.com/sitemap.xml" : "https://example.com/docs"}
                    />
                  </label>
                  {kind !== "web_dav" && (
                    <label className="text-xs text-muted">
                      Allowed origin
                      <input
                        value={allowedOrigin}
                        onChange={(event) => setAllowedOrigin(event.target.value)}
                        className="mt-1 h-8 w-full rounded-md border border-border bg-background px-2 font-mono text-xs text-foreground"
                      />
                    </label>
                  )}
                  {kind === "url" && (
                    <label className="text-xs text-muted">
                      Crawl depth
                      <input
                        type="number"
                        min={0}
                        max={4}
                        value={maxDepth}
                        onChange={(event) => setMaxDepth(Number(event.target.value))}
                        className="mt-1 h-8 w-full rounded-md border border-border bg-background px-2 text-xs text-foreground"
                      />
                    </label>
                  )}
                  {kind !== "web_dav" && (
                    <label className="text-xs text-muted">
                      Maximum pages
                      <input
                        type="number"
                        min={1}
                        max={200}
                        value={maxPages}
                        onChange={(event) => setMaxPages(Number(event.target.value))}
                        className="mt-1 h-8 w-full rounded-md border border-border bg-background px-2 text-xs text-foreground"
                      />
                    </label>
                  )}
                  {kind === "web_dav" && (
                    <>
                      <label className="text-xs text-muted">
                        Username
                        <input
                          value={webdavUsername}
                          onChange={(event) => setWebdavUsername(event.target.value)}
                          className="mt-1 h-8 w-full rounded-md border border-border bg-background px-2 text-xs text-foreground"
                        />
                      </label>
                      <label className="text-xs text-muted">
                        Password (saved in keychain)
                        <input
                          type="password"
                          value={webdavPassword}
                          onChange={(event) => setWebdavPassword(event.target.value)}
                          className="mt-1 h-8 w-full rounded-md border border-border bg-background px-2 text-xs text-foreground"
                        />
                      </label>
                    </>
                  )}
                  <label className="flex items-center gap-2 text-xs text-muted">
                    <input
                      type="checkbox"
                      checked={allowLoopback}
                      onChange={(event) => setAllowLoopback(event.target.checked)}
                    />
                    Allow loopback HTTP for local testing
                  </label>
                  {kind !== "web_dav" && (
                    <label className="flex items-center gap-2 text-xs text-muted">
                      <input
                        type="checkbox"
                        checked={obeyRobots}
                        onChange={(event) => setObeyRobots(event.target.checked)}
                      />
                      Respect robots.txt
                    </label>
                  )}
                </div>
              )}

              {kind === "selected_chats" && (
                <div className="mt-2 max-h-40 overflow-auto rounded-md border border-border bg-background p-2">
                  {sessions.length === 0 ? (
                    <p className="text-xs text-faint">No conversations are available.</p>
                  ) : (
                    sessions.map((session) => (
                      <label key={session.id} className="flex items-center gap-2 py-1 text-xs text-foreground">
                        <input
                          type="checkbox"
                          checked={selectedChats.includes(session.id)}
                          onChange={(event) =>
                            setSelectedChats((ids) =>
                              event.target.checked
                                ? [...ids, session.id]
                                : ids.filter((id) => id !== session.id),
                            )
                          }
                        />
                        <span className="truncate">{session.title}</span>
                      </label>
                    ))
                  )}
                </div>
              )}

              {formError && <p className="mt-2 text-xs text-danger">{formError}</p>}
              <div className="mt-3 flex justify-end gap-2">
                <Button variant="ghost" size="sm" onClick={resetForm}>Cancel</Button>
                <Button variant="primary" size="sm" disabled={busy} onClick={() => void handleAdd()}>
                  {busy && <Loader2 size={12} className="animate-spin" />} Add source
                </Button>
              </div>
            </div>
          )}

          <div className="mt-3 flex flex-col gap-1.5">
            {stackSources.length === 0 ? (
              <p className="rounded-md border border-dashed border-border p-3 text-center text-xs text-faint">
                Add a source to begin. The existing file/folder index remains available while you migrate.
              </p>
            ) : (
              stackSources.map((source) => (
                <SourceRow
                  key={source.id}
                  source={source}
                  expanded={expandedSource === source.id}
                  onToggle={() => setExpandedSource((id) => (id === source.id ? null : source.id))}
                  onEnabled={(enabled) => void updateSource(source.id, source.label, enabled, source.connector)}
                  onRemove={() => {
                    if (window.confirm(`Remove “${source.label}”? Its chunks will be removed immediately in a new atomic index generation.`)) {
                      void removeSource(source.id);
                    }
                  }}
                />
              ))
            )}
          </div>

          <RetrievalInspector
            stackId={stackId}
            sources={stackSources}
            onQuery={query}
            onCancelQuery={cancelQuery}
            chunkChars={selectedStack?.chunk_chars ?? 1_200}
            chunkOverlap={selectedStack?.chunk_overlap ?? 200}
            onApplyChunking={async (nextChunkChars, nextChunkOverlap) => {
              await updateChunking(stackId, nextChunkChars, nextChunkOverlap);
              await refreshStack(stackId);
            }}
          />
          <PrivacyPreview onPreview={piiPreview} />
        </>
      )}
    </section>
  );
}

function OcrControl({
  status,
  configureExternal,
  install,
  setEnabled,
}: {
  status: () => Promise<KnowledgeOcrConfig>;
  configureExternal: (
    executablePath: string,
    pdfRendererPath: string | null,
    languages: string[],
    lowConfidenceMicros: number,
  ) => Promise<KnowledgeOcrConfig>;
  install: (request: OcrInstallRequest) => Promise<KnowledgeOcrConfig>;
  setEnabled: (enabled: boolean) => Promise<KnowledgeOcrConfig>;
}) {
  const [config, setConfig] = useState<KnowledgeOcrConfig | null>(null);
  const [openPanel, setOpenPanel] = useState(false);
  const [executable, setExecutable] = useState("");
  const [renderer, setRenderer] = useState("");
  const [languages, setLanguages] = useState("eng");
  const [threshold, setThreshold] = useState(80);
  const [download, setDownload] = useState<OcrInstallRequest>({
    url: "",
    version: "",
    expected_sha256: "",
    size_bytes: 0,
    license_name: "Apache-2.0",
    license_url: null,
    provenance: "",
    languages: ["eng"],
  });
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    void status().then((next) => {
      setConfig(next);
      setExecutable(next.executable_path ?? "");
      setRenderer(next.pdf_renderer_path ?? "");
      setLanguages(next.languages.join(", "));
      setThreshold(next.low_confidence_micros / 10_000);
    }).catch((caught) => setError(errorMessage(caught)));
  }, [status]);

  const chooseExecutable = async (setter: (value: string) => void) => {
    const selected = await open({ multiple: false });
    if (typeof selected === "string") setter(selected);
  };

  const parsedLanguages = () => languages.split(/[,+\s]+/).map((value) => value.trim()).filter(Boolean);

  const configure = async () => {
    setBusy(true);
    setError(null);
    try {
      setConfig(await configureExternal(executable, renderer || null, parsedLanguages(), Math.round(threshold * 10_000)));
    } catch (caught) {
      setError(errorMessage(caught));
    } finally {
      setBusy(false);
    }
  };

  const downloadVerified = async () => {
    setBusy(true);
    setError(null);
    try {
      setConfig(await install({ ...download, languages: parsedLanguages() }));
    } catch (caught) {
      setError(errorMessage(caught));
    } finally {
      setBusy(false);
    }
  };

  return (
    <div className="mt-3 rounded-md border border-border bg-surface">
      <button type="button" onClick={() => setOpenPanel((value) => !value)} className="flex w-full items-center gap-2 px-3 py-2 text-left">
        <FileSearch size={14} className="text-accent" />
        <span className="flex-1 text-xs font-medium text-foreground">Local OCR</span>
        <StatusPill tone={config?.enabled ? "success" : config?.asset ? "neutral" : "warning"}>
          {config?.enabled ? `${config.asset?.engine ?? "OCR"} ready` : config?.asset ? "Disabled" : "Not installed"}
        </StatusPill>
        {openPanel ? <ChevronDown size={13} /> : <ChevronRight size={13} />}
      </button>
      {openPanel && (
        <div className="border-t border-border p-3">
          <p className="text-[11px] text-faint">
            Runs only a checksum-bound app-managed or explicitly selected Tesseract-compatible binary. Scanned PDFs also need a selected pdftoppm-compatible renderer. Low-confidence lines remain visibly marked.
          </p>
          <div className="mt-2 grid gap-2 sm:grid-cols-2">
            <label className="text-xs text-muted">
              OCR executable
              <div className="mt-1 flex gap-1">
                <input value={executable} readOnly className="h-8 min-w-0 flex-1 rounded-md border border-border bg-background px-2 font-mono text-[10px] text-foreground" />
                <Button variant="secondary" size="sm" onClick={() => void chooseExecutable(setExecutable)}>Choose</Button>
              </div>
            </label>
            <label className="text-xs text-muted">
              PDF renderer (optional)
              <div className="mt-1 flex gap-1">
                <input value={renderer} readOnly className="h-8 min-w-0 flex-1 rounded-md border border-border bg-background px-2 font-mono text-[10px] text-foreground" />
                <Button variant="secondary" size="sm" onClick={() => void chooseExecutable(setRenderer)}>Choose</Button>
              </div>
            </label>
            <label className="text-xs text-muted">
              Languages
              <input value={languages} onChange={(event) => setLanguages(event.target.value)} className="mt-1 h-8 w-full rounded-md border border-border bg-background px-2 text-xs text-foreground" placeholder="eng, swe" />
            </label>
            <label className="text-xs text-muted">
              Low-confidence threshold: {threshold}%
              <input type="range" min={0} max={100} value={threshold} onChange={(event) => setThreshold(Number(event.target.value))} className="mt-2 w-full" />
            </label>
          </div>
          <div className="mt-2 flex flex-wrap items-center gap-2">
            <Button variant="secondary" size="sm" disabled={busy || !executable} onClick={() => void configure()}>
              Use selected binaries
            </Button>
            {config?.asset && (
              <label className="flex items-center gap-1.5 text-xs text-muted">
                <input
                  type="checkbox"
                  checked={config.enabled}
                  onChange={(event) => void setEnabled(event.target.checked).then(setConfig).catch((caught) => setError(String(caught)))}
                />
                Enable OCR during refresh
              </label>
            )}
          </div>
          {config?.asset && (
            <div className="mt-2 rounded-md bg-background p-2 text-[10px] text-muted">
              <p>{config.asset.engine} {config.asset.engine_version} · {config.asset.license}</p>
              <p className="truncate font-mono">SHA-256 {config.asset.sha256}</p>
              <p className="truncate">{config.asset.provenance}</p>
            </div>
          )}
          <details className="mt-3 text-xs text-muted">
            <summary className="cursor-pointer">Download a verified standalone OCR sidecar</summary>
            <div className="mt-2 grid gap-2 sm:grid-cols-2">
              <label className="sm:col-span-2">HTTPS binary URL<input value={download.url} onChange={(event) => setDownload({ ...download, url: event.target.value })} className="mt-1 h-8 w-full rounded border border-border bg-background px-2 font-mono text-[10px]" /></label>
              <label>Version<input value={download.version} onChange={(event) => setDownload({ ...download, version: event.target.value })} className="mt-1 h-8 w-full rounded border border-border bg-background px-2" /></label>
              <label>Exact bytes<input type="number" min={1} value={download.size_bytes || ""} onChange={(event) => setDownload({ ...download, size_bytes: Number(event.target.value) })} className="mt-1 h-8 w-full rounded border border-border bg-background px-2" /></label>
              <label className="sm:col-span-2">SHA-256<input value={download.expected_sha256} onChange={(event) => setDownload({ ...download, expected_sha256: event.target.value })} className="mt-1 h-8 w-full rounded border border-border bg-background px-2 font-mono text-[10px]" /></label>
              <label>License<input value={download.license_name} onChange={(event) => setDownload({ ...download, license_name: event.target.value })} className="mt-1 h-8 w-full rounded border border-border bg-background px-2" /></label>
              <label>License URL<input value={download.license_url ?? ""} onChange={(event) => setDownload({ ...download, license_url: event.target.value || null })} className="mt-1 h-8 w-full rounded border border-border bg-background px-2" /></label>
              <label className="sm:col-span-2">Publisher / provenance<input value={download.provenance} onChange={(event) => setDownload({ ...download, provenance: event.target.value })} className="mt-1 h-8 w-full rounded border border-border bg-background px-2" /></label>
            </div>
            <Button variant="primary" size="sm" disabled={busy || !download.url || download.expected_sha256.length !== 64 || download.size_bytes <= 0} onClick={() => void downloadVerified()}>
              {busy && <Loader2 size={12} className="animate-spin" />} Download, verify, and activate
            </Button>
          </details>
          {error && <p className="mt-2 text-xs text-danger">{error}</p>}
        </div>
      )}
    </div>
  );
}

function SourceRow({
  source,
  expanded,
  onToggle,
  onEnabled,
  onRemove,
}: {
  source: KnowledgeSourceV2;
  expanded: boolean;
  onToggle: () => void;
  onEnabled: (enabled: boolean) => void;
  onRemove: () => void;
}) {
  return (
    <div className="rounded-md border border-border bg-surface">
      <div className="flex items-center gap-2 px-2 py-2">
        <button type="button" onClick={onToggle} className="text-faint" aria-label="Show source details">
          {expanded ? <ChevronDown size={13} /> : <ChevronRight size={13} />}
        </button>
        <Globe2 size={13} className="shrink-0 text-muted" />
        <div className="min-w-0 flex-1">
          <p className="truncate text-xs font-medium text-foreground">{source.label}</p>
          <p className="truncate font-mono text-[10px] text-faint">{sourceDescription(source)}</p>
        </div>
        <StatusPill tone={source.last_error ? "danger" : source.checkpoint ? "success" : "neutral"}>
          {source.last_error ? "Error" : source.checkpoint ? `${source.objects.length} objects` : "New"}
        </StatusPill>
        <label className="flex items-center gap-1 text-[11px] text-muted">
          <input type="checkbox" checked={source.enabled} onChange={(event) => onEnabled(event.target.checked)} />
          Enabled
        </label>
        <IconButton variant="ghost" size="sm" aria-label="Remove source" onClick={onRemove}>
          <Trash2 size={12} />
        </IconButton>
      </div>
      {expanded && (
        <div className="grid gap-1 border-t border-border px-3 py-2 text-[11px] text-muted sm:grid-cols-2">
          <span>Last refresh: {formatWhen(source.last_refresh_at_ms)}</span>
          <span className="truncate font-mono">checkpoint: {source.checkpoint ?? "none"}</span>
          <span className="truncate font-mono sm:col-span-2">cursor: {source.cursor ?? "none"}</span>
          {source.last_error && <span className="text-danger sm:col-span-2">{source.last_error}</span>}
          {source.retries.length > 0 && (
            <details className="sm:col-span-2">
              <summary className="cursor-pointer">{source.retries.length} retry record(s)</summary>
              <ul className="mt-1 space-y-1">
                {source.retries.slice(-5).map((retry) => (
                  <li key={`${retry.attempted_at_ms}-${retry.message}`}>
                    {formatWhen(retry.attempted_at_ms)} — {retry.message}
                  </li>
                ))}
              </ul>
            </details>
          )}
        </div>
      )}
    </div>
  );
}

function formatMicros(value: number | null): string {
  return value === null ? "—" : (value / 1_000_000).toFixed(4);
}

function formatRankAndScore(rank: number | null, score: number | null): string {
  if (rank === null && score === null) return "—";
  return `${rank ?? "—"} · ${formatMicros(score)}`;
}

function RetrievalInspector({
  stackId,
  sources,
  onQuery,
  onCancelQuery,
  chunkChars,
  chunkOverlap,
  onApplyChunking,
}: {
  stackId: string;
  sources: KnowledgeSourceV2[];
  onQuery: (
    stackId: string,
    query: string,
    config: HybridSearchConfig,
    excludedSourceIds: string[],
    rerank: boolean,
    tokenBudget: number,
    queryId?: string,
  ) => Promise<KnowledgeInspectorResponse>;
  onCancelQuery: (queryId: string) => Promise<boolean>;
  chunkChars: number;
  chunkOverlap: number;
  onApplyChunking: (chunkChars: number, chunkOverlap: number) => Promise<void>;
}) {
  const [open, setOpen] = useState(false);
  const [queryText, setQueryText] = useState("");
  const [config, setConfig] = useState(DEFAULT_HYBRID_CONFIG);
  const [excluded, setExcluded] = useState<string[]>([]);
  const [rerank, setRerank] = useState(false);
  const [tokenBudget, setTokenBudget] = useState(4096);
  const [result, setResult] = useState<KnowledgeInspectorResponse | null>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [copied, setCopied] = useState(false);
  const [activeQueryId, setActiveQueryId] = useState<string | null>(null);
  const [nextChunkChars, setNextChunkChars] = useState(chunkChars);
  const [nextChunkOverlap, setNextChunkOverlap] = useState(chunkOverlap);
  const [chunkingBusy, setChunkingBusy] = useState(false);

  useEffect(() => {
    setNextChunkChars(chunkChars);
    setNextChunkOverlap(chunkOverlap);
  }, [chunkChars, chunkOverlap, stackId]);

  const run = async () => {
    if (busy || !queryText.trim()) return;
    const queryId = crypto.randomUUID();
    setActiveQueryId(queryId);
    setBusy(true);
    setError(null);
    try {
      setResult(await onQuery(stackId, queryText, config, excluded, rerank, tokenBudget, queryId));
    } catch (caught) {
      setError(errorMessage(caught));
    } finally {
      setActiveQueryId(null);
      setBusy(false);
    }
  };

  const cancel = async () => {
    if (!activeQueryId) return;
    try {
      const accepted = await onCancelQuery(activeQueryId);
      if (!accepted) setError("The query already finished before cancellation arrived.");
    } catch (caught) {
      setError(errorMessage(caught));
    }
  };

  const applyChunking = async () => {
    if (nextChunkChars < 1 || nextChunkOverlap < 0 || nextChunkOverlap >= nextChunkChars) {
      setError("Chunk overlap must be zero or greater and smaller than the chunk size.");
      return;
    }
    setChunkingBusy(true);
    setError(null);
    try {
      await onApplyChunking(nextChunkChars, nextChunkOverlap);
      setResult(null);
    } catch (caught) {
      setError(errorMessage(caught));
    } finally {
      setChunkingBusy(false);
    }
  };

  const copyBundle = async () => {
    if (!result) return;
    await navigator.clipboard.writeText(JSON.stringify(result, null, 2));
    setCopied(true);
    window.setTimeout(() => setCopied(false), 1500);
  };

  return (
    <div className="mt-3 rounded-md border border-border bg-surface">
      <button type="button" onClick={() => setOpen((value) => !value)} className="flex w-full items-center gap-2 px-3 py-2 text-left">
        <FileSearch size={14} className="text-accent" />
        <span className="flex-1 text-xs font-medium text-foreground">Retrieval inspector</span>
        {open ? <ChevronDown size={13} /> : <ChevronRight size={13} />}
      </button>
      {open && (
        <div className="border-t border-border p-3">
          <div className="flex gap-2">
            <input
              value={queryText}
              onChange={(event) => setQueryText(event.target.value)}
              onKeyDown={(event) => event.key === "Enter" && void run()}
              className="h-8 min-w-0 flex-1 rounded-md border border-border bg-background px-2 text-xs text-foreground"
              placeholder="Test the exact query sent to retrieval"
            />
            {busy ? (
              <Button variant="danger" size="sm" disabled={!activeQueryId} onClick={() => void cancel()}>
                <Square size={12} /> Stop query
              </Button>
            ) : (
              <Button variant="primary" size="sm" disabled={!queryText.trim()} onClick={() => void run()}>
                <FileSearch size={12} /> Inspect
              </Button>
            )}
          </div>
          <div className="mt-2 grid gap-2 text-[11px] text-muted sm:grid-cols-5">
            <NumberSetting label="Lexical candidates" value={config.lexical_candidates} min={1} max={1000} onChange={(value) => setConfig({ ...config, lexical_candidates: value })} />
            <NumberSetting label="Vector candidates" value={config.vector_candidates} min={1} max={1000} onChange={(value) => setConfig({ ...config, vector_candidates: value })} />
            <NumberSetting label="Final top-k" value={config.final_results} min={1} max={100} onChange={(value) => setConfig({ ...config, final_results: value, rerank_candidates: Math.max(config.rerank_candidates, value) })} />
            <NumberSetting label="Rerank top-k" value={config.rerank_candidates} min={config.final_results} max={1000} onChange={(value) => setConfig({ ...config, rerank_candidates: Math.max(value, config.final_results) })} />
            <NumberSetting label="Token budget" value={tokenBudget} min={128} max={128000} onChange={setTokenBudget} />
          </div>
          <div className="mt-2 rounded-md border border-border bg-background p-2">
            <div className="grid items-end gap-2 text-[11px] text-muted sm:grid-cols-[1fr_1fr_auto]">
              <NumberSetting label="Chunk size (characters)" value={nextChunkChars} min={1} max={1_000_000} onChange={setNextChunkChars} />
              <NumberSetting label="Chunk overlap (characters)" value={nextChunkOverlap} min={0} max={Math.max(0, nextChunkChars - 1)} onChange={setNextChunkOverlap} />
              <Button
                variant="secondary"
                size="sm"
                disabled={chunkingBusy || (nextChunkChars === chunkChars && nextChunkOverlap === chunkOverlap)}
                onClick={() => void applyChunking()}
              >
                {chunkingBusy && <Loader2 size={12} className="animate-spin" />}
                Save & rebuild
              </Button>
            </div>
            <p className="mt-1 text-[10px] text-faint">A rebuild publishes atomically; the current index remains usable if refresh is cancelled or fails.</p>
          </div>
          <div className="mt-2 flex flex-wrap gap-3 text-[11px] text-muted">
            <label className="flex items-center gap-1.5">
              <input type="checkbox" checked={rerank} onChange={(event) => setRerank(event.target.checked)} />
              Local reranker (no cloud call)
            </label>
            {sources.map((source) => (
              <label key={source.id} className="flex items-center gap-1.5">
                <input
                  type="checkbox"
                  checked={excluded.includes(source.id)}
                  onChange={(event) => setExcluded((ids) => event.target.checked ? [...ids, source.id] : ids.filter((id) => id !== source.id))}
                />
                Exclude {source.label}
              </label>
            ))}
          </div>
          {error && <p className="mt-2 text-xs text-danger">{error}</p>}
          {result && (
            <div className="mt-3 space-y-2">
              <div className="flex flex-wrap items-center gap-2 text-[11px] text-muted">
                <StatusPill tone="success">{result.search.hits.length} results</StatusPill>
                {result.search.hits.some((hit) => hit.chunk.low_confidence) && (
                  <StatusPill tone="warning">
                    {result.search.hits.filter((hit) => hit.chunk.low_confidence).length} low-confidence OCR
                  </StatusPill>
                )}
                <span>{result.estimated_context_tokens}/{result.token_budget} estimated tokens</span>
                <span>normalized: <code className="text-foreground">{result.normalized_query}</code></span>
                <span className="font-mono">query {result.query_id.slice(0, 12)}</span>
                <span className="font-mono">trace {result.search.diagnostics.trace_sha256.slice(0, 12)}</span>
                <Button variant="ghost" size="sm" onClick={() => void copyBundle()}>
                  {copied ? <Check size={12} /> : <Clipboard size={12} />} {copied ? "Copied" : "Copy diagnostic"}
                </Button>
              </div>
              <div className="max-h-64 overflow-auto rounded-md border border-border bg-background">
                <table className="w-full text-left text-[10px]">
                  <thead className="sticky top-0 bg-surface text-faint">
                    <tr><th className="p-1.5">Final</th><th>Lexical</th><th>Vector</th><th>Fused</th><th>Rerank</th><th>Source / preview</th></tr>
                  </thead>
                  <tbody>
                    {result.search.diagnostics.candidates.map((candidate) => (
                      <tr key={candidate.chunk_id} className={`border-t border-border align-top text-muted ${candidate.low_confidence ? "bg-warning-soft/40" : ""}`}>
                        <td className="p-1.5">{candidate.final_rank ?? "—"}</td>
                        <td title="rank · raw BM25 score">{formatRankAndScore(candidate.lexical_rank, candidate.lexical_bm25_micros)}</td>
                        <td title="rank · cosine similarity">{formatRankAndScore(candidate.vector_rank, candidate.vector_similarity_micros)}</td>
                        <td>{candidate.fused_score_units}</td>
                        <td>{formatMicros(candidate.rerank_score_micros)}</td>
                        <td className="max-w-xs p-1.5">
                          <div className="flex min-w-0 items-center gap-1">
                            {candidate.low_confidence && (
                              <span className="shrink-0 rounded border border-warning/40 bg-warning-soft px-1 text-[9px] font-semibold uppercase text-warning">
                                Low OCR {candidate.confidence_micros === null ? "" : `${(candidate.confidence_micros / 10_000).toFixed(1)}%`}
                              </span>
                            )}
                            <p className="truncate font-mono text-faint">{candidate.citation.canonical_uri}</p>
                          </div>
                          <p className="line-clamp-2 text-foreground">{candidate.content_preview}</p>
                          <p className="truncate font-mono text-faint">{candidate.content_type || "legacy"} · {JSON.stringify(candidate.citation.location)}</p>
                        </td>
                      </tr>
                    ))}
                  </tbody>
                </table>
              </div>
              <details>
                <summary className="cursor-pointer text-xs text-muted">Final assembled context</summary>
                <pre className="mt-1 max-h-48 overflow-auto whitespace-pre-wrap rounded-md bg-background p-2 text-[11px] text-foreground">{result.final_context}</pre>
              </details>
            </div>
          )}
        </div>
      )}
    </div>
  );
}

function NumberSetting({ label, value, min, max, onChange }: { label: string; value: number; min: number; max: number; onChange: (value: number) => void }) {
  return (
    <label>
      {label}
      <input type="number" min={min} max={max} value={value} onChange={(event) => onChange(Number(event.target.value))} className="mt-1 h-7 w-full rounded border border-border bg-background px-1.5 text-xs text-foreground" />
    </label>
  );
}

function PrivacyPreview({ onPreview }: { onPreview: (text: string) => Promise<PiiPreview> }) {
  const [open, setOpen] = useState(false);
  const [text, setText] = useState("");
  const [result, setResult] = useState<PiiPreview | null>(null);
  const [error, setError] = useState<string | null>(null);

  const scan = async () => {
    setError(null);
    try {
      setResult(await onPreview(text));
    } catch (caught) {
      setError(errorMessage(caught));
    }
  };

  return (
    <div className="mt-2 rounded-md border border-border bg-surface">
      <button type="button" onClick={() => setOpen((value) => !value)} className="flex w-full items-center gap-2 px-3 py-2 text-left">
        <ShieldCheck size={14} className="text-accent" />
        <span className="flex-1 text-xs font-medium text-foreground">Cloud privacy preview</span>
        {open ? <ChevronDown size={13} /> : <ChevronRight size={13} />}
      </button>
      {open && (
        <div className="border-t border-border p-3">
          <p className="mb-2 text-[11px] text-faint">Scan and preview redaction locally before explicitly sending content to a configured cloud embedding or reranking provider.</p>
          <textarea value={text} onChange={(event) => setText(event.target.value)} className="h-24 w-full resize-y rounded-md border border-border bg-background p-2 text-xs text-foreground" placeholder="Paste a sample to scan locally" />
          <div className="mt-2 flex items-center gap-2">
            <Button variant="secondary" size="sm" disabled={!text} onClick={() => void scan()}>Scan locally</Button>
            {result && <StatusPill tone={result.findings.length ? "warning" : "success"}>{result.findings.length} finding(s)</StatusPill>}
          </div>
          {error && <p className="mt-2 text-xs text-danger">{error}</p>}
          {result && (
            <div className="mt-2 grid gap-2 sm:grid-cols-2">
              <div className="rounded-md bg-background p-2">
                <p className="mb-1 text-[11px] font-medium text-muted">Findings (masked)</p>
                {result.findings.length === 0 ? <p className="text-xs text-faint">No supported PII or secret pattern found.</p> : result.findings.map((finding) => (
                  <p key={`${finding.kind}-${finding.byte_start}`} className="flex items-center gap-1 text-[11px] text-warning"><AlertTriangle size={10} /> {finding.kind} at {finding.line}:{finding.column} — {finding.masked_preview}</p>
                ))}
              </div>
              <pre className="max-h-40 overflow-auto whitespace-pre-wrap rounded-md bg-background p-2 text-[11px] text-foreground">{result.redacted_text}</pre>
            </div>
          )}
        </div>
      )}
    </div>
  );
}
