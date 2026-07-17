import { useMemo, useState } from "react";
import {
  AlertTriangle,
  CheckCircle2,
  CircleDot,
  CircleSlash,
  Download,
  Loader2,
  RefreshCw,
  Stethoscope,
  Wrench,
} from "lucide-react";
import { Button } from "../ui";
import {
  useDiagnosticsStore,
  type DiagnosticFinding,
  type DiagnosticStatus,
} from "../../store/diagnosticsStore";
import { useT } from "../../lib/i18n";

const STATUS_ORDER: Record<DiagnosticStatus, number> = {
  critical: 0,
  warning: 1,
  fixed: 2,
  info: 3,
  not_configured: 4,
  pass: 5,
};

const STATUS_STYLE: Record<DiagnosticStatus, string> = {
  critical: "border-danger/40 bg-danger/10 text-danger",
  warning: "border-warning/40 bg-warning/10 text-warning",
  fixed: "border-success/40 bg-success/10 text-success",
  pass: "border-success/30 bg-success/5 text-success",
  info: "border-border bg-surface-2 text-muted",
  not_configured: "border-border bg-surface-2 text-faint",
};

const SUBSYSTEM_LABEL_KEYS: Record<string, string> = {
  ollama: "DiagnosticsPanel.subsystemOllama",
  llama: "DiagnosticsPanel.subsystemLlama",
  embed_llama: "DiagnosticsPanel.subsystemEmbedLlama",
  api_server: "DiagnosticsPanel.subsystemApiServer",
  mcp: "DiagnosticsPanel.subsystemMcp",
  knowledge_index: "DiagnosticsPanel.subsystemKnowledgeIndex",
  automation_daemon: "DiagnosticsPanel.subsystemAutomationDaemon",
  keychain: "DiagnosticsPanel.subsystemKeychain",
  connectors: "DiagnosticsPanel.subsystemConnectors",
  remote_pairing: "DiagnosticsPanel.subsystemRemotePairing",
};

function StatusIcon({ status }: { status: DiagnosticStatus }) {
  if (status === "critical") return <AlertTriangle size={15} />;
  if (status === "warning") return <AlertTriangle size={15} />;
  if (status === "fixed") return <Wrench size={15} />;
  if (status === "pass") return <CheckCircle2 size={15} />;
  if (status === "not_configured") return <CircleSlash size={15} />;
  return <CircleDot size={15} />;
}

function FindingCard({ finding }: { finding: DiagnosticFinding }) {
  const { t } = useT();
  const busy = useDiagnosticsStore((s) => s.busy[`fix-${finding.id}`] ?? false);
  const applyFix = useDiagnosticsStore((s) => s.applyFix);
  const statusKey: Record<DiagnosticStatus, string> = {
    pass: "DiagnosticsPanel.statusPass",
    info: "DiagnosticsPanel.statusInfo",
    warning: "DiagnosticsPanel.statusWarning",
    critical: "DiagnosticsPanel.statusCritical",
    fixed: "DiagnosticsPanel.statusFixed",
    not_configured: "DiagnosticsPanel.statusNotConfigured",
  };

  return (
    <article className="rounded-lg border border-border bg-surface p-3">
      <div className="flex items-start gap-3">
        <span
          className={`mt-0.5 inline-flex shrink-0 items-center gap-1 rounded-full border px-2 py-1 text-[10px] font-semibold uppercase tracking-wide ${STATUS_STYLE[finding.status]}`}
        >
          <StatusIcon status={finding.status} />
          {t(statusKey[finding.status])}
        </span>
        <div className="min-w-0 flex-1">
          <div className="flex flex-wrap items-start justify-between gap-2">
            <h5 className="text-xs font-semibold text-foreground">{finding.title}</h5>
            {finding.fixable && finding.status !== "fixed" && (
              <span className="rounded-full border border-border px-2 py-0.5 text-[10px] text-faint">
                {t("DiagnosticsPanel.fixableBadge")}
              </span>
            )}
          </div>
          <p className="mt-1 text-xs leading-5 text-muted">{finding.detail}</p>
          {finding.remediation && (
            <p className="mt-2 text-[11px] leading-4 text-foreground">
              <span className="font-medium">{t("DiagnosticsPanel.remediationLabel")}:</span> {finding.remediation}
            </p>
          )}
          {finding.fixable && finding.status !== "fixed" && (
            <div className="mt-2">
              <Button
                size="sm"
                variant="secondary"
                disabled={busy}
                onClick={() => void applyFix(finding.id)}
              >
                {busy ? <Loader2 size={12} className="animate-spin" /> : <Wrench size={12} />}
                {t("DiagnosticsPanel.applyFix")}
              </Button>
            </div>
          )}
        </div>
      </div>
    </article>
  );
}

