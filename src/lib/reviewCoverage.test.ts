/**
 * Pins the two halves of `reviewCoverage.ts` that a reviewer's trust rests
 * on: the computed facts are deterministic and derived from the real diff, and
 * a model claim can never become coverage without a citation that resolves to
 * a fact this app computed itself.
 */
import { beforeEach, describe, expect, it, vi } from "vitest";

const mocks = vi.hoisted(() => ({
  resolveTarget: vi.fn(),
  attemptStream: vi.fn(),
  effortForTarget: vi.fn(),
  describeUsageTarget: vi.fn(),
}));

vi.mock("@tauri-apps/api/core", () => ({
  invoke: vi.fn(),
  isTauri: () => false,
}));
vi.mock("./agentLoop", () => ({
  resolveTarget: (...args: unknown[]) => mocks.resolveTarget(...args),
}));
vi.mock("./turnEngine", () => ({
  attemptStream: (...args: unknown[]) => mocks.attemptStream(...args),
  describeUsageTarget: (...args: unknown[]) => mocks.describeUsageTarget(...args),
}));
vi.mock("../store/modelStore", () => ({
  effortForTarget: (...args: unknown[]) => mocks.effortForTarget(...args),
}));

import {
  MAX_REVIEW_CRITERIA,
  MAX_REVIEW_HUNKS,
  REVIEW_FILE_CAP,
  buildCoverageMessages,
  buildCoverageReport,
  checkClaims,
  computeReviewFacts,
  isReportStale,
  mapReviewCoverage,
  parseCoverageEnvelope,
  parseCriteriaInput,
  type ReviewCoverageInput,
  type ReviewCriterion,
} from "./reviewCoverage";

const CRITERIA: ReviewCriterion[] = [
  { criterionId: "C1", text: "Rate limit rejects the 11th request in a minute." },
  { criterionId: "C2", text: "The limit is configurable per account." },
];

/** Two files, each with two separated changed regions, so hunk numbering and
 * line ranges are both observable. */
function review(overrides: Partial<ReviewCoverageInput> = {}): ReviewCoverageInput {
  return {
    branch: "feat/limits",
    target: "origin/develop",
    total_added: 4,
    total_deleted: 1,
    files: [
      {
        path: "src/limit.ts",
        old_content: ["keep-a", "old-b", "keep-c", "keep-d", "keep-e", "keep-f", "keep-g", "keep-h", "keep-i", "keep-j", "old-k"].join("\n"),
        new_content: ["keep-a", "export function limit() {}", "keep-c", "keep-d", "keep-e", "keep-f", "keep-g", "keep-h", "keep-i", "keep-j", "new-k"].join("\n"),
        added: 2,
        deleted: 2,
        binary: false,
      },
      {
        path: "src-tauri/src/limit.rs",
        old_content: "fn old() {}",
        new_content: "pub fn enforce() {}",
        added: 1,
        deleted: 1,
        binary: false,
      },
    ],
    ...overrides,
  };
}

