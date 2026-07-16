import { useEffect } from "react";
import { AlertTriangle, CheckCircle2, Clock, RefreshCw, Server, X, Zap } from "lucide-react";

import { useT } from "../../lib/i18n";
import { useDailyBriefStore } from "../../store/dailyBriefStore";
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
                  key={item.connectorId}
                  title={item.label}
                  subtitle={item.summary}
                  action={{ label: t("DailyBriefPanel.connectors.open"), onClick: () => onOpenSettingsTab("mcp") }}
                />
              ))}
            </Section>
          )}

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
