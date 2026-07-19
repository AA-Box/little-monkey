import { useEffect, useMemo, useState } from "react";
import { AlertTriangle, CheckCircle2, Clock, Plug, Plus, RefreshCw, Server, Trash2, X, Zap } from "lucide-react";

import { useT } from "../../lib/i18n";
import { isReadOnlyBriefTool, useDailyBriefStore } from "../../store/dailyBriefStore";
import { useMcpStore } from "../../store/mcpStore";
import { IconButton, StatusPill, type PillTone } from "../ui";
import type { SettingsTab } from "../Settings";
import type { RiskLevel, RunStatus } from "../../lib/runProtocol";

interface DailyBriefPanelProps {
  onClose: () => void;
  onOpenRunCenter: (runId: string) => void;
  onOpenAgentInbox: () => void;
  onOpenSettingsTab: (tab: SettingsTab) => void;
}

const RISK_TONE: Record<RiskLevel, PillTone> = {
  low: "success",
  medium: "warning",
  high: "danger",
};

const RUN_STATUS_LABEL_KEY: Record<RunStatus, string> = {
  queued: "DailyBriefPanel.runStatus.queued",
  running: "DailyBriefPanel.runStatus.running",
  waiting_for_permission: "DailyBriefPanel.runStatus.waiting_for_permission",
  paused: "DailyBriefPanel.runStatus.paused",
  cancelling: "DailyBriefPanel.runStatus.cancelling",
  succeeded: "DailyBriefPanel.runStatus.succeeded",
  failed: "DailyBriefPanel.runStatus.failed",
  cancelled: "DailyBriefPanel.runStatus.cancelled",
  needs_reconciliation: "DailyBriefPanel.runStatus.needs_reconciliation",
};

function formatBytes(bytes: number | null): string {
  if (bytes == null) return "—";
  if (bytes < 1024) return `${bytes} B`;
  const units = ["KB", "MB", "GB", "TB"];
  let value = bytes / 1024;
  let unitIndex = 0;
  while (value >= 1024 && unitIndex < units.length - 1) {
    value /= 1024;
    unitIndex += 1;
  }
  return `${value.toFixed(1)} ${units[unitIndex]}`;
}

function formatDurationHours(ms: number): number {
  return Math.round(ms / (60 * 60 * 1000));
}

/**
 * Section shell: a heading, an optional "open elsewhere" action, and either
 * the section's rows or an empty-state message. Every section in this panel
 * is read-only — the action button always navigates to the real panel that
 * can act (Run Center, Agent Inbox, Settings), it never performs the action
 * itself, per ROADMAP.md's "Daily Brief and Command Center" acceptance
 * criteria.
 */
function Section({
  icon,
  title,
  count,
  emptyLabel,
  children,
}: {
  icon: React.ReactNode;
  title: string;
  count: number;
  emptyLabel: string;
  children: React.ReactNode;
}) {
  return (
    <section className="rounded-lg border border-border bg-background p-4" aria-label={title}>
      <div className="flex items-center gap-2 text-foreground">
        {icon}
        <h2 className="text-sm font-semibold">{title}</h2>
        <span className="text-xs text-faint">({count})</span>
      </div>
      <div className="mt-3 flex flex-col gap-2">
        {count === 0 ? <p className="text-sm text-muted">{emptyLabel}</p> : children}
      </div>
    </section>
  );
}

function Row({
  title,
  subtitle,
  tone,
  action,
}: {
  title: string;
  subtitle?: string;
  tone?: PillTone;
  action: { label: string; onClick: () => void };
}) {
  return (
    <div className="flex items-center justify-between gap-3 rounded-md border border-border bg-surface px-3 py-2">
      <div className="min-w-0 flex-1">
        <p className="truncate text-sm font-medium text-foreground">{title}</p>
        {subtitle && <p className="truncate text-xs text-muted">{subtitle}</p>}
      </div>
      {tone && <StatusPill tone={tone} />}
      <button
        type="button"
        onClick={action.onClick}
        className="shrink-0 rounded-md border border-border px-2.5 py-1 text-xs font-medium text-foreground hover:bg-surface-2"
      >
        {action.label}
      </button>
    </div>
  );
}

