import { useEffect, useMemo, useState } from "react";
import { useShallow } from "zustand/react/shallow";
import { ArrowRight, RefreshCw } from "lucide-react";

import { Button, StatusPill, Tabs } from "../ui";
import { useT } from "../../lib/i18n";
import { formatTimestamp } from "../../lib/format";
import type { SchedulerDecision } from "../../lib/daemonClient";
import {
  contextHitRate,
  contextReuseFor,
  destinationsFor,
  measurementOf,
  renderMeasurement,
  renderTotal,
  totalIsPartial,
  USAGE_FIELDS,
  type ContextReuse,
  type ProcessEgressDestinations,
  type ProcessUsageAggregate,
  type ProcessUsageRow,
  type UsageFieldSpec,
} from "../../lib/processUsage";
import { useResourceLedgerStore } from "../../store/resourceLedgerStore";

/**
 * The two inspection surfaces the K6/K8 backends landed without: what each
 * process actually consumed, and why the scheduler chose what it chose.
 *
 * ## Unavailable is not zero
 *
 * Every number on this panel goes through `renderMeasurement`/`renderTotal`,
 * which return a tagged union rather than a string. An unmeasured field has no
 * `.text` to read, so the only way to render it is the unavailable branch — and
 * that branch always carries the backend's own reason. There is deliberately no
 * chart here: a bar of height zero for a field nobody measured is the exact lie
 * this panel exists to avoid, and no bar chart can express "unknown" as
 * distinct from "idle".
 *
 * A total is shown with how many rows could not contribute, because a sum over
 * 3 of 10 rows is answering a narrower question than it looks like.
 *
 * ## The cited measurement's own timestamp
 *
 * A decision row shows `measuredAtMs` labelled as the *reading's* observation
 * time, next to and distinct from `decidedAtMs`. That separation is the item's
 * substance: a decision has to cite a real observation, and a re-derived guess
 * wearing a fresh timestamp is what the distinction rules out. Collapsing the
 * two into one "time" column would erase it.
 */

type Translate = ReturnType<typeof useT>["t"];

type LedgerTab = "usage" | "decisions";

function fieldLabel(t: Translate, field: string): string {
  return t(`ResourceLedgerPanel.field_${field}`);
}

/** One measurement cell: a number, or the reason there isn't one. */
function Measurement({ row, spec }: { row: ProcessUsageRow; spec: UsageFieldSpec }) {
  const { t } = useT();
  const rendered = renderMeasurement(
    measurementOf(row, spec.field),
    spec.unit,
    t("ResourceLedgerPanel.unavailableWithoutReason"),
  );
  return (
    <div className="min-w-0">
      <p className="text-[11px] text-faint">{fieldLabel(t, spec.field)}</p>
      {rendered.available ? (
        <p className="mt-0.5 font-mono text-xs text-foreground">{rendered.text}</p>
      ) : (
        // The honest branch. The label says "unavailable" and the reason says
        // why; neither is ever replaced by a 0, an em dash on its own, or a
        // blank cell that a reader would fill in as zero themselves.
        <p className="mt-0.5 text-[11px] leading-snug text-warning">
          {t("ResourceLedgerPanel.unavailableWithReason", { reason: rendered.reason })}
        </p>
      )}
    </div>
  );
}

/**
 * Where this process's allowed egress went.
 *
 * Renders nothing when nothing was recorded, rather than an empty list: the
 * ledger cannot tell "reached nowhere" from "this build recorded nothing", and
 * an empty list on screen would claim the first.
 */
export function DestinationList({ recorded }: { recorded: ProcessEgressDestinations | null }) {
  const { t } = useT();
  if (!recorded) return null;
  return (
    <div className="mt-2 border-t border-border pt-2">
      <p className="text-[11px] text-faint">{t("ResourceLedgerPanel.destinationsLabel")}</p>
      <ul className="mt-1 flex flex-wrap gap-x-3 gap-y-1">
        {recorded.destinations.map((destination) => (
          <li
            key={`${destination.scheme}://${destination.host}:${destination.port}`}
            className="font-mono text-[11px] text-foreground"
          >
            {destination.host}
            <span className="text-faint">
              {`:${destination.port} · `}
              {t("ResourceLedgerPanel.destinationRequests", { count: destination.requests })}
            </span>
          </li>
        ))}
      </ul>
      {/* Same rule as a partial total: the list is still real, but it is never
          shown without the count it is missing. */}
      {recorded.dropped > 0 && (
        <p className="mt-1 text-[11px] leading-snug text-warning">
          {t("ResourceLedgerPanel.destinationsDropped", { count: recorded.dropped })}
        </p>
      )}
    </div>
  );
}

