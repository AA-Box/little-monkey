import { create } from 'zustand';

import * as api from '../lib/productionDebugging';
import type {
  DebugWorktree,
  ProductionDebugReport,
  ProductionEvidence,
  ProductionEvidenceKind,
} from '../lib/productionDebugging';
import { errorMessage } from "../lib/errors";
import { hydrateState, persistState } from "../lib/persistedState";

const NO_DIFF_RISK = 'No worktree diff was captured; inspect the owned branch before using it.';
const VERIFICATION_RISK_PREFIX = 'Explicit verification finished with status:';

export const PRODUCTION_DEBUG_STORAGE_KEY = 'little-monkey-production-debug-cases-v1';
const STORAGE_VERSION = 1;

export type ProductionDebugCaseStatus =
  | 'draft'
  | 'diagnosing'
  | 'diagnosed'
  | 'creating_worktree'
  | 'fixing'
  | 'fix_prepared'
  | 'failed'
  | 'cancelled';

export interface ProductionDebugCase {
  id: string;
  title: string;
  description: string;
  repositorySlug: string;
  reproductionCommand: string;
  verificationCommand: string;
  evidence: ProductionEvidence[];
  status: ProductionDebugCaseStatus;
  report: ProductionDebugReport | null;
  worktree: DebugWorktree | null;
  fixSummary: string | null;
  error: string | null;
  createdAtMs: number;
  updatedAtMs: number;
}

export interface CreateProductionDebugCaseInput {
  title: string;
  description?: string;
  repositorySlug?: string;
}

type EditableCaseFields = Pick<
  ProductionDebugCase,
  'title' | 'description' | 'repositorySlug' | 'reproductionCommand' | 'verificationCommand'
>;

interface ProductionDebuggingState {
  cases: ProductionDebugCase[];
  selectedCaseId: string | null;
  activityByCase: Record<string, string>;

  init: () => void;
  selectCase: (caseId: string | null) => void;
  createCase: (input: CreateProductionDebugCaseInput) => ProductionDebugCase;
  updateCase: (caseId: string, patch: Partial<EditableCaseFields>) => void;
  addPastedEvidence: (caseId: string, kind: ProductionEvidenceKind, label: string, content: string) => void;
  addWorkspaceEvidence: (caseId: string, kind: ProductionEvidenceKind, path: string) => void;
  attachEvidence: (caseId: string, evidence: ProductionEvidence) => void;
  removeEvidence: (caseId: string, evidenceId: string) => void;
  diagnose: (caseId: string) => Promise<void>;
  prepareFix: (caseId: string) => Promise<void>;
  cancel: (caseId: string) => void;
  deleteCase: (caseId: string) => void;
}

function errorText(error: unknown): string {
  return errorMessage(error);
}

function persist(cases: readonly ProductionDebugCase[]): void {
  persistState(PRODUCTION_DEBUG_STORAGE_KEY, STORAGE_VERSION, { cases });
}

const STATUSES: ReadonlySet<ProductionDebugCaseStatus> = new Set([
  'draft',
  'diagnosing',
  'diagnosed',
  'creating_worktree',
  'fixing',
  'fix_prepared',
  'failed',
  'cancelled',
]);

const EVIDENCE_KINDS: ReadonlySet<ProductionEvidenceKind> = new Set([
  'log', 'trace', 'error', 'release', 'commit', 'deploy', 'code', 'terminal', 'browser', 'command',
]);

function hydrateEvidence(value: unknown): ProductionEvidence[] {
  if (!Array.isArray(value)) return [];
  const evidence = value.flatMap((candidate): ProductionEvidence[] => {
    if (!candidate || typeof candidate !== 'object') return [];
    const item = candidate as Partial<ProductionEvidence>;
    if (
      typeof item.id !== 'string'
      || !EVIDENCE_KINDS.has(item.kind as ProductionEvidenceKind)
      || (item.origin !== 'paste'
        && item.origin !== 'workspace-file'
        && item.origin !== 'terminal'
        && item.origin !== 'browser'
        && item.origin !== 'command')
      || typeof item.content !== 'string'
    ) return [];
    const hydrated = api.createProductionEvidence({
      id: item.id,
      kind: item.kind as ProductionEvidenceKind,
      origin: item.origin,
      label: typeof item.label === 'string' ? item.label : item.kind as string,
      sourceUri: typeof item.sourceUri === 'string' ? item.sourceUri : `${item.origin}://${item.id}`,
      content: item.content,
      collectedAtMs: typeof item.collectedAtMs === 'number' ? item.collectedAtMs : Date.now(),
    });
    return [{ ...hydrated, truncated: item.truncated === true || hydrated.truncated }];
  });
  return api.boundProductionEvidence(evidence);
}

