import { create } from "zustand";
import { open, save } from "@tauri-apps/plugin-dialog";

import { primaryRoot, useWorkspaceStore } from "./workspaceStore";
import {
  DEFAULT_STANDARDS_CHAR_BUDGET,
  emptyStandardsDocument,
  selectStandards,
  type StandardsDocument,
  type StandardsSelection,
} from "../lib/standards";
import {
  approveStandard,
  checkStandardsDrift,
  discoverAndMergeStandards,
  exportStandards,
  importStandards,
  loadStandards,
  setStandardStatus,
} from "../lib/standardsRepository";

interface StandardsStore {
  document: StandardsDocument | null;
  workspacePath: string | null;
  loading: boolean;
  error: string | null;
  refresh: () => Promise<void>;
  discover: () => Promise<void>;
  approve: (standardId: string) => Promise<void>;
  reject: (standardId: string) => Promise<void>;
  deprecate: (standardId: string) => Promise<void>;
  drift: () => Promise<void>;
  preview: (taskText: string, fileHints?: string[], budgetChars?: number) => StandardsSelection;
  importFile: () => Promise<void>;
  exportFile: () => Promise<void>;
}

function activeWorkspace(): string | null {
  return primaryRoot(useWorkspaceStore.getState().roots)?.path ?? null;
}

function message(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

let refreshSequence = 0;

export const useStandardsStore = create<StandardsStore>((set, get) => ({
  document: null,
  workspacePath: null,
  loading: false,
  error: null,

  refresh: async () => {
    const sequence = ++refreshSequence;
    const workspacePath = activeWorkspace();
    if (!workspacePath) {
      set({ document: null, workspacePath: null, loading: false, error: null });
      return;
    }
    set({ loading: true, error: null });
    try {
      const document = await loadStandards(workspacePath);
      if (sequence !== refreshSequence) return;
      set({ document, workspacePath, loading: false, error: null });
    } catch (error) {
      if (sequence !== refreshSequence) return;
      set({ document: emptyStandardsDocument(workspacePath), workspacePath, loading: false, error: message(error) });
    }
  },

  discover: async () => {
    const workspacePath = activeWorkspace();
    if (!workspacePath) throw new Error("Open a workspace before discovering standards.");
    set({ loading: true, error: null });
    try {
      const document = await discoverAndMergeStandards(workspacePath);
      set({ document, workspacePath, loading: false });
    } catch (error) {
      set({ loading: false, error: message(error) });
      throw error;
    }
  },

  approve: async (standardId) => {
    const workspacePath = activeWorkspace();
    const document = get().document;
    if (!workspacePath || !document) throw new Error("No standards workspace is loaded.");
    const next = await approveStandard(workspacePath, document, standardId);
    set({ document: next, error: null });
  },

  reject: async (standardId) => {
    const workspacePath = activeWorkspace();
    const document = get().document;
    if (!workspacePath || !document) throw new Error("No standards workspace is loaded.");
    set({ document: await setStandardStatus(workspacePath, document, standardId, "rejected"), error: null });
  },

  deprecate: async (standardId) => {
    const workspacePath = activeWorkspace();
    const document = get().document;
    if (!workspacePath || !document) throw new Error("No standards workspace is loaded.");
    set({ document: await setStandardStatus(workspacePath, document, standardId, "deprecated"), error: null });
  },

  drift: async () => {
    const workspacePath = activeWorkspace();
    const document = get().document;
    if (!workspacePath || !document) throw new Error("No standards workspace is loaded.");
    set({ loading: true, error: null });
    try {
      const next = await checkStandardsDrift(workspacePath, document);
      set({ document: next, loading: false });
    } catch (error) {
      set({ loading: false, error: message(error) });
      throw error;
    }
  },

  preview: (taskText, fileHints = [], budgetChars = DEFAULT_STANDARDS_CHAR_BUDGET) =>
    selectStandards(get().document?.standards ?? [], taskText, fileHints, budgetChars),

  importFile: async () => {
    const workspacePath = activeWorkspace();
    if (!workspacePath) throw new Error("Open a workspace before importing standards.");
    const source = await open({ multiple: false, directory: false, filters: [{ name: "Standards JSON", extensions: ["json"] }] });
    if (typeof source !== "string") return;
    const document = await importStandards(workspacePath, source);
    set({ document, workspacePath, error: null });
  },

  exportFile: async () => {
    const document = get().document;
    if (!document) throw new Error("No standards are loaded.");
    const target = await save({ defaultPath: "little-monkey-standards.json", filters: [{ name: "Standards JSON", extensions: ["json"] }] });
    if (typeof target !== "string") return;
    await exportStandards(document, target);
  },
}));
