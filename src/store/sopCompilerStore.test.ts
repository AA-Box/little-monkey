import { beforeAll, beforeEach, describe, expect, it, vi } from "vitest";

// `sopCompilerStore.ts` drives its one-shot compiler call through
// `agentLoop.ts`'s `resolveTarget` + `turnEngine.ts`'s `attemptStream` —
// exactly the same pair `compactSessionNow` uses for its own one-shot
// summary call (see that module's doc comment) — mocked here so these tests
// pin the STORE's own behavior (persistence, draft lifecycle, hand-off to
// Skill Proposals) without needing a real streaming provider.
const resolveTargetMock = vi.fn();
vi.mock("../lib/agentLoop", () => ({
  resolveTarget: (...args: unknown[]) => resolveTargetMock(...args),
}));

const attemptStreamMock = vi.fn();
vi.mock("../lib/turnEngine", () => ({
  attemptStream: (...args: unknown[]) => attemptStreamMock(...args),
}));

const dialogOpenMock = vi.fn();
vi.mock("@tauri-apps/plugin-dialog", () => ({
  open: (...args: unknown[]) => dialogOpenMock(...args),
}));

const readTextFileMock = vi.fn();
const statMock = vi.fn();
vi.mock("@tauri-apps/plugin-fs", () => ({
  readTextFile: (...args: unknown[]) => readTextFileMock(...args),
  stat: (...args: unknown[]) => statMock(...args),
}));

import { useSopCompilerStore } from "./sopCompilerStore";
import { usePromptStore } from "./promptStore";
import { useSkillProposalStore } from "./skillProposalStore";

const WELL_FORMED_REPLY = JSON.stringify({
  name: "Rotate API Credentials",
  summary: "Rotates the payments API key and confirms the new key is live.",
  suggestedCommand: "rotate-api-credentials",
  steps: [{ order: 1, action: "Generate a new API key." }],
  inputs: [{ name: "environment", description: "Target environment.", required: true }],
  policyGates: [{ label: "On-call approval", description: "Needs sign-off.", riskLevel: "high" }],
  tests: [{ label: "New key works", expected: "Test request returns 200." }],
  evidence: [{ label: "Vault entry", description: "Screenshot of the new secret version." }],
});

