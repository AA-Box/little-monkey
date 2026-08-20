// @vitest-environment jsdom
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";

const capture = vi.fn();
const draftCandidate = vi.fn();

vi.mock("../../lib/skillLearningClient", () => ({
  skillLearningClient: { capture: (...args: unknown[]) => capture(...args) },
}));
vi.mock("../../lib/skillLearningReflection", () => ({
  draftCandidate: (...args: unknown[]) => draftCandidate(...args),
}));

import { SaveSkillRow } from "./SaveSkillRow";
import { useSkillLearningFocusStore } from "../../store/skillLearningFocusStore";
import type { LearningCandidate, CaptureOutcome } from "../../lib/skillLearningClient";

function candidate(overrides: Partial<LearningCandidate> = {}): LearningCandidate {
  return {
    candidate_id: "learn-1",
    status: "detected",
    proposed_command: "retry-wrapper",
    ...overrides,
  } as LearningCandidate;
}

const notice = { runId: "run-1", userText: "make this reusable", scope: "workspace" as const };

beforeEach(() => {
  capture.mockReset();
  draftCandidate.mockReset();
  useSkillLearningFocusStore.getState().clear();
});

afterEach(() => {
  cleanup();
  vi.restoreAllMocks();
});

describe("SaveSkillRow", () => {
  it("captures, drafts, focuses, and opens Settings for a new candidate", async () => {
    const created: CaptureOutcome = { kind: "created", candidate: candidate() };
    capture.mockResolvedValue(created);
    draftCandidate.mockResolvedValue({ candidate: candidate({ status: "staged" }), declined: false, error: null });
    const onOpenSettingsTab = vi.fn();

    render(<SaveSkillRow notice={notice} onOpenSettingsTab={onOpenSettingsTab} />);
    fireEvent.click(screen.getByRole("button", { name: "Save as skill" }));

    await waitFor(() => {
      expect(capture).toHaveBeenCalledWith("run-1", "make this reusable");
      expect(draftCandidate).toHaveBeenCalledWith("learn-1");
      expect(useSkillLearningFocusStore.getState().candidateId).toBe("learn-1");
      expect(onOpenSettingsTab).toHaveBeenCalledWith("prompts");
    });
  });

  it("focuses an existing staged candidate without drafting it again", async () => {
    capture.mockResolvedValue({ kind: "existing", candidate: candidate({ status: "staged" }) });
    const onOpenSettingsTab = vi.fn();

    render(<SaveSkillRow notice={notice} onOpenSettingsTab={onOpenSettingsTab} />);
    fireEvent.click(screen.getByRole("button", { name: "Save as skill" }));

    await waitFor(() => {
      expect(draftCandidate).not.toHaveBeenCalled();
      expect(useSkillLearningFocusStore.getState().candidateId).toBe("learn-1");
      expect(onOpenSettingsTab).toHaveBeenCalledWith("prompts");
    });
  });

  it("shows an already-installed skill and offers to view it", async () => {
    capture.mockResolvedValue({
      kind: "already_installed",
      candidate: candidate({ status: "promoted" }),
    });
    const onOpenSettingsTab = vi.fn();

    render(<SaveSkillRow notice={notice} onOpenSettingsTab={onOpenSettingsTab} />);
    fireEvent.click(screen.getByRole("button", { name: "Save as skill" }));

    expect(await screen.findByText("Already saved as /retry-wrapper")).toBeTruthy();
    expect(draftCandidate).not.toHaveBeenCalled();
    expect(useSkillLearningFocusStore.getState().candidateId).toBeNull();
    fireEvent.click(screen.getByRole("button", { name: "View skill" }));
    expect(onOpenSettingsTab).toHaveBeenCalledWith("prompts");
  });
});

