// @vitest-environment jsdom
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

const mocks = vi.hoisted(() => ({
  client: {
    settings: vi.fn(),
    listCandidates: vi.fn(),
    learnedSkills: vi.fn(),
    discover: vi.fn(),
    evaluations: vi.fn(),
    improvementEvidence: vi.fn(),
    runEvidence: vi.fn(),
    beginImprovement: vi.fn(),
  },
  bump: vi.fn(),
  clearFocus: vi.fn(),
  draftCandidate: vi.fn(),
}));

vi.mock("../../lib/skillLearningClient", async () => {
  const actual = await vi.importActual<typeof import("../../lib/skillLearningClient")>(
    "../../lib/skillLearningClient",
  );
  return { ...actual, skillLearningClient: mocks.client };
});
vi.mock("../../lib/nativeSkillsClient", async () => {
  const actual = await vi.importActual<typeof import("../../lib/nativeSkillsClient")>(
    "../../lib/nativeSkillsClient",
  );
  return {
    ...actual,
    nativeSkillsClient: {
      rollback: vi.fn(),
      setEnabled: vi.fn(),
      uninstall: vi.fn(),
    },
  };
});
vi.mock("../../lib/skillLearningReflection", () => ({ draftCandidate: mocks.draftCandidate }));
vi.mock("../../store/nativeSkillsStore", () => ({
  useNativeSkillsStore: (selector: (state: { bump: () => void }) => unknown) => selector({ bump: mocks.bump }),
}));
vi.mock("../../store/skillLearningFocusStore", () => ({
  useSkillLearningFocusStore: (
    selector: (state: { focus: null; clear: () => void }) => unknown,
  ) => selector({ focus: null, clear: mocks.clearFocus }),
}));

import { SkillLearningPanel } from "./SkillLearningPanel";

const summary = {
  command: "review",
  scope: "global",
  version: "1.0.0",
  active_sha256: "a".repeat(64),
  enabled: true,
  deprecated: false,
  deprecation_reason: null,
  provenance: {
    origin: "learned",
    candidate_id: "candidate-a",
    source_run_ids: ["source-1"],
    source_kind: "manual_run_capture",
    parent_skill_sha256: null,
    installed_sha256: "a".repeat(64),
    evaluation_ids: [],
    promotion_policy: "user_approved",
    approval_id: "approval-1",
    promoted_at_unix_ms: 1,
  },
  previous_sha256: [],
  uses: 3,
  failures: 1,
  corrections: 1,
  last_used_at_unix_ms: 1,
  quality: {
    command: "review",
    scope: "global",
    active_sha256: "a".repeat(64),
    state: "needs_attention",
    reasons: ["A user correction was recorded after this version ran."],
    total_runs: 3,
    verified_successes: 2,
    verified_failures: 1,
    unknown_verification: 0,
    cancelled_runs: 0,
    corrections: 1,
    improvement_evidence_count: 6,
    recent_runs: [
      {
        run_id: "run-1",
        outcome: "failure",
        verification_passed: false,
        user_corrected: true,
        failure_signature: "verification:failed",
        recorded_at_unix_ms: 1,
        summary: "verification failed",
        evidence_available: true,
      },
    ],
    repeated_failure_signatures: [],
    last_used_at_unix_ms: 1,
    last_verified_success_at_unix_ms: 1,
    last_failure_at_unix_ms: 1,
    last_correction_at_unix_ms: 1,
    open_improvement_candidate_id: null,
  },
  history: [],
};

const evidence = {
  run_id: "run-1",
  completed: true,
  failed: false,
  cancelled: false,
  user_text: "review",
  tool_calls: [
    {
      event_id: "event-1",
      tool_call_id: "call-1",
      tool_name: "read_file",
      succeeded: true,
      mutation: false,
      arguments: null,
      output_excerpt: null,
      outcome: "succeeded",
      failure_excerpt: null,
      path: null,
    },
  ],
  verifications: [],
  changed_files: [],
  invoked_skills: [{ command: "review", scope: "global", sha256: "a".repeat(64) }],
  summary: "bounded evidence",
  failure_signatures: [],
  checkpoint_id: null,
};

beforeEach(() => {
  vi.clearAllMocks();
  mocks.client.settings.mockResolvedValue({ policy: "manual", allow_global_scope: true });
  mocks.client.listCandidates.mockResolvedValue([]);
  mocks.client.learnedSkills.mockResolvedValue([summary]);
  mocks.client.discover.mockResolvedValue([]);
  mocks.client.evaluations.mockResolvedValue([]);
  mocks.client.improvementEvidence.mockResolvedValue(
    Array.from({ length: 6 }, (_, index) => ({
      run_id: "run-" + (index + 1),
      outcome: "success",
      verification_passed: true,
      user_corrected: false,
      failure_signature: null,
      recorded_at_unix_ms: index + 1,
      summary: "evidence " + (index + 1),
    })),
  );
  mocks.client.runEvidence.mockResolvedValue(evidence);
  mocks.client.beginImprovement.mockResolvedValue({
    candidate_id: "candidate-b",
    proposed_command: "review",
  });
});

afterEach(() => {
  cleanup();
});

describe("SkillLearningPanel quality and improvement UX", () => {
  it("shows backend quality reasons and resolves bounded evidence", async () => {
    render(<SkillLearningPanel />);
    expect(await screen.findByRole("button", { name: "Needs attention" })).toBeTruthy();

    fireEvent.click(screen.getByRole("button", { name: "Needs attention" }));
    expect(await screen.findByText("A user correction was recorded after this version ran.")).toBeTruthy();
    fireEvent.click(screen.getByRole("button", { name: "View evidence" }));
    await waitFor(() => expect(mocks.client.runEvidence).toHaveBeenCalledWith("global", "review", "run-1"));
    expect(document.body.textContent).toContain("read_file");
  });

  it("keeps explicit improvement available under Manual policy and caps selection at five", async () => {
    render(<SkillLearningPanel />);
    fireEvent.click(await screen.findByRole("button", { name: "Improve skill" }));
    expect(await screen.findByText("Improvement evidence (max 5)")).toBeTruthy();
    const checkboxes = (await screen.findAllByRole("checkbox")).slice(-6);
    expect(checkboxes).toHaveLength(6);
    await waitFor(() =>
      expect(checkboxes.slice(0, 5).every((checkbox) => (checkbox as HTMLInputElement).checked)).toBe(true),
    );
    expect(checkboxes[5]).toHaveProperty("disabled", true);
    await waitFor(() => expect(mocks.client.improvementEvidence).toHaveBeenCalledWith("global", "review"));
  });
});