describe("sopCompilerStore", () => {
  beforeAll(() => {
    const values = new Map<string, string>();
    Object.defineProperty(globalThis, "localStorage", {
      configurable: true,
      value: {
        clear: () => values.clear(),
        getItem: (key: string) => values.get(key) ?? null,
        setItem: (key: string, value: string) => values.set(key, value),
        removeItem: (key: string) => values.delete(key),
      },
    });
  });

  beforeEach(() => {
    localStorage.clear();
    resolveTargetMock.mockReset();
    attemptStreamMock.mockReset();
    dialogOpenMock.mockReset();
    readTextFileMock.mockReset();
    statMock.mockReset();
    resolveTargetMock.mockResolvedValue({ kind: "local", baseUrl: "http://localhost:8090" });
    useSopCompilerStore.setState({
      sourceText: "",
      sourceFileName: null,
      compiling: false,
      importing: false,
      error: null,
      drafts: [],
      selectedDraftId: null,
    });
    usePromptStore.setState({ entries: [], defaultPersonaId: null, hasSeededDefaults: true, persistError: null });
    useSkillProposalStore.setState({ proposals: [] });
  });

  it("refuses to compile empty source without ever calling the model", async () => {
    await useSopCompilerStore.getState().compile();
    expect(attemptStreamMock).not.toHaveBeenCalled();
    expect(useSopCompilerStore.getState().error).toMatch(/paste or import/i);
  });

  it("compiles pasted source into a persisted, selected draft", async () => {
    attemptStreamMock.mockResolvedValue({ content: WELL_FORMED_REPLY, streamError: null, toolCalls: [], contentStarted: true });
    useSopCompilerStore.getState().setSourceText("Step 1: rotate the key.\nStep 2: confirm it works.");

    await useSopCompilerStore.getState().compile();

    const state = useSopCompilerStore.getState();
    expect(state.compiling).toBe(false);
    expect(state.error).toBeNull();
    expect(state.drafts).toHaveLength(1);
    expect(state.drafts[0].draft.name).toBe("Rotate API Credentials");
    expect(state.drafts[0].status).toBe("draft");
    expect(state.selectedDraftId).toBe(state.drafts[0].id);

    // `recordUsage` (8th positional arg) must be threaded through as `false`
    // — this one-shot compiler call is not a chat turn and must never
    // pollute a real session's usage ledger.
    expect(attemptStreamMock).toHaveBeenCalledTimes(1);
    const call = attemptStreamMock.mock.calls[0];
    expect(call[7]).toBe(false);
  });

  it("surfaces a compile error and never fabricates a draft", async () => {
    attemptStreamMock.mockResolvedValue({ content: "not json", streamError: null, toolCalls: [], contentStarted: true });
    useSopCompilerStore.getState().setSourceText("Some SOP text.");

    await useSopCompilerStore.getState().compile();

    const state = useSopCompilerStore.getState();
    expect(state.drafts).toHaveLength(0);
    expect(state.error).toMatch(/did not return a compilable workflow/i);
  });

  it("persists drafts across a store re-hydration", async () => {
    attemptStreamMock.mockResolvedValue({ content: WELL_FORMED_REPLY, streamError: null, toolCalls: [], contentStarted: true });
    useSopCompilerStore.getState().setSourceText("Some SOP text.");
    await useSopCompilerStore.getState().compile();

    const persistedRaw = localStorage.getItem("little-monkey-sop-compiler-drafts-v1");
    expect(persistedRaw).toBeTruthy();
    const persisted = JSON.parse(persistedRaw as string);
    expect(persisted.drafts).toHaveLength(1);
    expect(persisted.drafts[0].draft.suggestedCommand).toBe("rotate-api-credentials");
  });

  it("imports a file's contents into sourceText via the dialog + fs plugins", async () => {
    dialogOpenMock.mockResolvedValue("/Users/me/runbook.md");
    statMock.mockResolvedValue({ size: 1024 });
    readTextFileMock.mockResolvedValue("# Runbook\n1. Do the thing.");

    await useSopCompilerStore.getState().importFromFile();

    const state = useSopCompilerStore.getState();
    expect(state.sourceText).toBe("# Runbook\n1. Do the thing.");
    expect(state.sourceFileName).toBe("runbook.md");
    expect(state.importing).toBe(false);
  });

  it("rejects an import above the file size limit", async () => {
    dialogOpenMock.mockResolvedValue("/Users/me/huge.md");
    statMock.mockResolvedValue({ size: 50 * 1024 * 1024 });

    await useSopCompilerStore.getState().importFromFile();

    expect(readTextFileMock).not.toHaveBeenCalled();
    expect(useSopCompilerStore.getState().error).toMatch(/larger than/i);
  });

  it("does nothing when the user cancels the file picker", async () => {
    dialogOpenMock.mockResolvedValue(null);
    await useSopCompilerStore.getState().importFromFile();
    expect(statMock).not.toHaveBeenCalled();
    expect(useSopCompilerStore.getState().sourceText).toBe("");
  });

  it("sendToReview hands the compiled draft to the EXISTING quarantined skill-proposal flow, never activating it directly", async () => {
    attemptStreamMock.mockResolvedValue({ content: WELL_FORMED_REPLY, streamError: null, toolCalls: [], contentStarted: true });
    useSopCompilerStore.getState().setSourceText("Some SOP text.");
    await useSopCompilerStore.getState().compile();
    const draftId = useSopCompilerStore.getState().drafts[0].id;

    const proposal = await useSopCompilerStore.getState().sendToReview(draftId);

    expect(proposal.status).toBe("quarantined");
    expect(proposal.command).toBe("rotate-api-credentials");
    // Still not usable as a real /command until approved.
    expect(usePromptStore.getState().entries).toHaveLength(0);
    expect(useSkillProposalStore.getState().proposals).toHaveLength(1);

    const state = useSopCompilerStore.getState();
    expect(state.drafts[0].status).toBe("sent_for_review");
    expect(state.drafts[0].proposalId).toBe(proposal.id);
  });

  it("refuses to send the same draft to review twice", async () => {
    attemptStreamMock.mockResolvedValue({ content: WELL_FORMED_REPLY, streamError: null, toolCalls: [], contentStarted: true });
    useSopCompilerStore.getState().setSourceText("Some SOP text.");
    await useSopCompilerStore.getState().compile();
    const draftId = useSopCompilerStore.getState().drafts[0].id;

    await useSopCompilerStore.getState().sendToReview(draftId);
    await expect(useSopCompilerStore.getState().sendToReview(draftId)).rejects.toThrow(/already sent/i);
    expect(useSkillProposalStore.getState().proposals).toHaveLength(1);
  });

  it("discardDraft removes a draft and clears the selection if it was selected", async () => {
    attemptStreamMock.mockResolvedValue({ content: WELL_FORMED_REPLY, streamError: null, toolCalls: [], contentStarted: true });
    useSopCompilerStore.getState().setSourceText("Some SOP text.");
    await useSopCompilerStore.getState().compile();
    const draftId = useSopCompilerStore.getState().drafts[0].id;

    useSopCompilerStore.getState().discardDraft(draftId);

    const state = useSopCompilerStore.getState();
    expect(state.drafts).toHaveLength(0);
    expect(state.selectedDraftId).toBeNull();
  });
});
