import { useMemo, useState } from "react";
import {
  AlertTriangle,
  CheckCircle2,
  CircleDot,
  Loader2,
  RefreshCw,
  ShieldAlert,
  ShieldCheck,
  Wrench,
} from "lucide-react";
import {
  runSecurityAudit,
  type SecurityAuditReport,
  type SecurityFinding,
  type SecurityFindingStatus,
} from "../../lib/securityDoctorClient";
import { Button } from "../ui";
import { errorMessage } from "../../lib/errors";

const STATUS_ORDER: Record<SecurityFindingStatus, number> = {
  critical: 0,
  warning: 1,
  fixed: 2,
  info: 3,
  pass: 4,
};

const STATUS_STYLE: Record<SecurityFindingStatus, string> = {
  critical: "border-danger/40 bg-danger/10 text-danger",
  warning: "border-warning/40 bg-warning/10 text-warning",
  fixed: "border-success/40 bg-success/10 text-success",
  pass: "border-success/30 bg-success/5 text-success",
  info: "border-border bg-surface-2 text-muted",
};

function errorText(error: unknown): string {
  return errorMessage(error);
}

function FindingIcon({ status }: { status: SecurityFindingStatus }) {
  if (status === "critical") return <ShieldAlert size={15} />;
  if (status === "warning") return <AlertTriangle size={15} />;
  if (status === "fixed") return <Wrench size={15} />;
  if (status === "pass") return <CheckCircle2 size={15} />;
  return <CircleDot size={15} />;
}

function FindingCard({ finding }: { finding: SecurityFinding }) {
  return (
    <article className="rounded-lg border border-border bg-surface p-3">
      <div className="flex items-start gap-3">
        <span
          className={`mt-0.5 inline-flex shrink-0 items-center gap-1 rounded-full border px-2 py-1 text-[10px] font-semibold uppercase tracking-wide ${STATUS_STYLE[finding.status]}`}
        >
          <FindingIcon status={finding.status} />
          {finding.status}
        </span>
        <div className="min-w-0 flex-1">
          <div className="flex flex-wrap items-start justify-between gap-2">
            <h5 className="text-xs font-semibold text-foreground">{finding.title}</h5>
            {finding.fixable && finding.status !== "fixed" && (
              <span className="rounded-full border border-border px-2 py-0.5 text-[10px] text-faint">
                safe fix available
              </span>
            )}
          </div>
          <p className="mt-1 text-xs leading-5 text-muted">{finding.detail}</p>
          {finding.path && (
            <p className="mt-2 break-all rounded-md bg-background px-2 py-1 font-mono text-[10px] text-faint">
              {finding.path}
            </p>
          )}
          {finding.remediation && (
            <p className="mt-2 text-[11px] leading-4 text-foreground">
              <span className="font-medium">Next:</span> {finding.remediation}
            </p>
          )}
        </div>
      </div>
    </article>
  );
}