/**
 * What the runtime's prompt cache actually saved this process (roadmap K11).
 *
 * Renders nothing when the runtime reported no figure, for `DestinationList`'s
 * reason and one sharper: Ollama and MLX report nothing at all, so a "0% hit
 * rate" here would be a measurement this app invented for two of its three
 * runtimes. The percentage is shown beside the token count it came from, because
 * a rate without its denominator cannot be checked.
 */
export function ContextReuseSummary({ reuse }: { reuse: ContextReuse | null }) {
  const { t } = useT();
  const rate = contextHitRate(reuse);
  if (!reuse || rate === null) return null;
  return (
    <div className="mt-2 border-t border-border pt-2">
      <p className="text-[11px] text-faint">{t("ResourceLedgerPanel.contextReuseLabel")}</p>
      <p className="mt-0.5 font-mono text-xs text-foreground">
        {t("ResourceLedgerPanel.contextReuseHitRate", { percent: (rate * 100).toFixed(1) })}
        <span className="text-faint">
          {" · "}
          {t("ResourceLedgerPanel.contextReuseTokens", {
            reused: reuse.reusedTokens,
            evaluated: reuse.evaluatedTokens,
          })}
        </span>
      </p>
    </div>
  );
}

export function UsageRow({
  row,
  destinations = null,
  contextReuse = null,
}: {
  row: ProcessUsageRow;
  destinations?: ProcessEgressDestinations | null;
  contextReuse?: ContextReuse | null;
}) {
  const { t } = useT();
  const unavailableCount = row.usage.unavailable.length;
  return (
    <div className="rounded-lg border border-border bg-surface p-3">
      <div className="flex items-start gap-2">
        <span className="min-w-0 flex-1 break-all font-mono text-xs text-foreground">{row.externalId}</span>
        <StatusPill tone={row.state === "exited" ? "neutral" : "warning"}>
          {t(`ResourceLedgerPanel.kind_${row.kind}`)}
        </StatusPill>
      </div>
      <div className="mt-1 flex flex-wrap items-center gap-x-3 text-[11px] text-faint">
        {row.workspace && <span className="truncate">{row.workspace}</span>}
        {row.exitStatus && <span>{t(`ResourceLedgerPanel.exit_${row.exitStatus}`)}</span>}
        {unavailableCount > 0 && (
          <span className="text-warning">
            {t("ResourceLedgerPanel.rowUnavailableCount", { count: unavailableCount, total: USAGE_FIELDS.length })}
          </span>
        )}
      </div>
      <div className="mt-2 grid grid-cols-2 gap-2 sm:grid-cols-3 lg:grid-cols-5">
        {USAGE_FIELDS.map((spec) => (
          <Measurement key={spec.field} row={row} spec={spec} />
        ))}
      </div>
      <DestinationList recorded={destinations} />
      <ContextReuseSummary reuse={contextReuse} />
    </div>
  );
}

/** One aggregate field, always beside the count of rows it could not read. */
export function UsageTotalCard({ totals, spec }: { totals: ProcessUsageAggregate; spec: UsageFieldSpec }) {
  const { t } = useT();
  const total = totals[spec.field];
  const rendered = renderTotal(total, spec.unit, t("ResourceLedgerPanel.totalUnmeasured"));
  return (
    <div className="rounded-lg border border-border bg-surface p-3">
      <p className="text-[11px] text-faint">
        {fieldLabel(t, spec.field)}
        {" · "}
        {spec.fold === "max" ? t("ResourceLedgerPanel.foldMax") : t("ResourceLedgerPanel.foldSum")}
      </p>
      {rendered.available ? (
        <p className="mt-1 font-mono text-sm text-foreground">{rendered.text}</p>
      ) : (
        <p className="mt-1 text-[11px] leading-snug text-warning">{rendered.reason}</p>
      )}
      {/* A partial total is still a real number, so it is shown — but never
          alone. "9.2 GB over 3 of 10 rows" is a claim a reader can check; the
          same number with the other seven rows silently dropped is not. */}
      <p className={`mt-1 text-[11px] ${totalIsPartial(total) ? "text-warning" : "text-faint"}`}>
        {t("ResourceLedgerPanel.totalCoverage", {
          measured: total.measuredRows,
          rows: total.measuredRows + total.unavailableRows,
        })}
      </p>
    </div>
  );
}

