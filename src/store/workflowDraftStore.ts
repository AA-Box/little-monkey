import { create } from "zustand";

import { assertNoStoredSecrets, type DraftWorkflow, type DraftWorkflowStatus } from "../lib/workflowRecorder";

const STORAGE_KEY = "little-monkey-workflow-drafts-v1";

interface WorkflowDraftStore {
  drafts: DraftWorkflow[];
  /** Saves a freshly converted draft (always `status: "draft"`, never
   * auto-enabled — see `convertRecordingToDraft`). */
  saveDraft: (draft: DraftWorkflow) => void;
  renameDraft: (id: string, name: string) => void;
  renameInput: (id: string, inputId: string, name: string) => void;
  /** Explicitly marks (or unmarks) an input as a runtime-only value that is
   * prompted fresh on every replay and never persisted. Marking an input
   * runtime-only clears any stored default value immediately. Sensitive
   * (credential-like) inputs cannot be un-marked — they are always
   * runtime-only. */
  setInputRuntimeOnly: (id: string, inputId: string, runtimeOnly: boolean) => void;
  setInputDefaultValue: (id: string, inputId: string, value: string) => void;
  /** The only path that can ever move a draft to `"enabled"`. Requires the
   * caller to have reviewed the draft (`markReviewed`) first — this is the
   * "require user review before enabling replay" acceptance gate. */
  enableDraft: (id: string) => void;
  disableDraft: (id: string) => void;
  markReviewed: (id: string) => void;
  archiveDraft: (id: string) => void;
  deleteDraft: (id: string) => void;
}

function persist(drafts: DraftWorkflow[]): void {
  try {
    localStorage.setItem(STORAGE_KEY, JSON.stringify({ version: 1, drafts }));
  } catch {
    // Drafts remain live in memory for this session even if the optional
    // localStorage cache is unavailable or full.
  }
}

function isDraftWorkflow(value: unknown): value is DraftWorkflow {
  const item = value as Partial<DraftWorkflow> | null;
  return Boolean(
    item &&
    typeof item.id === "string" &&
    typeof item.name === "string" &&
    typeof item.status === "string" &&
    Array.isArray(item.inputs) &&
    Array.isArray(item.steps) &&
    typeof item.sourceRecordingId === "string" &&
    typeof item.originUrl === "string",
  );
}

function hydrate(): DraftWorkflow[] {
  try {
    const raw = JSON.parse(localStorage.getItem(STORAGE_KEY) ?? "null") as { version?: unknown; drafts?: unknown } | null;
    if (raw?.version !== 1 || !Array.isArray(raw.drafts)) return [];
    return raw.drafts.filter(isDraftWorkflow);
  } catch {
    return [];
  }
}

function updateDraft(drafts: DraftWorkflow[], id: string, update: (draft: DraftWorkflow) => DraftWorkflow): DraftWorkflow[] {
  return drafts.map((draft) => (draft.id === id ? update(draft) : draft));
}

export const useWorkflowDraftStore = create<WorkflowDraftStore>((set, get) => ({
  drafts: hydrate(),

  saveDraft: (draft) => {
    assertNoStoredSecrets(draft);
    const incoming = { ...draft, status: "draft" as const };
    // Upsert by id: `WorkflowDraftReview` reuses this same action both to
    // save a brand-new draft (fresh id from `convertRecordingToDraft`, not
    // yet in the list — prepended) and to re-save an already-persisted
    // draft reopened for another look (its id is already in the list —
    // replaced in place). Always prepending here would instead leave two
    // entries sharing one id the moment a saved draft is edited and saved
    // again.
    const existingIndex = get().drafts.findIndex((entry) => entry.id === incoming.id);
    const drafts = existingIndex >= 0
      ? get().drafts.map((entry, index) => (index === existingIndex ? incoming : entry))
      : [incoming, ...get().drafts];
    persist(drafts);
    set({ drafts });
  },

  renameDraft: (id, name) => {
    const trimmed = name.trim();
    if (!trimmed) return;
    const drafts = updateDraft(get().drafts, id, (draft) => ({ ...draft, name: trimmed, updatedAt: Date.now() }));
    persist(drafts);
    set({ drafts });
  },

  renameInput: (id, inputId, name) => {
    const trimmed = name.trim().replace(/[^a-zA-Z0-9_]+/g, "_").slice(0, 60);
    if (!trimmed) return;
    const drafts = updateDraft(get().drafts, id, (draft) => ({
      ...draft,
      updatedAt: Date.now(),
      inputs: draft.inputs.map((input) => (input.id === inputId ? { ...input, name: trimmed } : input)),
    }));
    persist(drafts);
    set({ drafts });
  },

  setInputRuntimeOnly: (id, inputId, runtimeOnly) => {
    const drafts = updateDraft(get().drafts, id, (draft) => ({
      ...draft,
      updatedAt: Date.now(),
      inputs: draft.inputs.map((input) => {
        if (input.id !== inputId) return input;
        // Sensitive inputs can never be downgraded out of runtime-only —
        // that would resurrect the exact leak `redactTypedValue` exists to
        // prevent.
        if (input.sensitive && !runtimeOnly) return input;
        return { ...input, runtimeOnly, defaultValue: runtimeOnly ? null : input.defaultValue };
      }),
    }));
    persist(drafts);
    set({ drafts });
  },

  setInputDefaultValue: (id, inputId, value) => {
    const drafts = updateDraft(get().drafts, id, (draft) => ({
      ...draft,
      updatedAt: Date.now(),
      inputs: draft.inputs.map((input) => {
        if (input.id !== inputId) return input;
        // A sensitive or runtime-only input must never get a stored value —
        // ignore the write rather than silently persisting a secret.
        if (input.sensitive || input.runtimeOnly) return input;
        return { ...input, defaultValue: value };
      }),
    }));
    persist(drafts);
    set({ drafts });
  },

  markReviewed: (id) => {
    const drafts = updateDraft(get().drafts, id, (draft) => ({ ...draft, reviewedAt: Date.now(), updatedAt: Date.now() }));
    persist(drafts);
    set({ drafts });
  },

  enableDraft: (id) => {
    const draft = get().drafts.find((entry) => entry.id === id);
    if (!draft) throw new Error("Unknown draft workflow.");
    if (!draft.reviewedAt) {
      throw new Error("Review this workflow before enabling replay.");
    }
    assertNoStoredSecrets(draft);
    const status: DraftWorkflowStatus = "enabled";
    const drafts = updateDraft(get().drafts, id, (entry) => ({ ...entry, status, updatedAt: Date.now() }));
    persist(drafts);
    set({ drafts });
  },

  disableDraft: (id) => {
    const drafts = updateDraft(get().drafts, id, (draft) =>
      draft.status === "enabled" ? { ...draft, status: "draft", updatedAt: Date.now() } : draft,
    );
    persist(drafts);
    set({ drafts });
  },

  archiveDraft: (id) => {
    const drafts = updateDraft(get().drafts, id, (draft) => ({ ...draft, status: "archived", updatedAt: Date.now() }));
    persist(drafts);
    set({ drafts });
  },

  deleteDraft: (id) => {
    const drafts = get().drafts.filter((draft) => draft.id !== id);
    persist(drafts);
    set({ drafts });
  },
}));

export default useWorkflowDraftStore;
