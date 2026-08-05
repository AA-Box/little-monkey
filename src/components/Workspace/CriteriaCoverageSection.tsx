import { useCallback, useMemo, useRef, useState } from "react";
import { AlertTriangle, ChevronDown, ChevronRight, ListChecks, ShieldAlert } from "lucide-react";

import { Button } from "../ui";
import {
  computeReviewFacts,
  isReportStale,
  mapReviewCoverage,
  parseCriteriaInput,
  type CheckedCriterionClaim,
  type ReviewBaseMode,
  type ReviewCoverageInput,
  type ReviewCoverageReport,
  type ReviewFacts,
} from "../../lib/reviewCoverage";

/**
 * The Review panel's criteria-coverage band: paste the acceptance criteria for
 * this change, and see which of them the diff actually satisfies — and, the
 * point of the thing, which of them nothing in the diff touches.
 *
 * The mapping is model output, so the two halves are rendered as two visually
 * separate blocks and never interleaved: what `reviewCoverage.ts` computed from
 * git, then what a model claimed *about* those facts, each claim carrying its
 * own check result. A claim citing a hunk this diff does not contain is shown
 * as discarded, with the invented id visible — quieter handling would leave a
 * reader thinking a model read a diff it did not.
 *
 * State is in-memory: the right sidebar keeps inactive tabs mounted (App.tsx),
 * so criteria and a report survive tab switching, and only an app restart
 * clears them. Persisting them is one localStorage call away if that turns out
 * to matter.
 */
export interface CriteriaCoverageSectionProps {
  /** The panel's current `git_review` payload, or null before the first load. */
  review: ReviewCoverageInput | null;
  mode: ReviewBaseMode;
  /** Threaded from `ReviewPanel`'s single `useT()` call, matching how
   * `FileDiff` receives it — children in this panel never call `useT`. */
  t: (key: string, vars?: Record<string, string | number>) => string;
  /** Scrolls the diff column to a file, so a citation is clickable. */
  onRevealPath?: (path: string) => void;
}

const STATUS_TONE: Record<CheckedCriterionClaim["status"], string> = {
  accepted: "text-success",
  unsupported: "text-warning",
  rejected: "text-danger",
};

/** `H3 · src/limit.ts:12` — a citation the reader can check for themselves. */
function citationLabel(facts: ReviewFacts, hunkId: string): string {
  const hunk = facts.hunks.find((candidate) => candidate.hunkId === hunkId);
  if (!hunk) return hunkId;
  const line = hunk.newStart ?? hunk.oldStart;
  return `${hunkId} · ${hunk.path}${line === null ? "" : `:${line}`}`;
}

/**
 * One criterion's row. Exported so `CriteriaCoverageSection.test.tsx` can
 * assert what a rejected or unsupported claim actually renders — this repo has
 * no DOM test environment, so a row is checked by rendering it to a static
 * string, the same way `PermissionModal` exports its own decision function to
 * be tested without a harness.
 */