function hydrate(): ProductionDebugCase[] {
  const raw = hydrateState(PRODUCTION_DEBUG_STORAGE_KEY, STORAGE_VERSION);
  if (!raw || !Array.isArray(raw.cases)) return [];
  return raw.cases.flatMap((candidate): ProductionDebugCase[] => {
    if (!candidate || typeof candidate !== 'object') return [];
    const item = candidate as Partial<ProductionDebugCase>;
    if (
      typeof item.id !== 'string'
      || typeof item.title !== 'string'
      || typeof item.createdAtMs !== 'number'
      || typeof item.updatedAtMs !== 'number'
      || !STATUSES.has(item.status as ProductionDebugCaseStatus)
    ) return [];
    const wasRunning = item.status === 'diagnosing' || item.status === 'creating_worktree' || item.status === 'fixing';
    return [{
      id: item.id,
      title: item.title.slice(0, 300),
      description: typeof item.description === 'string' ? item.description.slice(0, 8_000) : '',
      repositorySlug: typeof item.repositorySlug === 'string' ? item.repositorySlug.slice(0, 300) : '',
      reproductionCommand: typeof item.reproductionCommand === 'string' ? item.reproductionCommand.slice(0, 8_000) : '',
      verificationCommand: typeof item.verificationCommand === 'string' ? item.verificationCommand.slice(0, 8_000) : '',
      evidence: hydrateEvidence(item.evidence),
      status: wasRunning ? (item.report ? 'diagnosed' : 'draft') : item.status as ProductionDebugCaseStatus,
      report: item.report && typeof item.report === 'object' ? item.report as ProductionDebugReport : null,
      worktree: item.worktree && typeof item.worktree === 'object' ? item.worktree as DebugWorktree : null,
      fixSummary: typeof item.fixSummary === 'string' ? item.fixSummary : null,
      error: wasRunning
        ? 'The previous local run was interrupted when the app closed. Review the saved evidence and retry.'
        : typeof item.error === 'string' ? item.error : null,
      createdAtMs: item.createdAtMs,
      updatedAtMs: item.updatedAtMs,
    }];
  });
}

function upsert(cases: ProductionDebugCase[], next: ProductionDebugCase): ProductionDebugCase[] {
  const index = cases.findIndex((item) => item.id === next.id);
  if (index < 0) return [next, ...cases];
  const copy = [...cases];
  copy[index] = next;
  return copy;
}

const controllers = new Map<string, AbortController>();

export function __resetProductionDebugControllersForTests(): void {
  controllers.clear();
}

