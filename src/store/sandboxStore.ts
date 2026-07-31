import { create } from "zustand";

import * as api from "../lib/sandbox";
import type {
  SandboxDiffEntry,
  SandboxPromotePreview,
  SandboxRunListEntry,
  SandboxRunSummary,
} from "../lib/sandbox";
import { readDurableArtifact } from "../lib/durableArtifacts";
import { errorMessage } from "../lib/errors";

function errorText(error: unknown): string {
  return errorMessage(error);
}

function decodeBase64Text(base64: string): string {
  const binary = atob(base64);
  const bytes = new Uint8Array(binary.length);
  for (let i = 0; i < binary.length; i += 1) bytes[i] = binary.charCodeAt(i);
  return new TextDecoder().decode(bytes);
}

interface SandboxState {
  runs: SandboxRunListEntry[];
  activeRunId: string | null;
  activeSummary: SandboxRunSummary | null;
  stdoutText: string | null;
  stderrText: string | null;
  diff: SandboxDiffEntry[];
  selectedFiles: string[];
  preview: SandboxPromotePreview | null;
  busy: Record<string, boolean>;
  error: string | null;
  notice: string | null;

  clearMessages: () => void;
  refresh: () => Promise<void>;
  run: (command: string, options?: { timeoutMs?: number; allowNetwork?: boolean; approvedEnv?: string[] }) => Promise<SandboxRunSummary>;
  loadLogs: (summary: SandboxRunSummary) => Promise<void>;
  loadDiff: (runId: string) => Promise<void>;
  toggleFile: (path: string) => void;
  setSelectedFiles: (paths: string[]) => void;
  preparePromote: (runId: string, files: string[]) => Promise<SandboxPromotePreview>;
  cancelPromotePreview: () => void;
  executePromote: (confirmationPhrase: string) => Promise<void>;
  discard: (runId: string, reason?: string) => Promise<void>;
}

export const useSandboxStore = create<SandboxState>((set, get) => {
  const perform = async <T>(key: string, task: () => Promise<T>): Promise<T> => {
    set((state) => ({ busy: { ...state.busy, [key]: true }, error: null, notice: null }));
    try {
      return await task();
    } catch (error) {
      set({ error: errorText(error) });
      throw error;
    } finally {
      set((state) => ({ busy: { ...state.busy, [key]: false } }));
    }
  };

  return {
    runs: [],
    activeRunId: null,
    activeSummary: null,
    stdoutText: null,
    stderrText: null,
    diff: [],
    selectedFiles: [],
    preview: null,
    busy: {},
    error: null,
    notice: null,

    clearMessages: () => set({ error: null, notice: null }),

    refresh: () => perform("refresh", async () => {
      set({ runs: await api.listSandboxRuns() });
    }),

    run: (command, options) => perform("run", async () => {
      const summary = await api.runInSandbox(command, options);
      set({
        activeRunId: summary.runId,
        activeSummary: summary,
        diff: [],
        selectedFiles: [],
        preview: null,
        stdoutText: null,
        stderrText: null,
      });
      await get().refresh();
      return summary;
    }),

    loadLogs: (summary) => perform("logs", async () => {
      const [stdout, stderr] = await Promise.all([
        readDurableArtifact(summary.stdoutArtifactId),
        readDurableArtifact(summary.stderrArtifactId),
      ]);
      set({
        stdoutText: decodeBase64Text(stdout.contentBase64),
        stderrText: decodeBase64Text(stderr.contentBase64),
      });
    }),

    loadDiff: (runId) => perform("diff", async () => {
      const diff = await api.sandboxDiff(runId);
      set({ diff, selectedFiles: diff.map((entry) => entry.path) });
    }),

    toggleFile: (path) => set((state) => ({
      selectedFiles: state.selectedFiles.includes(path)
        ? state.selectedFiles.filter((item) => item !== path)
        : [...state.selectedFiles, path],
    })),

    setSelectedFiles: (paths) => set({ selectedFiles: paths }),

    preparePromote: (runId, files) => perform("preparePromote", async () => {
      if (files.length === 0) throw new Error("Select at least one file to promote.");
      const preview = await api.prepareSandboxPromote(runId, files);
      set({ preview });
      return preview;
    }),

    cancelPromotePreview: () => set({ preview: null }),

    executePromote: (confirmationPhrase) => perform("executePromote", async () => {
      const { preview } = get();
      if (!preview) throw new Error("Prepare an exact promote preview first.");
      if (Date.now() > preview.expiresAtMs) throw new Error("This confirmation preview expired. Prepare a new one.");
      const result = await api.executeSandboxPromote(preview.runId, preview.digest, confirmationPhrase);
      set({
        preview: null,
        notice: `Promoted ${result.promotedFiles.length} file(s) to the workspace.`,
      });
      await Promise.all([get().refresh(), get().loadDiff(result.runId)]);
    }),

    discard: (runId, reason) => perform("discard", async () => {
      await api.discardSandboxRun(runId, reason);
      set((state) => ({
        activeRunId: state.activeRunId === runId ? null : state.activeRunId,
        activeSummary: state.activeRunId === runId ? null : state.activeSummary,
        preview: state.preview?.runId === runId ? null : state.preview,
        diff: state.activeRunId === runId ? [] : state.diff,
      }));
      await get().refresh();
    }),
  };
});