export function DailyBriefPanel({ onClose, onOpenRunCenter, onOpenAgentInbox, onOpenSettingsTab }: DailyBriefPanelProps) {
  const { t } = useT();
  const brief = useDailyBriefStore((state) => state);
  const refresh = useDailyBriefStore((state) => state.refresh);
  const saveConnectorSource = useDailyBriefStore((state) => state.saveConnectorSource);
  const removeConnectorSource = useDailyBriefStore((state) => state.removeConnectorSource);
  const setConnectorSourceEnabled = useDailyBriefStore((state) => state.setConnectorSourceEnabled);
  const servers = useMcpStore((state) => state.servers);
  const [sourceServerId, setSourceServerId] = useState("");
  const [sourceToolName, setSourceToolName] = useState("");
  const [sourceLabel, setSourceLabel] = useState("");
  const [sourceArguments, setSourceArguments] = useState("{}");
  const [sourceError, setSourceError] = useState<string | null>(null);

  const availableSources = useMemo(() => servers
    .filter((server) => server.enabled && server.status === "connected")
    .flatMap((server) => server.tools
      .filter((tool) => isReadOnlyBriefTool(tool.name))
      .filter((tool) => !server.toolAllowlist || server.toolAllowlist.includes(tool.name))
      .map((tool) => ({ serverId: server.id, serverLabel: server.label, toolName: tool.name }))), [servers]);

  useEffect(() => {
    if (availableSources.length === 0) {
      setSourceServerId("");
      setSourceToolName("");
      return;
    }
    if (!availableSources.some((source) => source.serverId === sourceServerId && source.toolName === sourceToolName)) {
      setSourceServerId(availableSources[0].serverId);
      setSourceToolName(availableSources[0].toolName);
    }
  }, [availableSources, sourceServerId, sourceToolName]);

  function addConnectorSource() {
    setSourceError(null);
    try {
      const args: unknown = JSON.parse(sourceArguments);
      if (!args || typeof args !== "object" || Array.isArray(args)) {
        throw new Error(t("DailyBriefPanel.connectors.argumentsObjectError"));
      }
      const selected = availableSources.find((source) =>
        source.serverId === sourceServerId && source.toolName === sourceToolName
      );
      if (!selected) throw new Error(t("DailyBriefPanel.connectors.noReadTool"));
      saveConnectorSource({
        serverId: selected.serverId,
        toolName: selected.toolName,
        label: sourceLabel.trim() || `${selected.serverLabel} · ${selected.toolName}`,
        arguments: args as Record<string, unknown>,
        enabled: true,
      });
      setSourceLabel("");
      setSourceArguments("{}");
    } catch (error) {
      setSourceError(error instanceof Error ? error.message : String(error));
    }
  }

  useEffect(() => {
    void refresh();
  }, [refresh]);

  return (
    <section className="flex min-h-0 flex-1 flex-col" aria-labelledby="daily-brief-title">
      <header className="flex shrink-0 items-center justify-between gap-3 border-b border-border px-4 py-3">
        <div className="min-w-0">
          <h1 id="daily-brief-title" className="text-base font-semibold text-foreground">
            {t("DailyBriefPanel.title")}
          </h1>
          <p className="truncate text-xs text-muted">{t("DailyBriefPanel.subtitle")}</p>
        </div>
        <div className="flex shrink-0 items-center gap-1">
          <IconButton
            size="sm"
            onClick={() => void refresh()}
            aria-label={t("DailyBriefPanel.refresh")}
            disabled={brief.loading}
          >
            <RefreshCw size={15} className={brief.loading ? "animate-spin" : ""} />
          </IconButton>
          <IconButton size="sm" onClick={onClose} aria-label={t("DailyBriefPanel.close")}>
            <X size={16} />
          </IconButton>
        </div>
      </header>

      {brief.error && (
        <div role="alert" className="border-b border-danger/30 bg-danger-soft px-4 py-2 text-xs text-danger">
          {t("DailyBriefPanel.loadError", { error: brief.error })}
        </div>
      )}

      <div className="min-h-0 flex-1 overflow-y-auto px-4 py-4">
        <div className="mx-auto flex max-w-3xl flex-col gap-4">
          {brief.lastRefreshedAtMs != null && (
            <p className="text-xs text-faint">
              {t("DailyBriefPanel.generatedAt", { time: new Date(brief.lastRefreshedAtMs).toLocaleString() })}
            </p>
          )}

          <Section
            icon={<AlertTriangle size={16} className="text-warning" aria-hidden="true" />}
            title={t("DailyBriefPanel.pendingApprovals.title")}
            count={brief.pendingApprovals.length}
            emptyLabel={t("DailyBriefPanel.pendingApprovals.empty")}
          >
            {brief.pendingApprovals.map((item) => (
              <Row
                key={item.id}
                title={item.title}
                subtitle={item.detail}
                tone={item.riskLevel ? RISK_TONE[item.riskLevel] : undefined}
                action={{ label: t("DailyBriefPanel.pendingApprovals.openInbox"), onClick: onOpenAgentInbox }}
              />
            ))}
          </Section>

          <Section
            icon={<Zap size={16} className="text-accent" aria-hidden="true" />}
            title={t("DailyBriefPanel.running.title")}
            count={brief.running.length}
            emptyLabel={t("DailyBriefPanel.running.empty")}
          >
            {brief.running.map((item) => (
              <Row
                key={item.id}
                title={item.title}
                subtitle={`${item.subtitle} · ${t(RUN_STATUS_LABEL_KEY[item.status])}`}
                action={{ label: t("DailyBriefPanel.running.open"), onClick: () => onOpenRunCenter(item.runId) }}
              />
            ))}
          </Section>

          <Section
            icon={<AlertTriangle size={16} className="text-danger" aria-hidden="true" />}
            title={t("DailyBriefPanel.failedJobs.title")}
            count={brief.failedScheduledJobs.length}
            emptyLabel={t("DailyBriefPanel.failedJobs.empty")}
          >
            {brief.failedScheduledJobs.map((item) => (
              <Row
                key={item.id}
                title={item.recipeName}
                subtitle={t("DailyBriefPanel.failedJobs.lastRun", {
                  time: new Date(item.lastRunAt).toLocaleString(),
                  status: t(`DailyBriefPanel.failedJobs.status.${item.lastStatus}`),
                })}
                tone="danger"
                action={{ label: t("DailyBriefPanel.failedJobs.open"), onClick: () => onOpenSettingsTab("automation") }}
              />
            ))}
          </Section>

          <Section
            icon={<CheckCircle2 size={16} className="text-success" aria-hidden="true" />}
            title={t("DailyBriefPanel.completed.title")}
            count={brief.recentlyCompleted.length}
            emptyLabel={t("DailyBriefPanel.completed.empty")}
          >
            {brief.recentlyCompleted.map((item) => (
              <Row
                key={item.id}
                title={item.title}
                subtitle={new Date(item.updatedAtMs).toLocaleString()}
                action={{ label: t("DailyBriefPanel.completed.open"), onClick: () => onOpenRunCenter(item.runId) }}
              />
            ))}
          </Section>

          <Section
            icon={<Clock size={16} className="text-muted" aria-hidden="true" />}
            title={t("DailyBriefPanel.stale.title")}
            count={brief.staleTasks.length}
            emptyLabel={t("DailyBriefPanel.stale.empty")}
          >
            {brief.staleTasks.map((item) => (
              <Row
                key={item.id}
                title={item.title}
                subtitle={t("DailyBriefPanel.stale.description", { hours: formatDurationHours(item.staleForMs) })}
                tone="warning"
                action={{ label: t("DailyBriefPanel.stale.open"), onClick: () => onOpenRunCenter(item.runId) }}
              />
            ))}
          </Section>

          {/* Connector-sourced highlights: omitted entirely (not even an
              empty-state message) whenever there's nothing real to show —
              see dailyBriefStore.ts's buildConnectorHighlights doc comment
              for why that's always the case today. */}
          {brief.connectorHighlights.length > 0 && (
            <Section
              icon={<Zap size={16} className="text-accent" aria-hidden="true" />}
              title={t("DailyBriefPanel.connectors.title")}
              count={brief.connectorHighlights.length}
              emptyLabel={t("DailyBriefPanel.connectors.empty")}
            >
              {brief.connectorHighlights.map((item) => (
                <Row
                  key={item.id}
                  title={item.label}
                  subtitle={`${item.summary} · ${item.connectorId}/${item.toolName} · ${new Date(item.fetchedAtMs).toLocaleTimeString()}`}
                  tone={item.status === "ok" ? "success" : "danger"}
                  action={{ label: t("DailyBriefPanel.connectors.open"), onClick: () => onOpenSettingsTab("mcp") }}
                />
              ))}
            </Section>
          )}

          <section className="rounded-lg border border-border bg-background p-4" aria-labelledby="daily-brief-connector-sources-title">
            <div className="flex items-center gap-2">
              <Plug size={16} className="text-muted" aria-hidden="true" />
              <div>
                <h2 id="daily-brief-connector-sources-title" className="text-sm font-semibold text-foreground">
                  {t("DailyBriefPanel.connectors.sourcesTitle")}
                </h2>
                <p className="mt-0.5 text-xs text-muted">{t("DailyBriefPanel.connectors.sourcesDescription")}</p>
              </div>
            </div>

            {brief.connectorSources.length > 0 && (
              <ul className="mt-3 space-y-2">
                {brief.connectorSources.map((source) => (
                  <li key={source.id} className="flex items-center gap-2 rounded-md border border-border bg-surface px-3 py-2">
                    <label className="flex min-w-0 flex-1 items-center gap-2 text-xs text-foreground">
                      <input
                        type="checkbox"
                        checked={source.enabled}
                        onChange={(event) => setConnectorSourceEnabled(source.id, event.target.checked)}
                        className="h-4 w-4 accent-[var(--color-accent)]"
                      />
                      <span className="min-w-0"><span className="block truncate font-medium">{source.label}</span><span className="block truncate font-mono text-[10px] text-faint">{source.serverId}/{source.toolName}</span></span>
                    </label>
                    <button type="button" onClick={() => removeConnectorSource(source.id)} aria-label={t("DailyBriefPanel.connectors.removeSource")} className="rounded p-1 text-faint hover:bg-surface-2 hover:text-danger">
                      <Trash2 size={14} />
                    </button>
                  </li>
                ))}
              </ul>
            )}

            <div className="mt-3 grid gap-2 sm:grid-cols-2">
              <label className="text-xs text-muted">
                {t("DailyBriefPanel.connectors.readTool")}
                <select
                  value={`${sourceServerId}\u0000${sourceToolName}`}
                  onChange={(event) => {
                    const [serverId, toolName] = event.target.value.split("\u0000");
                    setSourceServerId(serverId ?? "");
                    setSourceToolName(toolName ?? "");
                  }}
                  className="mt-1 h-9 w-full rounded-md border border-border bg-surface px-2 text-xs text-foreground"
                  disabled={availableSources.length === 0}
                >
                  {availableSources.length === 0
                    ? <option value="">{t("DailyBriefPanel.connectors.noReadTool")}</option>
                    : availableSources.map((source) => <option key={`${source.serverId}:${source.toolName}`} value={`${source.serverId}\u0000${source.toolName}`}>{source.serverLabel} · {source.toolName}</option>)}
                </select>
              </label>
              <label className="text-xs text-muted">
                {t("DailyBriefPanel.connectors.sourceLabel")}
                <input value={sourceLabel} onChange={(event) => setSourceLabel(event.target.value)} className="mt-1 h-9 w-full rounded-md border border-border bg-surface px-2 text-xs text-foreground" />
              </label>
              <label className="text-xs text-muted sm:col-span-2">
                {t("DailyBriefPanel.connectors.arguments")}
                <textarea value={sourceArguments} onChange={(event) => setSourceArguments(event.target.value)} className="mt-1 min-h-20 w-full rounded-md border border-border bg-surface p-2 font-mono text-xs text-foreground" />
              </label>
            </div>
            {sourceError && <p role="alert" className="mt-2 text-xs text-danger">{sourceError}</p>}
            <button type="button" onClick={addConnectorSource} disabled={availableSources.length === 0} className="mt-2 inline-flex min-h-9 items-center gap-1.5 rounded-md bg-accent px-3 text-xs font-medium text-accent-foreground disabled:opacity-50">
              <Plus size={13} /> {t("DailyBriefPanel.connectors.addSource")}
            </button>
          </section>

          {brief.runtimeHealth.hasData && (
            <section className="rounded-lg border border-border bg-background p-4" aria-label={t("DailyBriefPanel.runtime.title")}>
              <div className="flex items-center justify-between gap-2">
                <div className="flex items-center gap-2 text-foreground">
                  <Server size={16} className="text-muted" aria-hidden="true" />
                  <h2 className="text-sm font-semibold">{t("DailyBriefPanel.runtime.title")}</h2>
                </div>
                <button
                  type="button"
                  onClick={() => onOpenSettingsTab("runtimehub")}
                  className="shrink-0 rounded-md border border-border px-2.5 py-1 text-xs font-medium text-foreground hover:bg-surface-2"
                >
                  {t("DailyBriefPanel.runtime.open")}
                </button>
              </div>
              <div className="mt-3 flex flex-col gap-1 text-sm text-foreground">
                <p>
                  {t("DailyBriefPanel.runtime.summary", {
                    ready: brief.runtimeHealth.inferenceReadyCount,
                    total: brief.runtimeHealth.nodes.length,
                  })}
                </p>
                {(brief.runtimeHealth.storageUsedBytes != null || brief.runtimeHealth.storageQuotaBytes != null) && (
                  <p className="text-xs text-muted">
                    {t("DailyBriefPanel.runtime.storage", {
                      used: formatBytes(brief.runtimeHealth.storageUsedBytes),
                      quota: formatBytes(brief.runtimeHealth.storageQuotaBytes),
                    })}
                  </p>
                )}
                {brief.runtimeHealth.overviewError && (
                  <p className="text-xs text-danger">
                    {t("DailyBriefPanel.runtime.error", { error: brief.runtimeHealth.overviewError })}
                  </p>
                )}
              </div>
            </section>
          )}
        </div>
      </div>
    </section>
  );
}

export default DailyBriefPanel;