export function ClaimRow({ claim, report, t, onRevealPath }: {
  claim: CheckedCriterionClaim;
  report: ReviewCoverageReport;
  t: CriteriaCoverageSectionProps["t"];
  onRevealPath?: (path: string) => void;
}) {
  const facts = report.computed;
  const criterion = facts.criteria.find((entry) => entry.criterionId === claim.criterionId);

  return (
    <li className="border-b border-border px-3 py-2 last:border-b-0">
      <div className="flex items-start gap-2">
        <span className={`shrink-0 font-mono text-[11px] ${STATUS_TONE[claim.status]}`}>
          {claim.criterionId}
        </span>
        <span className="min-w-0 flex-1 text-xs text-foreground">{criterion?.text ?? claim.criterionId}</span>
        <span className={`shrink-0 text-[11px] ${STATUS_TONE[claim.status]}`}>
          {t(`ReviewPanel.coverageVerdict_${claim.claimed}`)}
        </span>
      </div>

      {claim.status === "rejected" && (
        <p className="mt-1 flex items-start gap-1.5 text-[11px] text-danger">
          <ShieldAlert size={12} className="mt-px shrink-0" />
          {t("ReviewPanel.coverageRejectedNote", { ids: claim.invalidCitations.join(", ") })}
        </p>
      )}
      {claim.status === "unsupported" && (
        <p className="mt-1 flex items-start gap-1.5 text-[11px] text-warning">
          <AlertTriangle size={12} className="mt-px shrink-0" />
          {t(claim.rationale ? "ReviewPanel.coverageUnsupportedNote" : "ReviewPanel.coverageNoClaimNote")}
        </p>
      )}

      {claim.validCitations.length > 0 && (
        <div className="mt-1 flex flex-wrap gap-1">
          {claim.validCitations.map((hunkId) => {
            const hunk = facts.hunks.find((candidate) => candidate.hunkId === hunkId);
            return (
              <button
                key={hunkId}
                type="button"
                onClick={() => hunk && onRevealPath?.(hunk.path)}
                title={hunk?.excerpt}
                className="rounded-md bg-surface-2 px-1.5 py-0.5 font-mono text-[10px] text-muted hover:bg-surface hover:text-foreground"
              >
                {citationLabel(facts, hunkId)}
              </button>
            );
          })}
        </div>
      )}

      {claim.rationale && claim.status !== "rejected" && (
        <p className="mt-1 text-[11px] text-muted">{claim.rationale}</p>
      )}
    </li>
  );
}

