/**
 * What a checked claim actually puts on screen. `reviewCoverage.test.ts` pins
 * the decision (is this claim accepted, unsupported, or rejected?); this pins
 * that the decision is *shown* — a fabricated citation has to be visible as
 * discarded, not quietly dropped, or the guard buys the reader nothing.
 *
 * Rendered with `renderToStaticMarkup`, since this repo has no DOM test
 * environment (vitest.config.ts uses `environment: "node"`) — the same
 * approach as `RuntimeHubPanel.test.tsx` and `AddCustomModelForm.test.tsx`.
 */
import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it } from "vitest";

import { ClaimRow } from "./CriteriaCoverageSection";
import {
  buildCoverageReport,
  checkClaims,
  computeReviewFacts,
  type RawCriterionClaim,
  type ReviewCoverageInput,
  type ReviewCriterion,
} from "../../lib/reviewCoverage";

/** Echoes the key plus its vars, so an assertion can see which string was
 * chosen and what was interpolated into it. */
function t(key: string, vars?: Record<string, string | number>): string {
  const rendered = vars ? Object.entries(vars).map(([name, value]) => `${name}=${value}`).join(",") : "";
  return rendered ? `${key}(${rendered})` : key;
}

const CRITERIA: ReviewCriterion[] = [
  { criterionId: "C1", text: "Rate limit rejects the 11th request." },
];

const REVIEW: ReviewCoverageInput = {
  branch: "feat/limits",
  target: "origin/develop",
  total_added: 1,
  total_deleted: 1,
  files: [
    {
      path: "src/limit.ts",
      old_content: "const max = 5;",
      new_content: "export const max = 10;",
      added: 1,
      deleted: 1,
      binary: false,
    },
  ],
};

function renderClaim(raw: RawCriterionClaim): string {
  const facts = computeReviewFacts(REVIEW, CRITERIA, "branch");
  const claims = checkClaims([raw], facts);
  const report = buildCoverageReport(facts, claims, "test model");
  return renderToStaticMarkup(<ClaimRow claim={claims[0]} report={report} t={t} />);
}

describe("ClaimRow", () => {
  it("shows an accepted claim's citation as a checkable file:line reference", () => {
    const html = renderClaim({
      criterionId: "C1",
      verdict: "covered",
      citedHunkIds: ["H1"],
      rationale: "raises the ceiling to 10",
    });

    expect(html).toContain("ReviewPanel.coverageVerdict_covered");
    expect(html).toContain("H1 · src/limit.ts:1");
    expect(html).toContain("raises the ceiling to 10");
    expect(html).not.toContain("coverageRejectedNote");
    expect(html).not.toContain("coverageUnsupportedNote");
  });

  it("names the invented hunk id when a claim is rejected, and drops its rationale", () => {
    const html = renderClaim({
      criterionId: "C1",
      verdict: "covered",
      citedHunkIds: ["H42"],
      rationale: "the guard I did not read",
    });

    expect(html).toContain("ReviewPanel.coverageRejectedNote(ids=H42)");
    // A rejected claim's reasoning is not shown as if it explained anything.
    expect(html).not.toContain("the guard I did not read");
  });

  it("marks a coverage claim with no citation unsupported", () => {
    const html = renderClaim({
      criterionId: "C1",
      verdict: "covered",
      citedHunkIds: [],
      rationale: "looks done to me",
    });
    expect(html).toContain("ReviewPanel.coverageUnsupportedNote");
  });

  it("says so plainly when the model never mentioned the criterion", () => {
    const facts = computeReviewFacts(REVIEW, CRITERIA, "branch");
    const claims = checkClaims([], facts);
    const report = buildCoverageReport(facts, claims, "test model");
    const html = renderToStaticMarkup(<ClaimRow claim={claims[0]} report={report} t={t} />);

    expect(html).toContain("ReviewPanel.coverageNoClaimNote");
    expect(html).toContain("ReviewPanel.coverageVerdict_uncovered");
  });

  it("renders the criterion's own text, not just its id", () => {
    const html = renderClaim({ criterionId: "C1", verdict: "uncovered", citedHunkIds: [], rationale: "" });
    expect(html).toContain("Rate limit rejects the 11th request.");
  });
});