describe("computeReviewFacts", () => {
  it("derives citable hunks with real line ranges from the diff", () => {
    const facts = computeReviewFacts(review(), CRITERIA, "branch");

    // Every changed file is accounted for, in a stable path-sorted order.
    expect([...facts.files].map((file) => file.path).sort()).toEqual([
      "src-tauri/src/limit.rs",
      "src/limit.ts",
    ]);
    expect(facts.files.map((file) => file.path)).toEqual(
      [...facts.files.map((file) => file.path)].sort((a, b) => a.localeCompare(b)),
    );

    const inLimitTs = facts.hunks.filter((hunk) => hunk.path === "src/limit.ts");
    expect(inLimitTs).toHaveLength(2);
    // Line 2 changed, then a run of unchanged lines, then line 11 changed —
    // two separate hunks, not one span swallowing the middle.
    expect(inLimitTs[0].newStart).toBe(2);
    expect(inLimitTs[0].newEnd).toBe(2);
    expect(inLimitTs[1].newStart).toBe(11);
    expect(inLimitTs[1].newEnd).toBe(11);
    expect(inLimitTs[0].added).toBe(1);
    expect(inLimitTs[0].removed).toBe(1);

    // Ids are app-assigned and unique — the only thing a model may cite.
    expect(new Set(facts.hunks.map((hunk) => hunk.hunkId)).size).toBe(facts.hunks.length);
  });

  it("text-matches changed exported and pub declarations in both languages", () => {
    const facts = computeReviewFacts(review(), CRITERIA, "branch");
    const ts = facts.files.find((file) => file.path === "src/limit.ts");
    const rs = facts.files.find((file) => file.path.endsWith(".rs"));
    expect(ts?.changedSymbols).toContain("limit");
    expect(rs?.changedSymbols).toContain("enforce");
  });

  it("lists a binary file as uncitable rather than inventing hunks for it", () => {
    const input = review();
    input.files[0] = { ...input.files[0], binary: true, old_content: "", new_content: "" };
    const facts = computeReviewFacts(input, CRITERIA, "branch");
    expect(facts.uncitableFilePaths).toContain("src/limit.ts");
    expect(facts.hunks.every((hunk) => hunk.path !== "src/limit.ts")).toBe(true);
  });

  it("is deterministic, and the digest tracks the diff it was computed from", () => {
    const first = computeReviewFacts(review(), CRITERIA, "branch");
    const second = computeReviewFacts(review(), CRITERIA, "branch");
    expect(second.digest).toBe(first.digest);

    const changed = review();
    changed.files[1] = { ...changed.files[1], new_content: "pub fn enforce_hard() {}" };
    const afterEdit = computeReviewFacts(changed, CRITERIA, "branch");
    expect(afterEdit.digest).not.toBe(first.digest);

    const report = buildCoverageReport(first, checkClaims([], first), "test model");
    expect(isReportStale(report, first)).toBe(false);
    expect(isReportStale(report, afterEdit)).toBe(true);
  });

  it("flags a payload sitting at git_review's file cap as possibly incomplete", () => {
    const small = computeReviewFacts(review(), CRITERIA, "branch");
    expect(small.filesPossiblyTruncated).toBe(false);
    expect(small.hunksPossiblyTruncated).toBe(false);

    // `git_review` skips files past the cap without saying so (git.rs), so a
    // payload at the cap has to be read as "maybe there was more".
    const capped = review({
      files: Array.from({ length: REVIEW_FILE_CAP }, (_, index) => ({
        path: `src/file${index}.ts`,
        old_content: "old",
        new_content: "new",
        added: 1,
        deleted: 1,
        binary: false,
      })),
    });
    const cappedFacts = computeReviewFacts(capped, CRITERIA, "branch");
    expect(cappedFacts.filesPossiblyTruncated).toBe(true);
    // The warning reaches the model too, so it cannot call a criterion
    // uncovered off a list it was told is partial.
    const [system] = buildCoverageMessages(cappedFacts);
    expect(system.content).toContain("absence from this list is not proof");
  });

  it("flags a diff with more hunks than one pass carries", () => {
    const many = review({
      files: Array.from({ length: MAX_REVIEW_HUNKS + 5 }, (_, index) => ({
        path: `src/file${index}.ts`,
        old_content: "old",
        new_content: "new",
        added: 1,
        deleted: 1,
        binary: false,
      })),
    });
    const facts = computeReviewFacts(many, CRITERIA, "branch");
    expect(facts.hunks).toHaveLength(MAX_REVIEW_HUNKS);
    expect(facts.hunksPossiblyTruncated).toBe(true);
  });

  it("separates the same diff read against a different base", () => {
    const branch = computeReviewFacts(review(), CRITERIA, "branch");
    const working = computeReviewFacts(review(), CRITERIA, "working");
    expect(working.digest).not.toBe(branch.digest);
  });
});

describe("parseCriteriaInput", () => {
  it("strips bullet and numbered prefixes and drops blank lines", () => {
    const criteria = parseCriteriaInput("- first\n\n2. second\n* third\n");
    expect(criteria).toEqual([
      { criterionId: "C1", text: "first" },
      { criterionId: "C2", text: "second" },
      { criterionId: "C3", text: "third" },
    ]);
  });

  it("refuses a pasted document rather than silently truncating it", () => {
    const tooMany = Array.from({ length: MAX_REVIEW_CRITERIA + 1 }, (_, i) => `criterion ${i}`).join("\n");
    expect(() => parseCriteriaInput(tooMany)).toThrow(/at most/i);
  });
});

