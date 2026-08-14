import type { ProcessLimitReport, ProcessResourceReport } from "../../lib/processTable";
import {
  breachHeldAtTheCap,
  enforcementDetail,
  enforcementOf,
  formatLimitValue,
  isBounded,
  type EnforcementLevel,
} from "../../lib/processResources";
import { StatusPill } from "../ui";
import { useT } from "../../lib/i18n";

/**
 * What is bounding one process, and who is holding it.
 *
 * The questions this exists to answer, in order, because they are the ones a user
 * could not answer before at all:
 *
 * - what limit was requested, and what is actually installed;
 * - who supplied the winning number;
 * - which mechanism is enforcing it, and whether that mechanism is the kernel, a
 *   supervisor in this app, or the workload's own owner;
 * - what it cost;
 * - and if a limit fired, what was configured against what was observed.
 *
 * # Nothing here is a hard-coded capability string
 *
 * Every mechanism name, level and detail arrives typed from
 * `process_resource_report`, which builds them from the same
 * `ResourceController` the enforcement runs through and the same
 * `ProcessKind::limit_support` matrix `monkey processes limits` prints. A second
 * table maintained in this file is how the panel and the CLI would come to
 * disagree about the same process.
 *
 * # An unsupported resource is not zero
 *
 * A limit nothing holds renders as its reason, never as "0". The two are
 * indistinguishable to a reader, and one of them is a claim that a budget of
 * nothing is in force.
 */

type Translate = ReturnType<typeof useT>["t"];

function enforcementLabel(t: Translate, level: EnforcementLevel): string {
  switch (level) {
    case "kernel":
      return t("ProcessesPanel.enforcementKernel");
    case "supervised":
      return t("ProcessesPanel.enforcementSupervised");
    case "owner-sourced":
      return t("ProcessesPanel.enforcementOwnerSourced");
    case "not-applicable":
      return t("ProcessesPanel.enforcementNotApplicable");
    default:
      return t("ProcessesPanel.enforcementUnavailable");
  }
}

/** Kernel bounds survive this app dying; nothing else here does. */
function enforcementTone(level: EnforcementLevel): "success" | "neutral" | "warning" {
  switch (level) {
    case "kernel":
      return "success";
    case "supervised":
    case "owner-sourced":
      return "neutral";
    default:
      return "warning";
  }
}

function limitLabel(t: Translate, limit: string): string {
  switch (limit) {
    case "max_wall_ms":
      return t("ProcessesPanel.limitWall");
    case "max_memory_bytes":
      return t("ProcessesPanel.limitMemory");
    case "max_output_bytes":
      return t("ProcessesPanel.limitOutput");
    case "max_child_processes":
      return t("ProcessesPanel.limitChildProcesses");
    case "max_context_tokens":
      return t("ProcessesPanel.limitContextTokens");
    default:
      return limit;
  }
}

/** Who supplied the number, in the row's own terms. */
function originLabel(t: Translate, report: ProcessLimitReport): string | null {
  switch (report.origin) {
    case "class_default":
      return t("ProcessesPanel.originClassDefault");
    case "caller_override":
      return t("ProcessesPanel.originCallerOverride", {
        classDefault: formatLimitValue(report.limit, report.classDefault ?? 0),
      });
    case "caller_supplied":
      return t("ProcessesPanel.originCallerSupplied");
    case "unrecorded":
      return t("ProcessesPanel.originUnrecorded");
    default:
      return null;
  }
}

function LimitRow({ report }: { report: ProcessLimitReport }) {
  const { t } = useT();
  const level = enforcementOf(report);
  const bounded = isBounded(report);
  const origin = originLabel(t, report);

  return (
    <div className="border-t border-border py-1.5 first:border-t-0">
      <div className="flex flex-wrap items-baseline gap-x-2 gap-y-0.5">
        <span className="text-xs text-foreground">{limitLabel(t, report.limit)}</span>
        <span className="font-mono text-xs text-muted">
          {bounded
            ? formatLimitValue(report.limit, report.effective as number)
            : t("ProcessesPanel.limitUnbounded")}
        </span>
        <StatusPill tone={enforcementTone(level)}>{enforcementLabel(t, level)}</StatusPill>
      </div>
      {origin && <p className="mt-0.5 text-[11px] text-faint">{origin}</p>}
      <p className="mt-0.5 text-[11px] text-faint">{enforcementDetail(report)}</p>
      <p className="mt-0.5 text-[11px] text-faint">
        {report.observed === undefined
          ? t("ProcessesPanel.usageUnavailable", {
              reason: report.observedUnavailable ?? "",
            })
          : t("ProcessesPanel.usageObserved", {
              observed: formatLimitValue(report.limit, report.observed),
            })}
      </p>
    </div>
  );
}

/**
 * The breach panel: both numbers, the mechanism, and — where a kernel refused
 * rather than allowing an overshoot — the counter that proved it.
 *
 * Equal numbers are rendered with that explanation rather than hidden, because
 * equal is the *normal* shape of a kernel breach: the bound exists so the
 * workload never passes it. Without the note, the limit that worked best would
 * read as the one that did not fire.
 */
function BreachDetail({ report }: { report: ProcessResourceReport }) {
  const { t } = useT();
  const breach = report.breach;
  if (!breach) return null;
  return (
    <div className="mt-2 rounded-md border border-danger bg-danger-soft p-2">
      <p className="text-xs font-semibold text-danger">
        {t("ProcessesPanel.breachTitle", { limit: limitLabel(t, breach.limit) })}
      </p>
      <p className="mt-0.5 font-mono text-[11px] text-danger">
        {t("ProcessesPanel.breachConfigured", {
          configured: formatLimitValue(breach.limit, breach.configured),
        })}
      </p>
      <p className="font-mono text-[11px] text-danger">
        {t("ProcessesPanel.breachObserved", {
          observed: formatLimitValue(breach.limit, breach.observed),
        })}
      </p>
      <p className="mt-0.5 text-[11px] text-danger">
        {t("ProcessesPanel.breachBackend", {
          backend: breach.backend,
          level: breach.level,
        })}
      </p>
      {breach.evidence && (
        <p className="mt-0.5 text-[11px] text-danger">
          {t("ProcessesPanel.breachEvidence", { evidence: breach.evidence })}
        </p>
      )}
      {breachHeldAtTheCap(breach) && (
        <p className="mt-0.5 text-[11px] text-danger">{t("ProcessesPanel.breachHeldAtCap")}</p>
      )}
    </div>
  );
}

export function ProcessResources({ report }: { report: ProcessResourceReport }) {
  const { t } = useT();
  return (
    <section
      className="mt-2 rounded-lg border border-border bg-surface p-2"
      aria-label={t("ProcessesPanel.resourcesAriaLabel")}
    >
      {report.backend && (
        <p className="mb-1 text-[11px] text-faint">
          {t("ProcessesPanel.resourceBackend", { backend: report.backend })}
        </p>
      )}
      {report.treePrimitive && (
        <p className="mb-1 text-[11px] text-faint">
          {t("ProcessesPanel.resourceTree", { primitive: report.treePrimitive })}
        </p>
      )}
      {report.limits.map((limit) => (
        <LimitRow key={limit.limit} report={limit} />
      ))}
      <BreachDetail report={report} />
    </section>
  );
}