export function DecisionRow({ decision }: { decision: SchedulerDecision }) {
  const { t } = useT();
  const promoted = decision.effectiveClass !== decision.processClass;
  return (
    <div className="rounded-lg border border-border bg-surface p-3">
      <div className="flex flex-wrap items-center gap-2">
        <StatusPill tone={decisionTone(decision.outcome)}>{outcomeLabel(t, decision.outcome)}</StatusPill>
        <span className="min-w-0 flex-1 break-all font-mono text-xs text-foreground">{decision.jobId}</span>
        <span className="text-[11px] text-faint">{formatTimestamp(decision.decidedAtMs, { timeStyle: "medium" })}</span>
      </div>

      {/* The causal chain, in the order it reads: this job, over these, because
          of this reading. Aging promotion is called out because the effective
          class — not the declared one — is what the ranking actually used. */}
      <p className="mt-2 flex flex-wrap items-center gap-1.5 text-[11px] text-muted">
        <span className="font-medium text-foreground">
          {promoted
            ? t("ResourceLedgerPanel.classPromoted", { declared: decision.processClass, effective: decision.effectiveClass })
            : t("ResourceLedgerPanel.classPlain", { effective: decision.effectiveClass })}
        </span>
        {decision.passedOver.length > 0 && (
          <>
            <ArrowRight size={11} className="shrink-0 text-faint" />
            <span>{t("ResourceLedgerPanel.passedOver", { jobs: decision.passedOver.join(", ") })}</span>
          </>
        )}
      </p>

      <p className="mt-1 text-[11px] leading-snug text-muted">{decision.detail}</p>

      <div className="mt-2 rounded-md bg-surface-2 p-2">
        <p className="text-[11px] text-faint">{t("ResourceLedgerPanel.decidingMeasurement")}</p>
        <p className="mt-0.5 font-mono text-xs text-foreground">
          {decision.measurement}
          {decision.measuredValue === null
            ? ` = ${t("ResourceLedgerPanel.measuredValueMissing")}`
            : ` = ${decision.measuredValue.toLocaleString()}`}
        </p>
        {/* Labelled as the READING's own observation time. Never relabelled as
            the decision time — that is the timestamp in the header above, and
            the gap between the two is the point of the column. */}
        <p className="mt-0.5 text-[11px] text-faint">
          {decision.measuredAtMs === null
            ? t("ResourceLedgerPanel.observedAtMissing")
            : t("ResourceLedgerPanel.observedAt", {
                at: formatTimestamp(decision.measuredAtMs, { timeStyle: "medium" }),
              })}
        </p>
      </div>

      {decision.workspace && <p className="mt-1 truncate text-[11px] text-faint">{decision.workspace}</p>}
    </div>
  );
}

function decisionTone(outcome: string): "neutral" | "success" | "warning" | "danger" {
  if (outcome === "admitted" || outcome === "resumed") return "success";
  if (outcome === "rejected") return "danger";
  if (outcome === "preempted") return "warning";
  return "neutral";
}

function outcomeLabel(t: Translate, outcome: string): string {
  switch (outcome) {
    case "admitted":
      return t("ResourceLedgerPanel.outcomeAdmitted");
    case "held":
      return t("ResourceLedgerPanel.outcomeHeld");
    case "preempted":
      return t("ResourceLedgerPanel.outcomePreempted");
    case "resumed":
      return t("ResourceLedgerPanel.outcomeResumed");
    case "rejected":
      return t("ResourceLedgerPanel.outcomeRejected");
    default:
      return outcome;
  }
}