export function DiagnosticsPanel() {
  const { t } = useT();
  const report = useDiagnosticsStore((s) => s.report);
  const bundle = useDiagnosticsStore((s) => s.bundle);
  const error = useDiagnosticsStore((s) => s.error);
  const busyRun = useDiagnosticsStore((s) => s.busy.run ?? false);
  const busyExport = useDiagnosticsStore((s) => s.busy.export ?? false);
  const run = useDiagnosticsStore((s) => s.run);
  const exportBundle = useDiagnosticsStore((s) => s.exportBundle);
  const dismissBundle = useDiagnosticsStore((s) => s.dismissBundle);
  const clearError = useDiagnosticsStore((s) => s.clearError);
  const [copied, setCopied] = useState(false);

  const groups = useMemo(() => {
    const grouped = new Map<string, DiagnosticFinding[]>();
    for (const finding of report?.findings ?? []) {
      const current = grouped.get(finding.subsystem) ?? [];
      current.push(finding);
      grouped.set(finding.subsystem, current);
    }
    return [...grouped.entries()]
      .map(([subsystem, findings]) => ({
        subsystem,
        findings: [...findings].sort(
          (left, right) => STATUS_ORDER[left.status] - STATUS_ORDER[right.status] || left.title.localeCompare(right.title),
        ),
      }))
      .sort((left, right) => left.subsystem.localeCompare(right.subsystem));
  }, [report]);

  const healthy = report ? report.summary.critical === 0 && report.summary.warnings === 0 : false;

  async function handleRun() {
    try {
      await run();
    } catch {
      // Surfaced via the store's `error` field below.
    }
  }

  async function handleExport() {
    setCopied(false);
    try {
      await exportBundle();
    } catch {
      // Surfaced via the store's `error` field below.
    }
  }

  async function handleCopyBundle() {
    if (!bundle) return;
    try {
      await navigator.clipboard.writeText(JSON.stringify(bundle, null, 2));
      setCopied(true);
    } catch {
      setCopied(false);
    }
  }

  return (
    <section className="flex flex-col gap-4" aria-labelledby="diagnostics-heading">
      <div className="flex items-start gap-3">
        <span className="rounded-lg border border-accent/30 bg-accent/10 p-2 text-accent">
          <Stethoscope size={20} />
        </span>
        <div>
          <h3 id="diagnostics-heading" className="text-sm font-semibold text-foreground">
            {t("DiagnosticsPanel.title")}
          </h3>
          <p className="mt-1 text-xs leading-5 text-muted">{t("DiagnosticsPanel.description")}</p>
        </div>
      </div>

      <div className="rounded-lg border border-border bg-surface p-3">
        <div className="flex flex-wrap gap-2">
          <Button variant="primary" disabled={busyRun} onClick={() => void handleRun()}>
            {busyRun ? <Loader2 size={14} className="animate-spin" /> : <RefreshCw size={14} />}
            {report ? t("DiagnosticsPanel.rerun") : t("DiagnosticsPanel.runDiagnosis")}
          </Button>
          <Button variant="secondary" disabled={busyExport} onClick={() => void handleExport()}>
            {busyExport ? <Loader2 size={14} className="animate-spin" /> : <Download size={14} />}
            {t("DiagnosticsPanel.exportBundle")}
          </Button>
        </div>
      </div>

      {bundle && (
        <div className="rounded-lg border border-border bg-surface p-3">
          <div className="flex flex-wrap items-start justify-between gap-2">
            <div>
              <h4 className="text-xs font-semibold text-foreground">{t("DiagnosticsPanel.bundleTitle")}</h4>
              <p className="mt-1 text-xs leading-5 text-muted">{t("DiagnosticsPanel.bundleDescription")}</p>
            </div>
            <div className="flex shrink-0 gap-2">
              <Button size="sm" variant="secondary" onClick={() => void handleCopyBundle()}>
                {copied ? t("DiagnosticsPanel.bundleCopied") : t("DiagnosticsPanel.bundleCopy")}
              </Button>
              <Button size="sm" variant="ghost" onClick={dismissBundle}>
                {t("DiagnosticsPanel.dismiss")}
              </Button>
            </div>
          </div>
          <pre className="mt-2 max-h-48 overflow-auto rounded-md bg-background px-2 py-1 text-[10px] text-faint">
            {JSON.stringify(bundle, null, 2)}
          </pre>
        </div>
      )}

      {report && (
        <>
          <div
            role="status"
            className={`rounded-lg border p-3 ${
              healthy ? "border-success/40 bg-success/10" : report.summary.critical > 0 ? "border-danger/40 bg-danger/10" : "border-warning/40 bg-warning/10"
            }`}
          >
            <div className="flex flex-wrap items-center justify-between gap-2">
              <p className="text-sm font-semibold text-foreground">
                {healthy
                  ? t("DiagnosticsPanel.healthyStatus")
                  : t("DiagnosticsPanel.unhealthyStatus", {
                      critical: String(report.summary.critical),
                      warnings: String(report.summary.warnings),
                    })}
              </p>
              <span className="text-[10px] text-faint">
                {t("DiagnosticsPanel.generatedAt", { time: new Date(report.generatedAtMs).toLocaleString() })}
              </span>
            </div>
            <div className="mt-3 grid grid-cols-2 gap-2 sm:grid-cols-6">
              {[
                ["DiagnosticsPanel.summaryCritical", report.summary.critical, "text-danger"],
                ["DiagnosticsPanel.summaryWarnings", report.summary.warnings, "text-warning"],
                ["DiagnosticsPanel.summaryFixed", report.summary.fixed, "text-success"],
                ["DiagnosticsPanel.summaryPassed", report.summary.passed, "text-success"],
                ["DiagnosticsPanel.summaryInfo", report.summary.informational, "text-muted"],
                ["DiagnosticsPanel.summaryNotConfigured", report.summary.notConfigured, "text-faint"],
              ].map(([labelKey, count, color]) => (
                <div key={String(labelKey)} className="rounded-md border border-border/70 bg-background/60 p-2">
                  <p className="text-[10px] uppercase tracking-wide text-faint">{t(String(labelKey))}</p>
                  <p className={`mt-1 text-lg font-semibold ${color}`}>{count}</p>
                </div>
              ))}
            </div>
          </div>

          {groups.map(({ subsystem, findings }) => (
            <div key={subsystem}>
              <h4 className="mb-2 text-xs font-semibold uppercase tracking-wide text-faint">
                {t(SUBSYSTEM_LABEL_KEYS[subsystem] ?? subsystem)}
              </h4>
              <div className="flex flex-col gap-2">
                {findings.map((finding) => (
                  <FindingCard key={finding.id} finding={finding} />
                ))}
              </div>
            </div>
          ))}
        </>
      )}

      {!report && !error && (
        <div className="rounded-lg border border-dashed border-border p-6 text-center">
          <Stethoscope size={24} className="mx-auto text-faint" />
          <p className="mt-2 text-xs font-medium text-foreground">{t("DiagnosticsPanel.emptyTitle")}</p>
          <p className="mt-1 text-xs text-muted">{t("DiagnosticsPanel.emptyBody")}</p>
        </div>
      )}
      {error && (
        <div role="alert" className="flex items-start justify-between gap-3 rounded-md border border-danger/40 bg-danger/10 p-3 text-xs text-danger">
          <span>{error}</span>
          <button type="button" className="shrink-0 font-medium hover:underline focus:outline-none focus:ring-2 focus:ring-danger" onClick={clearError}>
            {t("DiagnosticsPanel.dismiss")}
          </button>
        </div>
      )}
    </section>
  );
}
