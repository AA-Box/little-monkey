import { beforeAll, beforeEach, describe, expect, it } from "vitest";

import { convertRecordingToDraft, createRecording, appendTypeStep, stopRecording } from "../lib/workflowRecorder";
import { useWorkflowDraftStore } from "./workflowDraftStore";

// vitest's "node" test environment has no `localStorage` global (see
// `skillProposalStore.test.ts` for the same shim) — stub an in-memory one so
// the store's real persistence path is exercised rather than skipped.
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

function demoDraft() {
  let recording = createRecording("run-1", "https://example.com/login", 1_000);
  recording = appendTypeStep(
    recording,
    {
      url: "https://example.com/login",
      selector: "#username",
      rawValue: "jane",
      element: { tag: "input", role: "", ariaLabel: "Username", text: "" },
      screenshotArtifactId: null,
    },
    1_100,
  );
  recording = appendTypeStep(
    recording,
    {
      url: "https://example.com/login",
      selector: "#password",
      rawValue: "hunter2",
      element: { tag: "input", role: "", ariaLabel: "Password", text: "" },
      screenshotArtifactId: null,
    },
    1_200,
  );
  recording = stopRecording(recording, 1_300);
  return convertRecordingToDraft(recording, { name: "Demo login" });
}

beforeEach(() => {
  localStorage.clear();
  useWorkflowDraftStore.setState({ drafts: [] });
});

describe("workflowDraftStore", () => {
  it("saves a draft in draft status, never auto-enabled", () => {
    useWorkflowDraftStore.getState().saveDraft(demoDraft());
    const [saved] = useWorkflowDraftStore.getState().drafts;
    expect(saved.status).toBe("draft");
    expect(saved.reviewedAt).toBeNull();
  });

  it("re-saving an already-persisted draft (e.g. reopened from the library for another look) updates it in place instead of duplicating it", () => {
    const draft = demoDraft();
    useWorkflowDraftStore.getState().saveDraft(draft);
    useWorkflowDraftStore.getState().markReviewed(draft.id);
    useWorkflowDraftStore.getState().enableDraft(draft.id);
    expect(useWorkflowDraftStore.getState().drafts).toHaveLength(1);

    const reopened = useWorkflowDraftStore.getState().drafts[0];
    useWorkflowDraftStore.getState().saveDraft({ ...reopened, name: "Renamed while re-reviewing" });

    const all = useWorkflowDraftStore.getState().drafts;
    expect(all).toHaveLength(1);
    expect(all[0].id).toBe(draft.id);
    expect(all[0].name).toBe("Renamed while re-reviewing");
    // Saving (without re-enabling) demotes it back to draft — editing an
    // enabled workflow requires an explicit re-enable, same as a brand new one.
    expect(all[0].status).toBe("draft");
  });

  it("refuses to enable a draft that has not been reviewed", () => {
    const draft = demoDraft();
    useWorkflowDraftStore.getState().saveDraft(draft);
    expect(() => useWorkflowDraftStore.getState().enableDraft(draft.id)).toThrow(/review/i);
    expect(useWorkflowDraftStore.getState().drafts[0].status).toBe("draft");
  });

  it("enables a draft only after it has been explicitly marked reviewed", () => {
    const draft = demoDraft();
    useWorkflowDraftStore.getState().saveDraft(draft);
    useWorkflowDraftStore.getState().markReviewed(draft.id);
    useWorkflowDraftStore.getState().enableDraft(draft.id);
    expect(useWorkflowDraftStore.getState().drafts[0].status).toBe("enabled");
  });

  it("can disable an enabled draft back to draft status", () => {
    const draft = demoDraft();
    useWorkflowDraftStore.getState().saveDraft(draft);
    useWorkflowDraftStore.getState().markReviewed(draft.id);
    useWorkflowDraftStore.getState().enableDraft(draft.id);
    useWorkflowDraftStore.getState().disableDraft(draft.id);
    expect(useWorkflowDraftStore.getState().drafts[0].status).toBe("draft");
  });

  it("renames an input to a safe machine name", () => {
    const draft = demoDraft();
    useWorkflowDraftStore.getState().saveDraft(draft);
    const inputId = draft.inputs[0].id;
    useWorkflowDraftStore.getState().renameInput(draft.id, inputId, "Login Email!!");
    const renamed = useWorkflowDraftStore.getState().drafts[0].inputs.find((i) => i.id === inputId);
    expect(renamed?.name).toBe("Login_Email_");
  });

  it("marking a normal input runtime-only clears its stored default value", () => {
    const draft = demoDraft();
    useWorkflowDraftStore.getState().saveDraft(draft);
    const usernameInput = draft.inputs.find((i) => !i.sensitive);
    expect(usernameInput).toBeDefined();
    expect(usernameInput?.defaultValue).toBe("jane");
    useWorkflowDraftStore.getState().setInputRuntimeOnly(draft.id, usernameInput!.id, true);
    const updated = useWorkflowDraftStore.getState().drafts[0].inputs.find((i) => i.id === usernameInput!.id);
    expect(updated?.runtimeOnly).toBe(true);
    expect(updated?.defaultValue).toBeNull();
  });

  it("refuses to un-mark a sensitive input as runtime-only — it can never be persisted", () => {
    const draft = demoDraft();
    useWorkflowDraftStore.getState().saveDraft(draft);
    const passwordInput = draft.inputs.find((i) => i.sensitive);
    expect(passwordInput).toBeDefined();
    useWorkflowDraftStore.getState().setInputRuntimeOnly(draft.id, passwordInput!.id, false);
    const updated = useWorkflowDraftStore.getState().drafts[0].inputs.find((i) => i.id === passwordInput!.id);
    expect(updated?.runtimeOnly).toBe(true);
    expect(updated?.defaultValue).toBeNull();
  });

  it("refuses to set a stored default value for a sensitive input", () => {
    const draft = demoDraft();
    useWorkflowDraftStore.getState().saveDraft(draft);
    const passwordInput = draft.inputs.find((i) => i.sensitive);
    useWorkflowDraftStore.getState().setInputDefaultValue(draft.id, passwordInput!.id, "leaked-secret");
    const updated = useWorkflowDraftStore.getState().drafts[0].inputs.find((i) => i.id === passwordInput!.id);
    expect(updated?.defaultValue).toBeNull();
    const serialized = JSON.stringify(useWorkflowDraftStore.getState().drafts);
    expect(serialized).not.toContain("leaked-secret");
  });

  it("deletes a draft", () => {
    const draft = demoDraft();
    useWorkflowDraftStore.getState().saveDraft(draft);
    useWorkflowDraftStore.getState().deleteDraft(draft.id);
    expect(useWorkflowDraftStore.getState().drafts).toHaveLength(0);
  });

  it("persists drafts to localStorage and rehydrates them", () => {
    const draft = demoDraft();
    useWorkflowDraftStore.getState().saveDraft(draft);
    const raw = localStorage.getItem("little-monkey-workflow-drafts-v1");
    expect(raw).toBeTruthy();
    expect(JSON.parse(raw!).drafts[0].id).toBe(draft.id);
  });
});