describe("parseCoverageEnvelope", () => {
  const facts = computeReviewFacts(review(), CRITERIA, "branch");

  it("accepts a fenced envelope and normalizes an unknown verdict to uncovered", () => {
    const claims = parseCoverageEnvelope(
      '```json\n{"claims":[{"criterionId":"C1","verdict":"probably","citedHunkIds":["H1"],"rationale":"r"}]}\n```',
      facts,
    );
    expect(claims).toEqual([
      { criterionId: "C1", verdict: "uncovered", citedHunkIds: ["H1"], rationale: "r" },
    ]);
  });

  it("drops claims about criteria that were never given, and duplicates", () => {
    const claims = parseCoverageEnvelope(
      JSON.stringify({
        claims: [
          { criterionId: "C1", verdict: "covered", citedHunkIds: ["H1"], rationale: "a" },
          { criterionId: "C1", verdict: "uncovered", citedHunkIds: [], rationale: "duplicate" },
          { criterionId: "C99", verdict: "covered", citedHunkIds: ["H1"], rationale: "invented" },
        ],
      }),
      facts,
    );
    expect(claims.map((claim) => claim.criterionId)).toEqual(["C1"]);
    expect(claims[0].rationale).toBe("a");
  });

  it("throws on a non-JSON reply, a non-object, and an empty claim set", () => {
    expect(() => parseCoverageEnvelope("I had a look and it seems fine!", facts)).toThrow(/JSON coverage envelope/);
    expect(() => parseCoverageEnvelope("[]", facts)).toThrow(/not a JSON object/);
    expect(() => parseCoverageEnvelope('{"claims":[]}', facts)).toThrow(/no claim/);
    expect(() => parseCoverageEnvelope('{"notes":"x"}', facts)).toThrow(/claims array/);
  });
});

describe("checkClaims — the anti-fabrication guard", () => {
  const facts = computeReviewFacts(review(), CRITERIA, "branch");

  it("rejects a claim citing a hunk id that does not exist, and keeps the evidence", () => {
    const [checked] = checkClaims(
      [{ criterionId: "C1", verdict: "covered", citedHunkIds: ["H1", "H999"], rationale: "r" }],
      facts,
    ).filter((claim) => claim.criterionId === "C1");

    expect(checked.status).toBe("rejected");
    expect(checked.invalidCitations).toEqual(["H999"]);
    // The real citation is still recorded — the UI shows what was invented
    // next to what was not, instead of hiding the whole claim.
    expect(checked.validCitations).toEqual(["H1"]);
  });

  it("marks an uncitable coverage claim unsupported", () => {
    const checked = checkClaims(
      [{ criterionId: "C1", verdict: "covered", citedHunkIds: [], rationale: "trust me" }],
      facts,
    );
    expect(checked.find((claim) => claim.criterionId === "C1")?.status).toBe("unsupported");
  });

  it("accepts an uncovered verdict with no citations", () => {
    const checked = checkClaims(
      [{ criterionId: "C1", verdict: "uncovered", citedHunkIds: [], rationale: "nothing touches it" }],
      facts,
    );
    expect(checked.find((claim) => claim.criterionId === "C1")?.status).toBe("accepted");
  });

  it("accounts for every criterion even when the model said nothing about one", () => {
    const checked = checkClaims(
      [{ criterionId: "C1", verdict: "covered", citedHunkIds: ["H1"], rationale: "r" }],
      facts,
    );
    expect(checked.map((claim) => claim.criterionId)).toEqual(["C1", "C2"]);
    expect(checked[1].status).toBe("unsupported");
    expect(checked[1].claimed).toBe("uncovered");
  });
});