export function CriteriaCoverageSection({ review, mode, t, onRevealPath }: CriteriaCoverageSectionProps) {
  const [expanded, setExpanded] = useState(false);
  const [criteriaText, setCriteriaText] = useState("");
  const [report, setReport] = useState<ReviewCoverageReport | null>(null);
  const [running, setRunning] = useState(false);
  /** Either an i18n key or a raw message, told apart the same way the parent
   * panel's own error state is (`startsWith("ReviewPanel.")`). */
  const [error, setError] = useState<string | null>(null);
  const abortRef = useRef<AbortController | null>(null);

  /**
   * Whether a held report still describes the diff on screen. Re-derives the
   * facts, which re-diffs every file — so it only runs while the section is
   * open AND a report is being shown, never during ordinary panel rendering.
   */
  const stale = useMemo(() => {
    if (!expanded || !report || !review) return false;
    return isReportStale(report, computeReviewFacts(review, report.computed.criteria, mode));
  }, [expanded, report, review, mode]);

  const run = useCallback(async () => {
    if (!review) return;
    let criteria;
    try {
      criteria = parseCriteriaInput(criteriaText);
    } catch (parseError) {
      setError(String(parseError instanceof Error ? parseError.message : parseError));
      return;
    }
    if (criteria.length === 0) {
      setError("ReviewPanel.coverageNeedsCriteria");
      return;
    }

    abortRef.current?.abort();
    const controller = new AbortController();
    abortRef.current = controller;
    setRunning(true);
    setError(null);
    try {
      setReport(await mapReviewCoverage(review, criteria, mode, controller.signal));
    } catch (runError) {
      setError(String(runError instanceof Error ? runError.message : runError));
    } finally {
      setRunning(false);
    }
    // `t` is deliberately absent: useT() returns a fresh function each render
    // and the parent panel documents (ReviewPanel.tsx) what depending on it
    // costs. Errors are stored, then translated at render time.
  }, [criteriaText, mode, review]);

  const uncovered = report?.uncoveredCriterionIds.length ?? 0;
  const total = report?.computed.criteria.length ?? 0;

  return (
    <section className="shrink-0 border-b border-border">
      <button
        type="button"
        onClick={() => setExpanded((open) => !open)}
        className="flex w-full items-center gap-1.5 px-3 py-2 text-left hover:bg-surface-2"
      >
        {expanded ? <ChevronDown size={13} className="shrink-0 text-faint" /> : <ChevronRight size={13} className="shrink-0 text-faint" />}
        <ListChecks size={13} className="shrink-0 text-faint" />
        <span className="min-w-0 flex-1 truncate text-xs text-foreground">{t("ReviewPanel.coverageTitle")}</span>
        {report && (
          <span className={`shrink-0 text-[11px] ${uncovered > 0 ? "text-warning" : "text-success"}`}>
            {uncovered > 0
              ? t("ReviewPanel.coverageUncovered", { count: uncovered, total })
              : t("ReviewPanel.coverageAllCovered", { total })}
          </span>
        )}
      </button>

      {expanded && (
        <div className="border-t border-border px-3 py-2">
          <label className="block text-[11px] text-muted" htmlFor="review-coverage-criteria">
            {t("ReviewPanel.coverageHint")}
          </label>
          <textarea
            id="review-coverage-criteria"
            value={criteriaText}
            onChange={(event) => setCriteriaText(event.target.value)}
            placeholder={t("ReviewPanel.coveragePlaceholder")}
            rows={4}
            className="mt-1 w-full resize-y rounded-md border border-border bg-background px-2 py-1.5 font-mono text-xs text-foreground outline-none focus:border-accent"
          />
          <div className="mt-1.5 flex items-center gap-2">
            <Button size="sm" variant="secondary" onClick={() => void run()} disabled={running || !review}>
              <ListChecks size={13} /> {t(running ? "ReviewPanel.coverageRunning" : "ReviewPanel.coverageRun")}
            </Button>
            {report && (
              <span className="truncate text-[11px] text-faint">
                {t("ReviewPanel.coverageModel", { model: report.modelLabel })}
              </span>
            )}
          </div>

          {error && (
            <p className="mt-1.5 text-[11px] text-danger">
              {error.startsWith("ReviewPanel.") ? t(error) : error}
            </p>
          )}

          {stale && (
            <p className="mt-1.5 flex items-start gap-1.5 text-[11px] text-warning">
              <AlertTriangle size={12} className="mt-px shrink-0" />
              {t("ReviewPanel.coverageStale")}
            </p>
          )}

          {report && (
            <div className="mt-2 space-y-2">
              {/* Half one: facts this app computed. No model touched these. */}
              <div className="rounded-md border border-border">
                <p className="border-b border-border px-3 py-1.5 text-[11px] font-medium text-foreground">
                  {t("ReviewPanel.coverageComputedHeading")}
                </p>
                <p className="px-3 py-1.5 font-mono text-[11px] text-muted">
                  {t("ReviewPanel.coverageComputedSummary", {
                    files: report.computed.files.length,
                    hunks: report.computed.hunks.length,
                    added: report.computed.totalAdded,
                    removed: report.computed.totalRemoved,
                    digest: report.computed.digest,
                  })}
                </p>
                {(report.computed.filesPossiblyTruncated || report.computed.hunksPossiblyTruncated) && (
                  <p className="flex items-start gap-1.5 px-3 pb-1.5 text-[11px] text-warning">
                    <AlertTriangle size={12} className="mt-px shrink-0" />
                    {t("ReviewPanel.coverageTruncated")}
                  </p>
                )}
                {report.computed.uncitableFilePaths.length > 0 && (
                  <p className="px-3 pb-1.5 text-[11px] text-faint">
                    {t("ReviewPanel.coverageUncitable", {
                      paths: report.computed.uncitableFilePaths.join(", "),
                    })}
                  </p>
                )}
                {report.uncitedHunkIds.length > 0 && (
                  <p className="px-3 pb-1.5 text-[11px] text-muted">
                    {t("ReviewPanel.coverageUncitedHunks", { count: report.uncitedHunkIds.length })}
                  </p>
                )}
              </div>

              {/* Half two: model claims, each already checked against half one. */}
              <div className="rounded-md border border-border">
                <p className="border-b border-border px-3 py-1.5 text-[11px] font-medium text-foreground">
                  {t("ReviewPanel.coverageClaimsHeading")}
                </p>
                <ul>
                  {report.claims.map((claim) => (
                    <ClaimRow
                      key={claim.criterionId}
                      claim={claim}
                      report={report}
                      t={t}
                      onRevealPath={onRevealPath}
                    />
                  ))}
                </ul>
              </div>
            </div>
          )}
        </div>
      )}
    </section>
  );
}

export default CriteriaCoverageSection;