export function ResourceLedgerPanel() {
  const { t } = useT();
  const [tab, setTab] = useState<LedgerTab>("usage");

  const rows = useResourceLedgerStore(useShallow((state) => state.rows));
  const totals = useResourceLedgerStore((state) => state.totals);
  const destinations = useResourceLedgerStore(useShallow((state) => state.destinations));
  const contextReuse = useResourceLedgerStore(useShallow((state) => state.contextReuse));
  const closedOnly = useResourceLedgerStore((state) => state.closedOnly);
  const loadingLedger = useResourceLedgerStore((state) => state.loadingLedger);
  const ledgerError = useResourceLedgerStore((state) => state.ledgerError);
  const decisions = useResourceLedgerStore(useShallow((state) => state.decisions));
  const loadingDecisions = useResourceLedgerStore((state) => state.loadingDecisions);
  const decisionsError = useResourceLedgerStore((state) => state.decisionsError);

  // Read on demand, per tab: neither surface is a live dashboard, and the
  // decision log in particular is read when somebody is asking why.
  useEffect(() => {
    if (tab === "usage") void useResourceLedgerStore.getState().refreshLedger();
    else void useResourceLedgerStore.getState().refreshDecisions();
  }, [tab]);

  const partialTotals = useMemo(
    () => (totals ? USAGE_FIELDS.filter((spec) => totalIsPartial(totals[spec.field])).length : 0),
    [totals],
  );

  return (
    <section className="flex flex-col gap-4">
      <div>
        <h3 className="text-sm font-semibold text-foreground">{t("ResourceLedgerPanel.title")}</h3>
        <p className="mt-1 text-xs leading-5 text-muted">{t("ResourceLedgerPanel.description")}</p>
      </div>
      <Tabs
        tabs={[
          { id: "usage", label: t("ResourceLedgerPanel.tabUsage") },
          { id: "decisions", label: t("ResourceLedgerPanel.tabDecisions") },
        ]}
        active={tab}
        onChange={(id) => setTab(id as LedgerTab)}
      />

      {tab === "usage" && (
        <>
          <div className="flex flex-wrap items-center gap-2">
            <Button
              size="sm"
              disabled={loadingLedger}
              onClick={() => void useResourceLedgerStore.getState().refreshLedger()}
            >
              <RefreshCw size={14} className={loadingLedger ? "animate-spin" : undefined} />
              {t("ResourceLedgerPanel.refresh")}
            </Button>
            <label className="flex items-center gap-2 text-xs text-muted">
              <input
                type="checkbox"
                checked={closedOnly}
                onChange={(event) => void useResourceLedgerStore.getState().setClosedOnly(event.target.checked)}
              />
              {t("ResourceLedgerPanel.closedOnly")}
            </label>
          </div>
          <p className="text-[11px] leading-5 text-faint">{t("ResourceLedgerPanel.unavailableExplainer")}</p>

          {ledgerError && (
            <p role="alert" className="rounded-md border border-danger bg-danger-soft p-2 text-xs text-danger">
              {ledgerError}
            </p>
          )}

          {totals && (
            <div>
              <h4 className="text-xs font-semibold text-foreground">
                {t("ResourceLedgerPanel.totalsHeading", { rows: totals.rows })}
              </h4>
              {partialTotals > 0 && (
                <p className="mt-1 text-[11px] leading-5 text-warning">
                  {t("ResourceLedgerPanel.totalsPartialWarning", { fields: partialTotals })}
                </p>
              )}
              <div className="mt-2 grid grid-cols-2 gap-2 md:grid-cols-3 xl:grid-cols-5">
                {USAGE_FIELDS.map((spec) => (
                  <UsageTotalCard key={spec.field} totals={totals} spec={spec} />
                ))}
              </div>
            </div>
          )}

          {rows.length === 0 && !loadingLedger && !ledgerError && (
            <p className="text-xs text-faint">{t("ResourceLedgerPanel.emptyUsage")}</p>
          )}
          <div className="flex flex-col gap-2">
            {rows.map((row) => (
              <UsageRow
                key={row.processId}
                row={row}
                destinations={destinationsFor({ destinations }, row.processId)}
                contextReuse={contextReuseFor({ contextReuse }, row.processId)}
              />
            ))}
          </div>
        </>
      )}

      {tab === "decisions" && (
        <>
          <div className="flex flex-wrap items-center gap-2">
            <Button
              size="sm"
              disabled={loadingDecisions}
              onClick={() => void useResourceLedgerStore.getState().refreshDecisions()}
            >
              <RefreshCw size={14} className={loadingDecisions ? "animate-spin" : undefined} />
              {t("ResourceLedgerPanel.refresh")}
            </Button>
          </div>
          <p className="text-[11px] leading-5 text-faint">{t("ResourceLedgerPanel.decisionsExplainer")}</p>

          {decisionsError && (
            <div role="alert" className="rounded-md border border-danger bg-danger-soft p-2 text-xs text-danger">
              <p>{decisionsError}</p>
              <p className="mt-1 text-[11px] text-muted">{t("ResourceLedgerPanel.decisionsUnavailableHint")}</p>
            </div>
          )}

          {decisions.length === 0 && !loadingDecisions && !decisionsError && (
            <p className="text-xs text-faint">{t("ResourceLedgerPanel.emptyDecisions")}</p>
          )}
          <div className="flex flex-col gap-2">
            {decisions.map((decision) => (
              <DecisionRow key={`${decision.decidedAtMs}-${decision.jobId}`} decision={decision} />
            ))}
          </div>
        </>
      )}
    </section>
  );
}