export function SecurityDoctorPanel() {
  const [deep, setDeep] = useState(false);
  const [busy, setBusy] = useState<"audit" | "fix" | null>(null);
  const [report, setReport] = useState<SecurityAuditReport | null>(null);
  const [error, setError] = useState<string | null>(null);

  const groups = useMemo(() => {
    const grouped = new Map<string, SecurityFinding[]>();
    for (const finding of report?.findings ?? []) {
      const current = grouped.get(finding.category) ?? [];
      current.push(finding);
      grouped.set(finding.category, current);
    }
    return [...grouped.entries()]
      .map(([category, findings]) => ({
        category,
        findings: [...findings].sort(
          (left, right) => STATUS_ORDER[left.status] - STATUS_ORDER[right.status] || left.title.localeCompare(right.title),
        ),
      }))
      .sort((left, right) => left.category.localeCompare(right.category));
  }, [report]);

  async function run(fix: boolean) {
    setBusy(fix ? "fix" : "audit");
    setError(null);
    try {
      setReport(await runSecurityAudit({ deep, fix }));
    } catch (cause) {
      setError(errorText(cause));
    } finally {
      setBusy(null);
    }
  }

  function confirmFix() {
    const approved = window.confirm(
      "Apply Security Doctor safe fixes? This can restrict permissions on Little Monkey-owned files and disable clearly unsafe MCP or remote-host listeners. It will not delete data, rotate credentials, or change workspace files.",
    );
    if (approved) void run(true);
  }

  const healthy = report ? report.summary.critical === 0 && report.summary.warnings === 0 : false;

  return (
    <section className="flex flex-col gap-4" aria-labelledby="security-doctor-heading">
      <div className="flex items-start gap-3">
        <span className="rounded-lg border border-accent/30 bg-accent/10 p-2 text-accent">
          <ShieldCheck size={20} />
        </span>
        <div>
          <h3 id="security-doctor-heading" className="text-sm font-semibold text-foreground">
            Security Doctor
          </h3>
          <p className="mt-1 text-xs leading-5 text-muted">
            Audit local file permissions, remote TLS, API and webhook binding, MCP origins, skill integrity,
            and active browser or companion grants. Audits are local and do not contact a model.
          </p>
        </div>
      </div>

      <div className="rounded-lg border border-border bg-surface p-3">
        <label className="flex items-start gap-2 text-xs text-foreground">
          <input
            type="checkbox"
            checked={deep}
            disabled={busy !== null}
            onChange={(event) => setDeep(event.target.checked)}
            className="mt-0.5"
          />
          <span>
            <span className="font-medium">Deep audit</span>
            <span className="mt-0.5 block leading-4 text-muted">
              Recursively checks protected app-data trees and verifies the configured remote TLS certificate pin.
            </span>
          </span>
        </label>
        <div className="mt-3 flex flex-wrap gap-2">
          <Button variant="primary" disabled={busy !== null} onClick={() => void run(false)}>
            {busy === "audit" ? <Loader2 size={14} className="animate-spin" /> : <RefreshCw size={14} />}
            Run {deep ? "deep " : ""}audit
          </Button>
          <Button variant="secondary" disabled={busy !== null} onClick={confirmFix}>
            {busy === "fix" ? <Loader2 size={14} className="animate-spin" /> : <Wrench size={14} />}
            Apply safe fixes
          </Button>
        </div>
        <p className="mt-2 text-[11px] leading-4 text-faint">
          Safe fixes are narrow and reversible: owner-only modes, plus disabling an unsafe app-owned listener while
          preserving its configuration. Workspace files, skills, chats, keys, and plugins are never deleted.
        </p>
      </div>

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
                  ? "No outstanding security warnings"
                  : `${report.summary.critical} critical and ${report.summary.warnings} warning finding(s)`}
              </p>
              <span className="text-[10px] text-faint">
                {report.deep ? "Deep" : "Standard"} · {new Date(report.generatedAtMs).toLocaleString()}
              </span>
            </div>
            <div className="mt-3 grid grid-cols-2 gap-2 sm:grid-cols-5">
              {[
                ["Critical", report.summary.critical, "text-danger"],
                ["Warnings", report.summary.warnings, "text-warning"],
                ["Fixed", report.summary.fixed, "text-success"],
                ["Passed", report.summary.passed, "text-success"],
                ["Info", report.summary.informational, "text-muted"],
              ].map(([label, count, color]) => (
                <div key={String(label)} className="rounded-md border border-border/70 bg-background/60 p-2">
                  <p className="text-[10px] uppercase tracking-wide text-faint">{label}</p>
                  <p className={`mt-1 text-lg font-semibold ${color}`}>{count}</p>
                </div>
              ))}
            </div>
          </div>

          {groups.map(({ category, findings }) => (
            <div key={category}>
              <h4 className="mb-2 text-xs font-semibold uppercase tracking-wide text-faint">{category}</h4>
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
          <ShieldCheck size={24} className="mx-auto text-faint" />
          <p className="mt-2 text-xs text-muted">Run an audit to capture the current local posture.</p>
        </div>
      )}
      {error && (
        <p role="alert" className="rounded-md border border-danger/40 bg-danger/10 p-3 text-xs text-danger">
          {error}
        </p>
      )}
    </section>
  );
}
