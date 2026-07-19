import { create } from "zustand";

import * as api from "../lib/securityAutofix";
import type { SecurityFinding, SecurityFixProposal } from "../lib/securityAutofix";

export type ApplyStatus = "idle" | "creating_branch" | "running" | "done" | "error" | "cancelled";

export interface ApplyState {
  status: ApplyStatus;
  branch: string | null;
  workspaceLabel: string | null;
  durableRunId: string | null;
  summary: string | null;
  error: string | null;
  activity: string | null;
}

function idleApplyState(): ApplyState {
  return {
    status: "idle",
    branch: null,
    workspaceLabel: null,
    durableRunId: null,
    summary: null,
    error: null,
    activity: null,
  };
}

function errorText(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

/** In-flight "apply in branch" cancellation handles, keyed by finding id —
 * deliberately NOT part of the zustand state, same reasoning as
 * `issueToPrStore.ts`'s own `controllers` map: an `AbortController` isn't a
 * value React/zustand needs to react to. */
const controllers = new Map<string, AbortController>();

/** Test-only: clears in-flight controllers — this module's `controllers` map
 * is process-lifetime by design, which otherwise leaks across tests. */
export function __resetSecurityAutofixControllersForTests(): void {
  controllers.clear();
}

interface SecurityAutofixState {
  findings: SecurityFinding[];
  proposals: Record<string, SecurityFixProposal>;
  proposing: Record<string, boolean>;
  applyState: Record<string, ApplyState>;
  repositorySlug: string;
  scanning: boolean;
  scanError: string | null;
  error: string | null;

  setRepositorySlug: (slug: string) => void;
  scan: () => Promise<void>;
  proposeFix: (findingId: string) => Promise<void>;
  applyFix: (findingId: string) => Promise<void>;
  cancelApply: (findingId: string) => void;
  clearError: () => void;
}

export const useSecurityAutofixStore = create<SecurityAutofixState>((set, get) => {
  const perform = async <T>(task: () => Promise<T>): Promise<T> => {
    set({ error: null });
    try {
      return await task();
    } catch (error) {
      set({ error: errorText(error) });
      throw error;
    }
  };

  return {
    findings: [],
    proposals: {},
    proposing: {},
    applyState: {},
    repositorySlug: "",
    scanning: false,
    scanError: null,
    error: null,

    setRepositorySlug: (slug) => set({ repositorySlug: slug }),

    clearError: () => set({ error: null }),

    scan: () =>
      perform(async () => {
        set({ scanning: true, scanError: null });
        try {
          const { findings, auditError } = await api.runSecurityScan();
          set({ findings, scanError: auditError });
        } finally {
          set({ scanning: false });
        }
      }),

    proposeFix: (findingId) =>
      perform(async () => {
        const finding = get().findings.find((candidate) => candidate.id === findingId);
        if (!finding) throw new Error(`Unknown finding "${findingId}".`);
        set((state) => ({ proposing: { ...state.proposing, [findingId]: true } }));
        try {
          const callModel = await api.defaultProposeFixCallModel(`security-autofix-propose-${findingId}`);
          const proposal = await api.proposeFixForFinding(finding, callModel);
          set((state) => ({ proposals: { ...state.proposals, [findingId]: proposal } }));
        } finally {
          set((state) => ({ proposing: { ...state.proposing, [findingId]: false } }));
        }
      }),

    applyFix: (findingId) =>
      perform(async () => {
        const finding = get().findings.find((candidate) => candidate.id === findingId);
        if (!finding) throw new Error(`Unknown finding "${findingId}".`);
        const proposal = get().proposals[findingId];
        if (!proposal) throw new Error("Propose a fix for this finding first.");
        const repositorySlug = get().repositorySlug.trim();
        if (!repositorySlug) throw new Error("Enter the GitHub repository (owner/repository) first.");

        const setApply = (patch: Partial<ApplyState>) =>
          set((state) => ({
            applyState: {
              ...state.applyState,
              [findingId]: { ...(state.applyState[findingId] ?? idleApplyState()), ...patch },
            },
          }));

        setApply({ status: "creating_branch", error: null, summary: null });

        let branchInfo: Awaited<ReturnType<typeof api.createIsolatedBranchForFinding>>;
        try {
          branchInfo = await api.createIsolatedBranchForFinding(finding, repositorySlug);
        } catch (error) {
          setApply({ status: "error", error: errorText(error) });
          throw error;
        }

        setApply({
          status: "running",
          branch: branchInfo.branch,
          workspaceLabel: branchInfo.workspaceLabel,
        });

        const controller = new AbortController();
        controllers.set(findingId, controller);
        const runId = `security-autofix-${findingId}-${Date.now()}`;

        try {
          const result = await api.runSecurityAutofixAgent({
            runId,
            finding,
            proposal,
            branch: branchInfo.branch,
            workspaceLabel: branchInfo.workspaceLabel,
            signal: controller.signal,
            onToolActivity: (label) => setApply({ activity: label }),
          });

          if (result.outcome === "completed") {
            setApply({ status: "done", summary: result.summary, durableRunId: result.durableRunId, activity: null });
          } else if (result.outcome === "cancelled") {
            setApply({ status: "cancelled", summary: result.summary, activity: null });
          } else {
            setApply({ status: "error", error: result.summary, durableRunId: result.durableRunId, activity: null });
          }
        } catch (error) {
          setApply({ status: "error", error: errorText(error), activity: null });
        } finally {
          controllers.delete(findingId);
        }
      }),

    cancelApply: (findingId) => {
      controllers.get(findingId)?.abort();
    },
  };
});