export const useProductionDebuggingStore = create<ProductionDebuggingState>((set, get) => {
  const update = (caseId: string, patch: Partial<ProductionDebugCase>): ProductionDebugCase | null => {
    let updated: ProductionDebugCase | null = null;
    set((state) => {
      const current = state.cases.find((item) => item.id === caseId);
      if (!current) return state;
      updated = { ...current, ...patch, updatedAtMs: Date.now() };
      const cases = upsert(state.cases, updated);
      persist(cases);
      return { cases };
    });
    return updated;
  };

  const setActivity = (caseId: string, activity: string | null) => {
    set((state) => {
      const activityByCase = { ...state.activityByCase };
      if (activity) activityByCase[caseId] = activity;
      else delete activityByCase[caseId];
      return { activityByCase };
    });
  };

  return {
    cases: hydrate(),
    selectedCaseId: null,
    activityByCase: {},

    init: () => {
      const cases = hydrate();
      persist(cases);
      set((state) => ({
        cases,
        selectedCaseId: state.selectedCaseId && cases.some((item) => item.id === state.selectedCaseId)
          ? state.selectedCaseId
          : cases[0]?.id ?? null,
      }));
    },

    selectCase: (selectedCaseId) => set({ selectedCaseId }),

    createCase: (input) => {
      const title = input.title.trim();
      if (!title) throw new Error('Enter a production issue title.');
      const now = Date.now();
      const debugCase: ProductionDebugCase = {
        id: crypto.randomUUID(),
        title: title.slice(0, 300),
        description: input.description?.trim().slice(0, 8_000) ?? '',
        repositorySlug: input.repositorySlug?.trim().slice(0, 300) ?? '',
        reproductionCommand: '',
        verificationCommand: '',
        evidence: [],
        status: 'draft',
        report: null,
        worktree: null,
        fixSummary: null,
        error: null,
        createdAtMs: now,
        updatedAtMs: now,
      };
      set((state) => {
        const cases = upsert(state.cases, debugCase);
        persist(cases);
        return { cases, selectedCaseId: debugCase.id };
      });
      return debugCase;
    },

    updateCase: (caseId, patch) => {
      const current = get().cases.find((item) => item.id === caseId);
      if (!current) return;
      const diagnosisChanged = patch.title !== undefined
        || patch.description !== undefined
        || patch.repositorySlug !== undefined
        || patch.reproductionCommand !== undefined;
      const reproductionChanged = patch.reproductionCommand !== undefined;
      const verificationChanged = patch.verificationCommand !== undefined;
      update(caseId, {
        ...patch,
        ...(reproductionChanged ? {
          evidence: current.evidence.filter((item) => !(item.origin === 'command' && item.label === 'Reproduction command')),
        } : {}),
        ...(diagnosisChanged ? {
          report: null,
          worktree: null,
          status: 'draft' as const,
          error: null,
          fixSummary: null,
        } : verificationChanged && current.report ? {
          report: {
            ...current.report,
            verification: api.notRunCommand(patch.verificationCommand),
            verificationDurableRunId: null,
          },
          status: 'diagnosed' as const,
          error: null,
        } : {}),
      });
    },

    addPastedEvidence: (caseId, kind, label, content) => {
      if (!content.trim()) throw new Error('Paste evidence content first.');
      const evidence = api.createProductionEvidence({
        kind,
        origin: 'paste',
        label: label.trim() || `${kind} evidence`,
        sourceUri: `paste://${caseId}/${Date.now()}`,
        content,
      });
      const current = get().cases.find((item) => item.id === caseId);
      if (!current) throw new Error('Unknown production debugging case.');
      update(caseId, {
        evidence: api.boundProductionEvidence([
          evidence,
          ...current.evidence.filter((item) => item.id !== evidence.id),
        ]),
        report: null,
        worktree: null,
        status: 'draft',
        fixSummary: null,
        error: null,
      });
    },

    addWorkspaceEvidence: (caseId, kind, path) => {
      const current = get().cases.find((item) => item.id === caseId);
      if (!current) throw new Error('Unknown production debugging case.');
      const evidence = api.createWorkspaceFileEvidence(kind, path);
      update(caseId, {
        evidence: api.boundProductionEvidence([evidence, ...current.evidence]),
        report: null,
        worktree: null,
        status: 'draft',
        fixSummary: null,
        error: null,
      });
    },

    attachEvidence: (caseId, evidence) => {
      const current = get().cases.find((item) => item.id === caseId);
      if (!current) throw new Error('Unknown production debugging case.');
      update(caseId, {
        evidence: api.boundProductionEvidence([
          evidence,
          ...current.evidence.filter((item) => item.id !== evidence.id),
        ]),
        report: null,
        worktree: null,
        status: 'draft',
        fixSummary: null,
        error: null,
      });
    },

    removeEvidence: (caseId, evidenceId) => {
      const current = get().cases.find((item) => item.id === caseId);
      if (!current) return;
      update(caseId, {
        evidence: current.evidence.filter((item) => item.id !== evidenceId),
        report: null,
        worktree: null,
        status: 'draft',
        fixSummary: null,
        error: null,
      });
    },

    diagnose: async (caseId) => {
      const debugCase = get().cases.find((item) => item.id === caseId);
      if (!debugCase) throw new Error('Unknown production debugging case.');
      const diagnosticEvidence = debugCase.evidence.filter(
        (item) => !(item.origin === 'command' && item.label === 'Reproduction command'),
      );
      if (diagnosticEvidence.length === 0 && !debugCase.reproductionCommand.trim()) {
        throw new Error('Attach at least one evidence item or enter a reproduction command.');
      }
      const controller = new AbortController();
      controllers.set(caseId, controller);
      update(caseId, { status: 'diagnosing', error: null, report: null, fixSummary: null });

      try {
        let evidence = diagnosticEvidence;
        if (evidence.length !== debugCase.evidence.length) update(caseId, { evidence });
        let reproduction = api.notRunCommand(debugCase.reproductionCommand);
        if (debugCase.reproductionCommand.trim()) {
          const command = await api.executeExplicitDebugCommand({
            caseId,
            caseTitle: debugCase.title,
            purpose: 'reproduction',
            command: debugCase.reproductionCommand,
            signal: controller.signal,
            onToolActivity: (activity) => setActivity(caseId, activity),
          });
          reproduction = command.execution;
          evidence = api.boundProductionEvidence([command.evidence, ...evidence]);
          update(caseId, { evidence });
          if (command.execution.status === 'cancelled' || controller.signal.aborted) {
            update(caseId, { status: 'cancelled', error: null });
            return;
          }
        }

        const result = await api.diagnoseProductionIssue({
          caseId,
          title: debugCase.title,
          description: debugCase.description,
          evidence,
          reproduction,
          signal: controller.signal,
          onToolActivity: (activity) => setActivity(caseId, activity),
        });
        if (result.outcome === 'completed' && result.report) {
          update(caseId, { status: 'diagnosed', report: result.report, error: null });
        } else if (result.outcome === 'cancelled') {
          update(caseId, { status: 'cancelled', error: null });
        } else {
          update(caseId, { status: 'failed', error: result.summary });
        }
      } catch (error) {
        update(caseId, {
          status: controller.signal.aborted ? 'cancelled' : 'failed',
          error: controller.signal.aborted ? null : errorText(error),
        });
      } finally {
        controllers.delete(caseId);
        setActivity(caseId, null);
      }
    },

    prepareFix: async (caseId) => {
      const debugCase = get().cases.find((item) => item.id === caseId);
      if (!debugCase) throw new Error('Unknown production debugging case.');
      if (!debugCase.report) throw new Error('Run the production diagnosis before preparing a fix.');
      if (!debugCase.repositorySlug.trim()) throw new Error('Enter the repository as owner/name first.');

      const controller = new AbortController();
      controllers.set(caseId, controller);
      try {
        let worktree = debugCase.worktree;
        if (!worktree) {
          update(caseId, { status: 'creating_worktree', error: null });
          worktree = await api.createProductionDebugWorktree({
            caseId,
            title: debugCase.title,
            repositorySlug: debugCase.repositorySlug,
          });
          update(caseId, { worktree });
        }
        if (controller.signal.aborted) {
          update(caseId, { status: 'cancelled', error: null });
          return;
        }

        update(caseId, { status: 'fixing', error: null });
        const result = await api.runProductionDebugFix({
          caseId,
          title: debugCase.title,
          report: debugCase.report,
          evidence: get().cases.find((item) => item.id === caseId)?.evidence ?? debugCase.evidence,
          worktree,
          verificationCommand: debugCase.verificationCommand,
          signal: controller.signal,
          onToolActivity: (activity) => setActivity(caseId, activity),
        });
        if (result.verificationEvidence) {
          const latest = get().cases.find((item) => item.id === caseId);
          if (latest) {
            update(caseId, {
              evidence: api.boundProductionEvidence([
                result.verificationEvidence,
                ...latest.evidence.filter((item) => item.id !== result.verificationEvidence?.id),
              ]),
            });
          }
        }
        const unresolvedRisks = debugCase.report.unresolvedRisks.filter(
          (risk) => risk !== NO_DIFF_RISK && !risk.startsWith(VERIFICATION_RISK_PREFIX),
        );
        if (!result.patch.diff) unresolvedRisks.push(NO_DIFF_RISK);
        if (result.verification.status === 'failed' || result.verification.status === 'inconclusive') {
          unresolvedRisks.push(`${VERIFICATION_RISK_PREFIX} ${result.verification.status}.`);
        }
        const report: ProductionDebugReport = {
          ...debugCase.report,
          proposedPatch: result.patch,
          verification: result.verification,
          unresolvedRisks: [...new Set(unresolvedRisks)],
          fixDurableRunId: result.durableRunId,
          verificationDurableRunId: result.verification.durableRunId,
        };
        if (result.outcome === 'completed') {
          update(caseId, { status: 'fix_prepared', report, fixSummary: result.summary, error: null });
        } else if (result.outcome === 'cancelled') {
          update(caseId, { status: 'cancelled', report, fixSummary: result.summary, error: null });
        } else {
          update(caseId, { status: 'failed', report, fixSummary: result.summary, error: result.summary });
        }
      } catch (error) {
        update(caseId, {
          status: controller.signal.aborted ? 'cancelled' : 'failed',
          error: controller.signal.aborted ? null : errorText(error),
        });
      } finally {
        controllers.delete(caseId);
        setActivity(caseId, null);
      }
    },

    cancel: (caseId) => controllers.get(caseId)?.abort(),

    deleteCase: (caseId) => {
      controllers.get(caseId)?.abort();
      controllers.delete(caseId);
      set((state) => {
        const cases = state.cases.filter((item) => item.id !== caseId);
        persist(cases);
        return {
          cases,
          selectedCaseId: state.selectedCaseId === caseId ? cases[0]?.id ?? null : state.selectedCaseId,
        };
      });
    },
  };
});

export default useProductionDebuggingStore;