describe("buildCoverageReport — roll-ups are computed, never claimed", () => {
  const facts = computeReviewFacts(review(), CRITERIA, "branch");

  it("leaves a criterion uncovered when its coverage claim was rejected", () => {
    const claims = checkClaims(
      [
        { criterionId: "C1", verdict: "covered", citedHunkIds: ["H404"], rationale: "fabricated" },
        { criterionId: "C2", verdict: "covered", citedHunkIds: ["H1"], rationale: "real" },
      ],
      facts,
    );
    const report = buildCoverageReport(facts, claims, "test model");

    expect(report.uncoveredCriterionIds).toEqual(["C1"]);
    expect(report.unverifiedCriterionIds).toEqual(["C1"]);
  });

  it("treats an unsupported coverage claim as uncovered too", () => {
    const claims = checkClaims(
      [
        { criterionId: "C1", verdict: "covered", citedHunkIds: [], rationale: "" },
        { criterionId: "C2", verdict: "partial", citedHunkIds: ["H1"], rationale: "partly" },
      ],
      facts,
    );
    const report = buildCoverageReport(facts, claims, "test model");
    // C2 is accepted but only "partial", so it is not covered either.
    expect(report.uncoveredCriterionIds).toEqual(["C1", "C2"]);
    expect(report.unverifiedCriterionIds).toEqual(["C1"]);
  });

  it("reports hunks no accepted claim cites", () => {
    const claims = checkClaims(
      [{ criterionId: "C1", verdict: "covered", citedHunkIds: ["H1"], rationale: "r" }],
      facts,
    );
    const report = buildCoverageReport(facts, claims, "test model");
    expect(report.uncitedHunkIds).not.toContain("H1");
    expect(report.uncitedHunkIds).toEqual(facts.hunks.slice(1).map((hunk) => hunk.hunkId));
  });

  it("does not let a rejected claim's citation count as explaining its hunk", () => {
    const claims = checkClaims(
      [{ criterionId: "C1", verdict: "covered", citedHunkIds: ["H1", "H999"], rationale: "r" }],
      facts,
    );
    const report = buildCoverageReport(facts, claims, "test model");
    expect(report.uncitedHunkIds).toContain("H1");
  });
});

describe("buildCoverageMessages", () => {
  it("wraps the diff as untrusted data and only offers real hunk ids", () => {
    const facts = computeReviewFacts(review(), CRITERIA, "branch");
    const [system, user] = buildCoverageMessages(facts);

    expect(system.content).toContain("rejects the whole claim");
    expect(user.content).toContain("[Untrusted data from");
    for (const hunk of facts.hunks) expect(user.content).toContain(hunk.hunkId);
    expect(user.content).toContain("C1: Rate limit rejects the 11th request in a minute.");
  });
});

describe("mapReviewCoverage", () => {
  beforeEach(() => {
    vi.clearAllMocks();
    mocks.resolveTarget.mockResolvedValue({ kind: "ollama", baseUrl: "http://localhost:11434", model: "qwen" });
    mocks.effortForTarget.mockReturnValue("medium");
    mocks.describeUsageTarget.mockReturnValue("Ollama · qwen");
  });

  it("runs one tool-free call that does not record session usage, and checks the reply", async () => {
    mocks.attemptStream.mockResolvedValue({
      content: JSON.stringify({
        claims: [
          { criterionId: "C1", verdict: "covered", citedHunkIds: ["H1"], rationale: "the guard" },
          { criterionId: "C2", verdict: "covered", citedHunkIds: ["H_not_real"], rationale: "invented" },
        ],
      }),
    });

    const report = await mapReviewCoverage(review(), CRITERIA, "branch");

    const [, , tools, , , sessionId, onDelta, recordUsage] = mocks.attemptStream.mock.calls[0];
    expect(tools).toEqual([]);
    expect(String(sessionId)).toMatch(/^review-coverage:/);
    expect(onDelta).toBeUndefined();
    expect(recordUsage).toBe(false);

    expect(report.modelLabel).toBe("Ollama · qwen");
    expect(report.claims.find((claim) => claim.criterionId === "C2")?.status).toBe("rejected");
    expect(report.uncoveredCriterionIds).toEqual(["C2"]);
  });

  it("surfaces a stream error and refuses a run with no criteria or no changes", async () => {
    mocks.attemptStream.mockResolvedValue({ content: "", streamError: "model offline" });
    await expect(mapReviewCoverage(review(), CRITERIA, "branch")).rejects.toThrow("model offline");

    await expect(mapReviewCoverage(review(), [], "branch")).rejects.toThrow(/at least one acceptance criterion/);
    await expect(
      mapReviewCoverage({ ...review(), files: [] }, CRITERIA, "branch"),
    ).rejects.toThrow(/no citable text changes/);
  });
});
